use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use wx_context::{AccountContext, ContactResolver, Direction, ResolveParams, VisibilityIndex};
use wx_monitor::SessionEventKind;

use super::contacts::build_visibility;
use crate::schema::{enrich_session_event, project_session_sender};
use crate::util::{open_db_core, print_cache_stats, print_detection_note};
use crate::OutputFormat;

#[derive(Serialize)]
struct WatchEvent {
    #[serde(flatten)]
    enriched: crate::schema::EnrichedSession,
    kind: SessionEventKind,
}

#[cfg(test)]
fn format_watch_line(
    ev: &wx_monitor::SessionEvent,
    resolver: &ContactResolver,
    self_wxid: &str,
) -> String {
    use crate::schema::derive_session_direction;
    let time = chrono::Local::now().format("%H:%M:%S");
    let is_group = wx_db::is_group_chat(&ev.username);

    let content = if !ev.summary.is_empty() {
        ev.summary.clone()
    } else if let Some(mt) = ev.last_msg_type {
        format!("[{}]", wx_db::msg_type_label(mt))
    } else {
        String::new()
    };

    let direction = derive_session_direction(ev.last_msg_sender.as_deref(), self_wxid);

    if is_group {
        let group_name = resolver.display_with_id(&ev.username);
        match direction {
            Some(Direction::Outgoing) => {
                format!("{time} 发送到群「{group_name}」：{content}")
            }
            Some(Direction::Incoming) => {
                let sender = ev.last_msg_sender.as_deref().unwrap_or("");
                if sender.is_empty() {
                    format!("{time} 来自群「{group_name}」：{content}")
                } else {
                    let sender_name = resolver.display_with_id(sender);
                    format!("{time} 来自群「{group_name}」的{sender_name}：{content}")
                }
            }
            None => format!("{time} 群消息更新「{group_name}」：{content}"),
        }
    } else {
        let name = resolver.display_with_id(&ev.username);
        match direction {
            Some(Direction::Outgoing) => format!("{time} 发送给{name}：{content}"),
            Some(Direction::Incoming) => format!("{time} 来自{name}：{content}"),
            None => format!("{time} 会话更新「{name}」：{content}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn make_event(
        username: &str,
        summary: &str,
        last_msg_sender: Option<&str>,
    ) -> wx_monitor::SessionEvent {
        wx_monitor::SessionEvent {
            username: username.to_string(),
            sort_timestamp: 1,
            detected_at: 2,
            kind: SessionEventKind::Updated,
            summary: summary.to_string(),
            last_msg_type: Some(1),
            last_msg_sender: last_msg_sender.map(str::to_string),
            last_sender_display_name: None,
        }
    }

    #[test]
    fn watch_text_uses_neutral_wording_when_private_direction_unknown() {
        let resolver = ContactResolver::empty();
        let line = format_watch_line(
            &make_event("wxid_friend", "hello", None),
            &resolver,
            "wxid_me",
        );

        assert!(line.contains("会话更新"));
        assert!(!line.contains("来自"));
        assert!(!line.contains("发送给"));
    }

    #[test]
    fn watch_text_keeps_explicit_wording_for_known_private_outgoing() {
        let resolver = ContactResolver::empty();
        let line = format_watch_line(
            &make_event("wxid_friend", "hello", Some("wxid_me")),
            &resolver,
            "wxid_me",
        );

        assert!(line.contains("发送给wxid_friend"));
    }

    #[test]
    fn watch_text_uses_neutral_group_wording_when_direction_unknown() {
        let resolver = ContactResolver::empty();
        let line = format_watch_line(
            &make_event("team@chatroom", "hello", Some("")),
            &resolver,
            "wxid_me",
        );

        assert!(line.contains("群消息更新"));
        assert!(!line.contains("来自群"));
        assert!(!line.contains("发送到群"));
    }

    #[test]
    fn watch_text_legacy_non_wxid_self_id_outgoing() {
        let resolver = ContactResolver::empty();
        let line = format_watch_line(
            &make_event("wxid_friend", "hello", Some("testuser001")),
            &resolver,
            "testuser001",
        );

        assert!(
            line.contains("发送给"),
            "expected outgoing wording, got: {line}"
        );
    }

    #[test]
    fn watch_text_legacy_non_wxid_self_id_incoming() {
        let resolver = ContactResolver::empty();
        let line = format_watch_line(
            &make_event("wxid_friend", "hello", Some("wxid_friend")),
            &resolver,
            "testuser001",
        );

        assert!(
            line.contains("来自"),
            "expected incoming wording, got: {line}"
        );
    }

    #[test]
    fn hidden_talker_is_dropped_by_default() {
        let visibility =
            VisibilityIndex::build(&["wxid_secret".to_string()], &[], &ContactResolver::empty());
        let event = make_event("wxid_secret", "hello", None);

        assert!(!should_emit_event(&visibility, false, &event));
    }

    #[test]
    fn show_hidden_restores_hidden_talker_output() {
        let visibility =
            VisibilityIndex::build(&["wxid_secret".to_string()], &[], &ContactResolver::empty());
        let event = make_event("wxid_secret", "hello", None);

        assert!(should_emit_event(&visibility, true, &event));
    }

    // --- Phase 2: sender-level tests ---

    fn make_enriched_session(
        username: &str,
        summary: &str,
        last_msg_sender: Option<&str>,
    ) -> crate::schema::EnrichedSession {
        use crate::schema::enrich_session_event;
        let ev = wx_monitor::SessionEvent {
            username: username.to_string(),
            sort_timestamp: 1,
            detected_at: 2,
            kind: wx_monitor::SessionEventKind::Updated,
            summary: summary.to_string(),
            last_msg_type: Some(1),
            last_msg_sender: last_msg_sender.map(str::to_string),
            last_sender_display_name: last_msg_sender.map(str::to_string),
        };
        enrich_session_event(ev, "wxid_me", &ContactResolver::empty())
    }

    #[test]
    fn watch_text_hidden_sender_shows_placeholder() {
        use crate::schema::project_session_sender;
        let visibility =
            VisibilityIndex::build(&["wxid_spam".to_string()], &[], &ContactResolver::empty());
        let mut enriched = make_enriched_session("group@chatroom", "spam msg", Some("wxid_spam"));
        project_session_sender(&mut enriched, &visibility);

        let line = format_watch_line_from_enriched(&enriched, &ContactResolver::empty(), "wxid_me");
        assert!(
            line.contains("[消息已隐藏]"),
            "should show placeholder: {line}"
        );
        assert!(
            !line.contains("wxid_spam"),
            "should not leak sender wxid: {line}"
        );
    }

    #[test]
    fn watch_text_visible_sender_shows_normal() {
        let enriched = make_enriched_session("group@chatroom", "hello", Some("wxid_normal"));
        let line = format_watch_line_from_enriched(&enriched, &ContactResolver::empty(), "wxid_me");
        assert!(line.contains("hello"), "should show normal summary: {line}");
    }
}

/// Format a watch line from an EnrichedSession (Phase 2: includes sender redaction).
fn format_watch_line_from_enriched(
    enriched: &crate::schema::EnrichedSession,
    resolver: &ContactResolver,
    _self_wxid: &str,
) -> String {
    let time = chrono::Local::now().format("%H:%M:%S");
    let session = &enriched.session;
    let is_group = wx_db::is_group_chat(&session.username);

    let content = if !session.summary.is_empty() {
        session.summary.clone()
    } else if let Some(mt) = session.last_msg_type {
        format!("[{}]", wx_db::msg_type_label(mt))
    } else {
        String::new()
    };

    let direction = &enriched.direction;

    if is_group {
        let group_name = resolver.display_with_id(&session.username);
        match direction {
            Some(Direction::Outgoing) => {
                format!("{time} 发送到群「{group_name}」：{content}")
            }
            Some(Direction::Incoming) => {
                let sender = session.last_msg_sender.as_deref().unwrap_or("");
                if sender.is_empty() {
                    format!("{time} 来自群「{group_name}」：{content}")
                } else {
                    let sender_name = resolver.display_with_id(sender);
                    format!("{time} 来自群「{group_name}」的{sender_name}：{content}")
                }
            }
            None => format!("{time} 群消息更新「{group_name}」：{content}"),
        }
    } else {
        let name = resolver.display_with_id(&session.username);
        match direction {
            Some(Direction::Outgoing) => format!("{time} 发送给{name}：{content}"),
            Some(Direction::Incoming) => format!("{time} 来自{name}：{content}"),
            None => format!("{time} 会话更新「{name}」：{content}"),
        }
    }
}

fn resolve_watch_mode(poll: bool, fsnotify: bool) -> wx_monitor::WatchMode {
    if poll {
        wx_monitor::WatchMode::Poll
    } else if fsnotify {
        wx_monitor::WatchMode::Fsnotify
    } else {
        wx_monitor::WatchMode::Auto
    }
}

fn should_emit_event(
    visibility: &VisibilityIndex,
    show_hidden: bool,
    event: &wx_monitor::SessionEvent,
) -> bool {
    show_hidden || !visibility.is_hidden_talker(&event.username)
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_watch(
    key_hex: Option<String>,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    poll: bool,
    fsnotify: bool,
    poll_ms: u64,
    format: OutputFormat,
    show_hidden: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let params = &wx_decrypt::MACOS_4_1_7_31;

    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key_hex.as_deref(),
    })?;
    print_detection_note(&acct);

    let (db, stats) = open_db_core(&acct, crate::util::decrypt_progress_callback)?;
    if let Some(ref s) = stats {
        print_cache_stats(s);
    }
    let resolver = ContactResolver::build(&db)?;
    let visibility = build_visibility(&acct, &resolver);
    let self_wxid = acct.base_wxid.clone();

    let encrypted_session_dir = acct.data_dir.join("db_storage").join("session");
    if !encrypted_session_dir.exists() {
        return Err(format!(
            "session directory not found: {}",
            encrypted_session_dir.display()
        )
        .into());
    }

    let watch_mode = resolve_watch_mode(poll, fsnotify);
    let monitor_derived_keys = wx_context::persisted_derived_keys(&acct)?;
    let config = wx_monitor::MonitorConfig {
        encrypted_session_dir,
        key_material: if monitor_derived_keys.is_empty() {
            acct.key_material.clone()
        } else {
            wx_decrypt::KeyMaterial::EncKeys(monitor_derived_keys)
        },
        params,
        watch_mode: watch_mode.clone(),
        poll_interval: Duration::from_millis(poll_ms),
        channel_capacity: 1000,
        raw_key: acct.raw_key,
        encrypted_root: if acct.raw_key.is_some() {
            Some(acct.data_dir.join("db_storage"))
        } else {
            None
        },
    };

    let resolved = wx_monitor::resolve_watch_mode(&watch_mode);
    eprintln!("Starting monitor (mode={watch_mode:?} -> {resolved:?}, interval={poll_ms}ms)...");
    let mut monitor = wx_monitor::WechatMonitor::start(config)?;
    eprintln!("Monitor started. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                monitor.stop();
                break;
            }
            event = monitor.recv() => {
                match event {
                    Some(ev) => {
                        if !should_emit_event(&visibility, show_hidden, &ev) {
                            continue;
                        }
                        let kind = ev.kind.clone();
                        let mut enriched = enrich_session_event(ev, &self_wxid, &resolver);
                        // Phase 2: redact hidden sender in group session
                        if !show_hidden {
                            project_session_sender(&mut enriched, &visibility);
                        }
                        match format {
                            OutputFormat::Json => {
                                let watch_event = WatchEvent { enriched, kind };
                                println!("{}", serde_json::to_string(&watch_event).unwrap());
                            }
                            OutputFormat::Text => {
                                println!(
                                    "{}",
                                    format_watch_line_from_enriched(
                                        &enriched,
                                        &resolver,
                                        &self_wxid
                                    )
                                );
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    eprintln!("Monitor stopped.");
    Ok(())
}
