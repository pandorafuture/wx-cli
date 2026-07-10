use std::path::PathBuf;

use wx_context::{register_mm_fts_tokenizer, AccountContext, ContactResolver, ResolveParams};

use super::thin_client::{ThinClient, ThinClientCliArgs, ThinClientOptions};
use crate::output::{JsonEnvelope, PagingMeta, StatsMeta};
use crate::schema::{enrich_message_as_hit, enrich_native_fts_hit, SearchHit};
use crate::util::{
    effective_limit_all, open_db_all, print_cache_stats, print_detection_note, try_remote_or_local,
};
use crate::OutputFormat;

// Unused imports kept for Task 6 cleanup reference:
// use crate::util::print_fts_stats;
// use wx_db::{FtsSearchResult};

#[allow(clippy::too_many_arguments)]
pub fn cmd_search(
    keyword: &str,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    limit: usize,
    offset: usize,
    all: bool,
    format: OutputFormat,
    server: ThinClientCliArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let options = ThinClientOptions::resolve_from_process_env(server);
    let effective_limit = effective_limit_all(all, limit);

    let envelope = try_remote_or_local(
        &options,
        |client| fetch_remote_search(client, keyword, effective_limit, offset),
        || load_local_search(keyword, data_dir, account, key, effective_limit, offset),
        "search",
    )?;
    print_search_output(&envelope, format)
}

fn load_local_search(
    keyword: &str,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    effective_limit: usize,
    offset: usize,
) -> Result<JsonEnvelope<SearchHit>, Box<dyn std::error::Error>> {
    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key.as_deref(),
    })?;
    print_detection_note(&acct);

    let (db, _cache, stats) = open_db_all(&acct, crate::util::decrypt_progress_callback)?;
    if let Some(ref s) = stats {
        print_cache_stats(s);
    }
    let resolver = ContactResolver::build(&db)?;
    let self_wxid = &acct.base_wxid;

    let search_start = std::time::Instant::now();

    // --- Native FTS search → fallback to scan ---
    let use_fallback = match db.message_fts_path.as_deref() {
        Some(fts_path) => match db.open_related_readonly(fts_path).and_then(|conn| {
            register_mm_fts_tokenizer(&conn).map_err(wx_db::DbError::FtsInit)?;
            Ok(conn)
        }) {
            Ok(conn) => {
                match wx_db::native_fts::search_message_fts(&conn, keyword, effective_limit, offset)
                {
                    Ok(result) => {
                        return native_fts_envelope(
                            result,
                            &resolver,
                            self_wxid,
                            effective_limit,
                            offset,
                            &search_start,
                        );
                    }
                    Err(e) => {
                        eprintln!("Native FTS query failed, falling back to scan: {e}");
                        true
                    }
                }
            }
            Err(e) => {
                eprintln!("Cannot open FTS DB, falling back to scan: {e}");
                true
            }
        },
        None => {
            // No native FTS DB available — silent fallback
            true
        }
    };

    debug_assert!(use_fallback);
    scan_envelope(
        keyword,
        &db,
        &resolver,
        self_wxid,
        effective_limit,
        offset,
        &search_start,
    )
}

fn fetch_remote_search(
    client: &ThinClient,
    keyword: &str,
    limit: usize,
    offset: usize,
) -> Result<JsonEnvelope<SearchHit>, super::thin_client::ThinClientError> {
    let query = vec![
        ("q".to_string(), keyword.to_string()),
        ("limit".to_string(), limit.to_string()),
        ("offset".to_string(), offset.to_string()),
    ];
    client.get_json("/api/v1/search", &query)
}

fn print_search_output(
    envelope: &JsonEnvelope<SearchHit>,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(envelope)?),
        OutputFormat::Text => render_search_text(&envelope.items),
    }
    Ok(())
}

fn render_search_text(items: &[SearchHit]) {
    for hit in items {
        let ts = chrono::DateTime::from_timestamp(hit.create_time, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();

        if wx_db::is_group_chat(&hit.talker) {
            println!(
                "{ts}  [群「{}」] {}: {}",
                display_name_only(&hit.talker_display_name, &hit.talker),
                display_name_only(&hit.sender_display_name, &hit.sender),
                hit.snippet
            );
        } else {
            println!(
                "{ts}  [{}] {}",
                display_name_only(&hit.talker_display_name, &hit.talker),
                hit.snippet
            );
        }
    }
}

fn display_name_only<'a>(display: &'a str, wxid: &str) -> &'a str {
    let suffix = format!("（{wxid}）");
    display
        .strip_suffix(&suffix)
        .filter(|name| !name.is_empty())
        .unwrap_or(display)
}

