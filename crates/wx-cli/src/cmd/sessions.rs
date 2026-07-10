use std::path::PathBuf;

use wx_context::{AccountContext, ContactResolver, ResolveParams};

use super::contacts::build_visibility;
use super::thin_client::{ThinClientCliArgs, ThinClientOptions};
use crate::output::JsonEnvelope;
use crate::schema::{enrich_session, EnrichedSession};
use crate::util::{
    effective_limit_all, open_db_core, print_cache_stats, print_detection_note, try_remote_or_local,
};
use crate::visibility_projection::project_sessions_envelope_enriched;
use crate::{OutputFormat, SortOrderArg};

#[allow(clippy::too_many_arguments)]
pub fn cmd_sessions(
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    limit: usize,
    offset: usize,
    order: SortOrderArg,
    all: bool,
    format: OutputFormat,
    show_hidden: bool,
    server: ThinClientCliArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let options = ThinClientOptions::resolve_from_process_env(server);
    let effective_limit = effective_limit_all(all, limit);

    let envelope = try_remote_or_local(
        &options,
        |client| {
            let mut query = vec![
                ("limit".to_string(), effective_limit.to_string()),
                ("offset".to_string(), offset.to_string()),
                (
                    "order".to_string(),
                    match order {
                        SortOrderArg::Asc => "asc".to_string(),
                        SortOrderArg::Desc => "desc".to_string(),
                    },
                ),
            ];
            if show_hidden {
                query.push(("show_hidden".to_string(), "1".to_string()));
            }
            client.get_json("/api/v1/sessions", &query)
        },
        || {
            load_local_sessions(
                data_dir,
                account,
                key,
                effective_limit,
                offset,
                order.clone(),
                show_hidden,
            )
        },
        "sessions",
    )?;
    print_sessions_output(&envelope, format)
}

fn load_local_sessions(
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    effective_limit: usize,
    offset: usize,
    order: SortOrderArg,
    show_hidden: bool,
) -> Result<JsonEnvelope<EnrichedSession>, Box<dyn std::error::Error>> {
    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key.as_deref(),
    })?;
    print_detection_note(&acct);

    let (db, stats) = open_db_core(&acct, crate::util::decrypt_progress_callback)?;
    if let Some(ref s) = stats {
        print_cache_stats(s);
    }
    let resolver = ContactResolver::build(&db)?;
    let self_wxid = &acct.base_wxid;
    let visibility = build_visibility(&acct, &resolver);

    let result = db.query_sessions(
        &wx_db::SessionQuery::new()
            .limit(wx_db::MAX_QUERY_LIMIT)
            .offset(0)
            .order(order.into()),
    )?;

    let envelope = JsonEnvelope::from_query_result(result, wx_db::MAX_QUERY_LIMIT, 0, |s| {
        enrich_session(s, self_wxid, &resolver, None)
    });

    Ok(project_sessions_envelope_enriched(
        envelope.items,
        &visibility,
        effective_limit,
        offset,
        &envelope.stats,
        show_hidden,
    ))
}

fn print_sessions_output(
    envelope: &JsonEnvelope<EnrichedSession>,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(envelope)?),
        OutputFormat::Text => render_sessions_text(&envelope.items),
    }
    Ok(())
}

fn render_sessions_text(items: &[EnrichedSession]) {
    for s in items {
        let ts = chrono::DateTime::from_timestamp(s.session.sort_timestamp, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();

        let is_group = wx_db::is_group_chat(&s.session.username);
        let summary = if is_group {
            if let Some(ref sender) = s.session.last_sender_display_name {
                format!("{sender}: {}", s.session.summary)
            } else {
                s.session.summary.clone()
            }
        } else {
            s.session.summary.clone()
        };
        let summary: String = summary.chars().take(60).collect();
        println!("{ts}  {}  {summary}", s.display_name);
    }
}
