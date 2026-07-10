use std::path::PathBuf;

use wx_context::{AccountContext, ContactResolver, ResolveParams, VisibilityIndex};
use wx_db::Contact;

use super::thin_client::{ThinClientCliArgs, ThinClientOptions};
use crate::output::JsonEnvelope;
use crate::settings::Settings;
use crate::util::{
    effective_limit_all, open_db_core, print_cache_stats, print_detection_note, try_remote_or_local,
};
use crate::visibility_projection::project_contacts_envelope;
use crate::OutputFormat;

#[allow(clippy::too_many_arguments)]
pub fn cmd_contacts(
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    search: Option<String>,
    limit: usize,
    offset: usize,
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
            ];
            if let Some(ref search) = search {
                query.push(("search".to_string(), search.to_string()));
            }
            if show_hidden {
                query.push(("show_hidden".to_string(), "1".to_string()));
            }
            client.get_json("/api/v1/contacts", &query)
        },
        || {
            load_local_contacts(
                data_dir,
                account,
                key,
                search.clone(),
                effective_limit,
                offset,
                show_hidden,
            )
        },
        "contacts",
    )?;
    print_contacts_output(&envelope, format)
}

pub(crate) fn build_visibility(
    acct: &AccountContext,
    resolver: &ContactResolver,
) -> VisibilityIndex {
    let settings = Settings::load_default().unwrap_or_default();
    let account_settings = settings.for_account(&acct.account_id);
    VisibilityIndex::build(
        &account_settings.ignore_contacts,
        &account_settings.ignore_tags,
        resolver,
    )
}

fn load_local_contacts(
    data_dir: Option<PathBuf>,
    account: Option<String>,
    key: Option<String>,
    search: Option<String>,
    effective_limit: usize,
    offset: usize,
    show_hidden: bool,
) -> Result<JsonEnvelope<Contact>, Box<dyn std::error::Error>> {
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
    let visibility = build_visibility(&acct, &resolver);

    let mut query = wx_db::ContactQuery::new()
        .limit(wx_db::MAX_QUERY_LIMIT)
        .offset(0);
    if let Some(ref kw) = search {
        query = query.keyword(kw);
    }

    let result = db.query_contacts(&query)?;
    let envelope = JsonEnvelope::from_query_result(result, wx_db::MAX_QUERY_LIMIT, 0, |c| c);
    Ok(project_contacts_envelope(
        envelope.items,
        &visibility,
        effective_limit,
        offset,
        &envelope.stats,
        show_hidden,
    ))
}

fn print_contacts_output(
    envelope: &JsonEnvelope<Contact>,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(envelope)?),
        OutputFormat::Text => {
            render_contacts_text(&envelope.items);
            eprintln!("{} contacts found.", envelope.items.len());
        }
    }
    Ok(())
}

fn render_contacts_text(items: &[Contact]) {
    for c in items {
        let alias_part = if c.alias.is_empty() {
            String::new()
        } else {
            format!(" / {}", c.alias)
        };

        let display = if !c.remark.is_empty() {
            format!("{}{}", c.remark, alias_part)
        } else if !c.nick_name.is_empty() {
            format!("{}{}", c.nick_name, alias_part)
        } else {
            alias_part.trim_start_matches(" / ").to_string()
        };

        if display.is_empty() {
            println!("  ({})", c.user_name);
        } else {
            println!("  {:<30} ({})", display, c.user_name);
        }

        print_contact_tree(c);
    }
}

fn gender_label(g: u32) -> &'static str {
    match g {
        1 => "male",
        2 => "female",
        _ => "unknown",
    }
}

fn source_scene_label(s: u32) -> String {
    match s {
        1 => "通过QQ号添加".to_string(),
        3 => "通过微信号添加".to_string(),
        6 => "通过手机号添加".to_string(),
        10 => "通过名片添加".to_string(),
        14 => "通过群聊添加".to_string(),
        30 => "通过扫一扫添加".to_string(),
        _ => format!("场景码 {s}"),
    }
}

fn print_contact_tree(c: &Contact) {
    let mut lines: Vec<(String, String)> = Vec::new();

    if let Some(ref phone) = c.phone {
        lines.push(("Phone".to_string(), phone.clone()));
    }
    if let Some(ref sig) = c.signature {
        lines.push(("Signature".to_string(), sig.clone()));
    }
    if let Some(ref region) = c.region {
        lines.push(("Region".to_string(), region.clone()));
    }
    if let Some(g) = c.gender {
        lines.push(("Gender".to_string(), gender_label(g).to_string()));
    }
    if let Some(s) = c.source_scene {
        lines.push(("Source".to_string(), source_scene_label(s)));
    }
    if let Some(ref memo) = c.memo {
        lines.push(("Memo".to_string(), memo.clone()));
    }
    if !c.labels.is_empty() {
        lines.push(("Labels".to_string(), c.labels.join(", ")));
    }

    if lines.is_empty() {
        return;
    }

    let last_idx = lines.len() - 1;
    for (i, (key, val)) in lines.iter().enumerate() {
        let prefix = if i == last_idx { "└─" } else { "├─" };
        println!("  {prefix} {key}: {val}");
    }
}