/// Native FTS path: return the existing JSON envelope contract.
fn native_fts_envelope(
    result: wx_db::NativeFtsResult,
    resolver: &ContactResolver,
    self_wxid: &str,
    limit: usize,
    offset: usize,
    search_start: &std::time::Instant,
) -> Result<JsonEnvelope<SearchHit>, Box<dyn std::error::Error>> {
    let total_hits = result.total_hits;
    let returned = result.hits.len();

    let enriched: Vec<SearchHit> = result
        .hits
        .into_iter()
        .map(|hit| enrich_native_fts_hit(hit, self_wxid, resolver))
        .collect();
    Ok(JsonEnvelope {
        items: enriched,
        paging: PagingMeta {
            limit,
            offset,
            returned,
            has_more: offset + returned < total_hits,
            total: total_hits,
        },
        stats: StatsMeta {
            scanned: 0,
            skipped: 0,
            elapsed_ms: Some(search_start.elapsed().as_millis() as u64),
            shard_warnings: Vec::new(),
        },
    })
}

/// Existing scan path: iterate all sessions, query_messages per session, collect + sort + paginate.
#[allow(clippy::too_many_arguments)]
fn scan_envelope(
    keyword: &str,
    db: &wx_db::WechatDb,
    resolver: &ContactResolver,
    self_wxid: &str,
    effective_limit: usize,
    offset: usize,
    search_start: &std::time::Instant,
) -> Result<JsonEnvelope<SearchHit>, Box<dyn std::error::Error>> {
    // Paginate through ALL sessions so we never miss conversations.
    let mut all_sessions = Vec::new();
    let page_size = wx_db::MAX_QUERY_LIMIT;
    let mut sess_offset = 0;
    loop {
        let page = db.query_sessions(
            &wx_db::SessionQuery::new()
                .limit(page_size)
                .offset(sess_offset),
        )?;
        if page.items.is_empty() {
            break;
        }
        sess_offset += page.items.len();
        all_sessions.extend(page.items);
        if sess_offset >= page.stats.total_rows {
            break;
        }
    }

    let mut all_hits: Vec<(wx_db::Message, String)> = Vec::new();
    let mut total_scanned: usize = 0;
    let mut total_skipped: usize = 0;
    let mut all_shard_warnings: Vec<wx_db::ShardWarning> = Vec::new();

    for session in &all_sessions {
        // BUG FIX: per-session query uses MAX_QUERY_LIMIT (not user limit)
        let query = wx_db::MessageQuery::for_talker(&session.username)
            .keyword(keyword)
            .limit(wx_db::MAX_QUERY_LIMIT);

        let result = db.query_messages(&query)?;
        total_scanned += result.stats.total_rows;
        total_skipped += result.stats.skipped;
        all_shard_warnings.extend(result.shard_warnings);
        for msg in result.items {
            all_hits.push((msg, session.username.clone()));
        }
    }

    for w in &all_shard_warnings {
        eprintln!("warning: shard {}: {}", w.path, w.reason);
    }
    if total_skipped > 0 {
        eprintln!("warning: {} messages skipped (decode error)", total_skipped);
    }

    // Sort by (sort_seq DESC, create_time DESC, server_id DESC) to align with
    // query_messages() stable ordering. server_id provides a unique tie-breaker.
    all_hits.sort_by(|a, b| {
        b.0.sort_seq
            .cmp(&a.0.sort_seq)
            .then_with(|| b.0.create_time.cmp(&a.0.create_time))
            .then_with(|| b.0.server_id.cmp(&a.0.server_id))
    });

    let total_hits = all_hits.len();
    // Apply offset + limit
    let page: Vec<_> = all_hits
        .into_iter()
        .skip(offset)
        .take(effective_limit)
        .collect();

    let enriched: Vec<_> = page
        .into_iter()
        .map(|(m, talker)| enrich_message_as_hit(m, talker, self_wxid, resolver))
        .collect();
    let returned = enriched.len();
    Ok(JsonEnvelope {
        items: enriched,
        paging: PagingMeta {
            limit: effective_limit,
            offset,
            returned,
            has_more: offset + returned < total_hits,
            total: total_hits,
        },
        stats: StatsMeta {
            scanned: total_scanned,
            skipped: total_skipped,
            elapsed_ms: Some(search_start.elapsed().as_millis() as u64),
            shard_warnings: all_shard_warnings,
        },
    })
}
