use std::path::PathBuf;

use wx_context::{
    route_shards_for_query, write_shard_metadata_sidecar, AccountContext, ContactResolver,
    DecryptRequest, Direction, PersistentCache, ResolveParams, VisibilityIndex,
};

use super::contacts::build_visibility;
use super::thin_client::{ThinClient, ThinClientCliArgs, ThinClientOptions};
use crate::output::JsonEnvelope;
use crate::schema::{enrich_message, project_message_items, EnrichedMessage};
use crate::util::{open_db_all, print_cache_stats, print_detection_note};
use crate::{OutputFormat, SortOrderArg};

/// Resolve a contact identifier to a talker wxid.
///
/// Delegates to the shared `contact_id::resolve_contact_id` resolver and prints
/// resolution notes to stderr for CLI feedback.
pub(crate) fn resolve_talker(
    contact: &str,
    resolver: &ContactResolver,
    db: &wx_db::WechatDb,
    visibility: Option<&VisibilityIndex>,
    show_hidden: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    use crate::contact_id::{resolve_contact_id, ContactResolveError};

    match resolve_contact_id(contact, resolver, db, visibility, show_hidden) {
        Ok(resolved) => {
            if let Some(ref name) = resolved.display_name {
                eprintln!("Resolved \"{contact}\" → {name}（{}）", resolved.wxid);
            }
            Ok(resolved.wxid)
        }
        Err(
            ContactResolveError::NotFound(msg)
            | ContactResolveError::Ambiguous(msg)
            | ContactResolveError::Hidden(msg),
        ) => Err(msg.into()),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_query(
    contact: &str,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<String>,
    limit: usize,
    offset: usize,
    order: SortOrderArg,
    all: bool,
    format: OutputFormat,
    around_sort_seq: Option<i64>,
    around_server_id: Option<i64>,
    context: Option<usize>,
    after_sort_seq: Option<i64>,
    show_hidden: bool,
    server: ThinClientCliArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let has_around = around_sort_seq.is_some() || around_server_id.is_some();
    let has_anchor =
        around_sort_seq.is_some() || around_server_id.is_some() || after_sort_seq.is_some();
    let effective_limit = if has_anchor {
        limit
    } else if all {
        wx_db::MAX_QUERY_LIMIT
    } else {
        wx_db::effective_limit(limit)
    };

    let options = ThinClientOptions::resolve_from_process_env(server);
    let preserve_local_warning = context.is_some() && !has_around;

    if options.is_enabled() && !preserve_local_warning {
        let client = ThinClient::new(options.clone());
        match client.probe_health() {
            Ok(()) => {
                let envelope = fetch_remote_query(
                    &client,
                    contact,
                    since,
                    until,
                    msg_type.clone(),
                    effective_limit,
                    offset,
                    order.clone(),
                    around_sort_seq,
                    around_server_id,
                    context,
                    after_sort_seq,
                    show_hidden,
                )?;
                let is_group = envelope
                    .items
                    .first()
                    .map(|item| wx_db::is_group_chat(&item.message.talker))
                    .unwrap_or_else(|| wx_db::is_group_chat(contact));
                return print_query_output(&envelope, format, is_group);
            }
            Err(err) if err.should_fallback(options.mode) => {
                eprintln!(
                    "note: remote server unavailable, falling back to local query ({})",
                    err.fallback_detail()
                );
            }
            Err(err) => return Err(err.into()),
        }
    }

    let (envelope, is_group) = load_local_query(
        contact,
        data_dir,
        account,
        key,
        since,
        until,
        msg_type,
        limit,
        offset,
        order,
        all,
        around_sort_seq,
        around_server_id,
        context,
        after_sort_seq,
        show_hidden,
    )?;
    print_query_output(&envelope, format, is_group)
}

#[allow(clippy::too_many_arguments)]
fn load_local_query(
    contact: &str,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<String>,
    limit: usize,
    offset: usize,
    order: SortOrderArg,
    all: bool,
    around_sort_seq: Option<i64>,
    around_server_id: Option<i64>,
    context: Option<usize>,
    after_sort_seq: Option<i64>,
    show_hidden: bool,
) -> Result<(JsonEnvelope<EnrichedMessage>, bool), Box<dyn std::error::Error>> {
    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key.as_deref(),
    })?;
    print_detection_note(&acct);

    let (db, _cache, _stats) = if acct.raw_key.is_some() {
        // Direct encrypted open — no shard routing needed
        let (db, _, stats) = open_db_all(&acct, crate::util::decrypt_progress_callback)?;
        (db, None::<PersistentCache>, stats)
    } else {
        let params = &wx_decrypt::MACOS_4_1_7_31;
        let cache = PersistentCache::new(&acct, params)?;

        // Two-phase decrypt: try shard routing when time bounds are present.
        DecryptRequest::new()
            .core()
            .execute_with_progress(&cache, crate::util::decrypt_progress_callback)?;

        let shard_ids = route_shards_for_query(cache.decrypted_root(), contact, since, until);

        let stats = match shard_ids {
            Some(ref ids) => {
                eprintln!(
                    "shard routing: decrypting {} of N shards for time-bounded query",
                    ids.len()
                );
                DecryptRequest::new()
                    .shards(ids)
                    .execute_with_progress(&cache, crate::util::decrypt_progress_callback)?
            }
            None => DecryptRequest::new()
                .all()
                .execute_with_progress(&cache, crate::util::decrypt_progress_callback)?,
        };
        print_cache_stats(&stats);

        let db = wx_db::WechatDb::open(cache.decrypted_root())?;

        if let Err(e) = write_shard_metadata_sidecar(&db, cache.decrypted_root()) {
            eprintln!("warning: failed to write shard metadata sidecar: {e}");
        }

        (db, Some(cache), Some(stats))
    };

    let resolver = ContactResolver::build(&db)?;
    let visibility = build_visibility(&acct, &resolver);
    let self_wxid = &acct.base_wxid;

    let talker = resolve_talker(contact, &resolver, &db, Some(&visibility), show_hidden)?;

    // Determine if we're using anchor mode
    let has_anchor =
        around_sort_seq.is_some() || around_server_id.is_some() || after_sort_seq.is_some();

    // Validate: offset must be 0 in anchor mode
    if has_anchor && offset != 0 {
        return Err("--offset is not supported with anchor queries (--around-sort-seq, --around-server-id, --after-sort-seq)".into());
    }

    // Warn if --context is used without --around-*
    let has_around = around_sort_seq.is_some() || around_server_id.is_some();
    if context.is_some() && !has_around {
        eprintln!("warning: --context is only meaningful with --around-sort-seq or --around-server-id, ignoring");
    }

    let result = if has_anchor {
        let mut query = wx_db::MessageQuery::for_talker(&talker);

        if let Some(seq) = around_sort_seq {
            query = query.around_sort_seq(seq);
        } else if let Some(id) = around_server_id {
            query = query.around_server_id(id);
        } else if let Some(seq) = after_sort_seq {
            query = query.after_sort_seq(seq).limit(limit);
        }

        if has_around {
            if let Some(ctx) = context {
                query = query.context(ctx);
            }
        }

        if let Some(ref mt_str) = msg_type {
            if let Some(mt) = wx_db::parse_msg_type(mt_str) {
                query = query.msg_type(mt);
            }
        }

        db.query_messages_anchor(&query)?
    } else {
        let effective_limit = if all {
            wx_db::MAX_QUERY_LIMIT
        } else {
            wx_db::effective_limit(limit)
        };

        let mut query = wx_db::MessageQuery::for_talker(&talker)
            .limit(effective_limit)
            .offset(offset)
            .order(order.into());

        if let Some(s) = since {
            query = query.since(s);
        }
        if let Some(u) = until {
            query = query.until(u);
        }

        if let Some(ref mt_str) = msg_type {
            if let Some(mt) = wx_db::parse_msg_type(mt_str) {
                query = query.msg_type(mt);
            }
        }

        db.query_messages(&query)?
    };

    for w in &result.shard_warnings {
        eprintln!("warning: shard {}: {}", w.path, w.reason);
    }
    if result.stats.skipped > 0 {
        eprintln!(
            "warning: {} messages skipped (decode error)",
            result.stats.skipped
        );
    }

    let is_group = wx_db::is_group_chat(&talker);
    let effective_limit = if has_anchor {
        result.items.len()
    } else if all {
        wx_db::MAX_QUERY_LIMIT
    } else {
        wx_db::effective_limit(limit)
    };

    let mut envelope =
        JsonEnvelope::from_message_query_result(result, effective_limit, offset, |m| {
            enrich_message(m, self_wxid, &resolver)
        });

    // When limit pushdown was used (non-anchor, non-all), total_rows only reflects the
    // scanned window. Use a lightweight COUNT(*) query to get the actual DB-level total.
    if !has_anchor && !all {
        let mt_filter = msg_type.as_ref().and_then(|s| wx_db::parse_msg_type(s));
        let db_total = db.count_messages(
            &talker,
            since.unwrap_or(0),
            until.unwrap_or(i64::MAX),
            mt_filter,
        );
        envelope.paging.total = db_total;
        envelope.paging.has_more = offset + envelope.paging.returned < db_total;
    }

    // Phase 2: sender-level projection
    let projected = project_message_items(envelope.items, &talker, &visibility, show_hidden);
    envelope.paging.returned = projected.len();
    envelope.items = projected;

    Ok((envelope, is_group))
}

#[allow(clippy::too_many_arguments)]
fn fetch_remote_query(
    client: &ThinClient,
    contact: &str,
    since: Option<i64>,
    until: Option<i64>,
    msg_type: Option<String>,
    limit: usize,
    offset: usize,
    order: SortOrderArg,
    around_sort_seq: Option<i64>,
    around_server_id: Option<i64>,
    context: Option<usize>,
    after_sort_seq: Option<i64>,
    show_hidden: bool,
) -> Result<JsonEnvelope<EnrichedMessage>, super::thin_client::ThinClientError> {
    let mut query = vec![
        ("contact".to_string(), contact.to_string()),
        ("limit".to_string(), limit.to_string()),
        ("offset".to_string(), offset.to_string()),
        (
            "order".to_string(),
            match order {
                SortOrderArg::Asc => "asc".to_string(),
                SortOrderArg::Desc => "desc".to_string(),
            },
        ),
    ];
    if let Some(since) = since {
        query.push(("since".to_string(), since.to_string()));
    }
    if let Some(until) = until {
        query.push(("until".to_string(), until.to_string()));
    }
    if let Some(msg_type) = msg_type {
        query.push(("type".to_string(), msg_type));
    }
    if let Some(seq) = around_sort_seq {
        query.push(("around_sort_seq".to_string(), seq.to_string()));
    }
    if let Some(id) = around_server_id {
        query.push(("around_server_id".to_string(), id.to_string()));
    }
    if let Some(context) = context {
        query.push(("context".to_string(), context.to_string()));
    }
    if let Some(seq) = after_sort_seq {
        query.push(("after_sort_seq".to_string(), seq.to_string()));
    }
    if show_hidden {
        query.push(("show_hidden".to_string(), "1".to_string()));
    }
    client.get_json("/api/v1/messages", &query)
}

fn print_query_output(
    envelope: &JsonEnvelope<EnrichedMessage>,
    format: OutputFormat,
    is_group: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(envelope)?),
        OutputFormat::Text => render_query_text(&envelope.items, is_group),
    }
    Ok(())
}

fn render_query_text(items: &[EnrichedMessage], is_group: bool) {
    for item in items {
        let ts = chrono::DateTime::from_timestamp(item.message.create_time, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();

        if is_group {
            let sender_name = if item.direction == Direction::Outgoing {
                "我"
            } else {
                display_name_only(&item.sender_display_name, &item.message.sender)
            };
            println!("{ts} [{sender_name}] {}", item.snippet);
        } else {
            let arrow = match item.direction {
                Direction::Incoming => "<<",
                Direction::Outgoing => ">>",
            };
            println!("{ts} {arrow} {}", item.snippet);
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
