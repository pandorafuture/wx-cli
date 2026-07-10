use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use wx_context::ContactResolver;
use wx_monitor::SessionEvent;

use crate::schema::{
    enrich_message, enrich_session_event, project_message_items, project_session_sender,
    EnrichedMessage,
};

use super::event::{MessagePayload, SessionPayload, SseEvent};
use super::refresh::RefreshTrigger;
use super::state::AppState;

struct BridgeState {
    cursors: HashMap<String, i64>,
    /// Global fallback baseline for talkers not seen at startup.
    /// Set to max(sort_seq) across all talkers at serve startup time.
    startup_watermark: i64,
}

fn should_broadcast_talker(visibility: &wx_context::VisibilityIndex, talker: &str) -> bool {
    !visibility.is_hidden_talker(talker)
}

/// Initialize per-talker baselines and a global startup_watermark.
///
/// Must be called **before** `WechatMonitor::start()` to avoid a race where
/// a message arriving between monitor start and baseline completion gets
/// absorbed into the baseline and never pushed to SSE clients.
pub fn init_baselines(db: &wx_db::WechatDb) -> Result<(HashMap<String, i64>, i64), String> {
    let sessions = db
        .query_sessions(&wx_db::SessionQuery::new().limit(10000))
        .map_err(|e| format!("query_sessions failed: {e}"))?;

    let usernames: Vec<String> = sessions.items.iter().map(|s| s.username.clone()).collect();
    let cursors = db.bulk_max_sort_seq(&usernames);
    let global_max = cursors.values().copied().max().unwrap_or(0);

    eprintln!(
        "bridge: initialized baselines for {} talkers, startup_watermark={}",
        cursors.len(),
        global_max
    );
    Ok((cursors, global_max))
}

/// Result from the blocking DB query, returned to the async context for
/// state updates and event broadcasting.
struct BridgeUpdate {
    /// Effective cursor value (always `Some` in the unified path; `None` not produced).
    last_sort_seq: Option<i64>,
    /// Enriched messages to broadcast (empty when no new messages or on failure).
    messages: Vec<EnrichedMessage>,
}

pub async fn run_bridge(
    mut receiver: mpsc::Receiver<SessionEvent>,
    state: Arc<AppState>,
    cursors: HashMap<String, i64>,
    startup_watermark: i64,
    refresh_tx: mpsc::Sender<RefreshTrigger>,
    mut refresh_watch: watch::Receiver<u64>,
    shutdown: CancellationToken,
) {
    let mut bridge_state = BridgeState {
        cursors,
        startup_watermark,
    };
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Some(ev) => handle_session_event(
                        ev,
                        &state,
                        &mut bridge_state,
                        &refresh_tx,
                        &mut refresh_watch,
                        &shutdown,
                    ).await,
                    None => break,
                }
            }
            _ = heartbeat.tick() => {
                let _ = state.broadcast_tx.send(Arc::new(SseEvent::Heartbeat));
            }
            _ = shutdown.cancelled() => break,
        }
    }
}

async fn handle_session_event(
    ev: SessionEvent,
    state: &AppState,
    bridge_state: &mut BridgeState,
    refresh_tx: &mpsc::Sender<RefreshTrigger>,
    refresh_watch: &mut watch::Receiver<u64>,
    shutdown: &CancellationToken,
) {
    if !should_broadcast_talker(&state.visibility, &ev.username) {
        return;
    }

    // Read cursor snapshot BEFORE triggering refresh (async context only).
    let baseline: i64 = bridge_state
        .cursors
        .get(&ev.username)
        .copied()
        .unwrap_or(bridge_state.startup_watermark);

    // Snapshot current epoch, then trigger refresh
    let epoch_before = *refresh_watch.borrow();
    if refresh_tx.send(RefreshTrigger::Refresh).await.is_err() {
        eprintln!("warn: bridge refresh_tx send failed (channel closed)");
        return;
    }

    // Wait for refresh to complete (epoch advances past our snapshot).
    // Timeout after 30s to avoid blocking forever if refresh keeps failing.
    // Also abort if shutdown fires during the wait.
    let wait_result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if *refresh_watch.borrow() > epoch_before {
                return true;
            }
            tokio::select! {
                result = refresh_watch.changed() => {
                    if result.is_err() {
                        return false;
                    }
                }
                _ = shutdown.cancelled() => return false,
            }
        }
    })
    .await;

    match wait_result {
        Ok(true) => {} // refresh succeeded
        Ok(false) => {
            if shutdown.is_cancelled() {
                return;
            }
            eprintln!("warn: bridge refresh_watch closed");
            return;
        }
        Err(_) => {
            eprintln!("warn: bridge refresh wait timed out (30s), skipping event");
            return;
        }
    }

    // DB query in blocking context; returns None on DB lock failure
    let db = Arc::clone(&state.db);
    let resolver = Arc::clone(&state.resolver);
    let visibility = Arc::clone(&state.visibility);
    let self_wxid = state.self_wxid.clone();
    let username = ev.username.clone();

    let update = tokio::task::spawn_blocking(move || {
        let guard = match db.lock() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("warn: bridge db lock failed: {e}");
                return None;
            }
        };

        // Unified path: always query after baseline (works for both first and subsequent events)
        let query = wx_db::MessageQuery::for_talker(&username).after_sort_seq(baseline);
        match guard.query_messages_anchor(&query) {
            Ok(result) => {
                for w in &result.shard_warnings {
                    eprintln!("warn: bridge shard {}: {}", w.path, w.reason);
                }
                let max_seq = result.items.iter().map(|m| m.sort_seq).max();
                let effective_seq = max_seq.unwrap_or(baseline);
                let messages =
                    enrich_messages(result.items, &self_wxid, &resolver, &username, &visibility);
                Some(BridgeUpdate {
                    last_sort_seq: Some(effective_seq),
                    messages,
                })
            }
            Err(e) => {
                eprintln!("warn: bridge query_messages_anchor failed: {e}");
                Some(BridgeUpdate {
                    last_sort_seq: Some(baseline),
                    messages: vec![],
                })
            }
        }
    })
    .await;

    // Async context: update per-talker cursors and broadcast events
    let username = ev.username.clone();

    let (last_sort_seq, messages) = apply_update(&mut bridge_state.cursors, &username, update);

    // Session event (always sent)
    let mut enriched = enrich_session_event(ev, &state.self_wxid, &state.resolver);
    // Phase 2: redact hidden sender in session summary
    project_session_sender(&mut enriched, &state.visibility);
    let session_payload = SessionPayload {
        enriched,
        last_sort_seq,
    };
    let _ = state
        .broadcast_tx
        .send(Arc::new(SseEvent::Session(session_payload)));

    // Message event (when there are new messages after baseline)
    if !messages.is_empty() {
        let payload = MessagePayload {
            talker: username.clone(),
            talker_display_name: state.resolver.display_with_id(&username),
            messages,
            anchor_sort_seq: last_sort_seq,
        };
        let _ = state
            .broadcast_tx
            .send(Arc::new(SseEvent::Message(payload)));
    }
}

fn enrich_messages(
    items: Vec<wx_db::Message>,
    self_wxid: &str,
    resolver: &ContactResolver,
    talker: &str,
    visibility: &wx_context::VisibilityIndex,
) -> Vec<EnrichedMessage> {
    let enriched: Vec<EnrichedMessage> = items
        .into_iter()
        .map(|m| enrich_message(m, self_wxid, resolver))
        .collect();
    // Phase 2: sender-level projection (SSE has no bypass)
    project_message_items(enriched, talker, visibility, false)
}

/// Apply a BridgeUpdate to the cursor map and return (last_sort_seq, messages).
///
/// Cursor update rules (unified path — no first/subsequent distinction):
/// - Query ok + new messages: cursor advanced to max(sort_seq) of new messages
/// - Query ok + no new messages: cursor set to baseline (unchanged)
/// - Query failed: cursor set to baseline (unchanged)
/// - DB lock failed / spawn panic: cursor unchanged, fallback to existing value
fn apply_update(
    cursors: &mut HashMap<String, i64>,
    username: &str,
    update: Result<Option<BridgeUpdate>, tokio::task::JoinError>,
) -> (Option<i64>, Vec<EnrichedMessage>) {
    match update {
        Ok(Some(u)) => {
            if let Some(seq) = u.last_sort_seq {
                cursors.insert(username.to_string(), seq);
            }
            (u.last_sort_seq, u.messages)
        }
        _ => {
            // spawn_blocking panicked or DB lock failed - use existing cursor if any
            let fallback_seq = cursors.get(username).copied();
            (fallback_seq, vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_context::{ContactResolver, VisibilityIndex};

    fn make_msg(sort_seq: i64) -> EnrichedMessage {
        EnrichedMessage {
            message: wx_db::Message {
                sort_seq,
                server_id: 0,
                msg_type: 1,
                sub_type: 0,
                sender: String::new(),
                talker: String::new(),
                create_time: 0,
                content: wx_db::MessageContent::Text(String::new()),
                status: 0,
            },
            sender_display_name: String::new(),
            direction: wx_context::Direction::Incoming,
            snippet: String::new(),
        }
    }

    #[test]
    fn first_event_initializes_cursor_with_messages() {
        let mut cursors = HashMap::new();
        let update = Ok(Some(BridgeUpdate {
            last_sort_seq: Some(600),
            messages: vec![make_msg(600)],
        }));
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(600));
        assert_eq!(messages.len(), 1);
        assert_eq!(cursors.get("wxid_alice"), Some(&600));
    }

    #[test]
    fn first_event_no_new_messages_cursor_at_baseline() {
        let mut cursors = HashMap::new();
        let update = Ok(Some(BridgeUpdate {
            last_sort_seq: Some(500),
            messages: vec![],
        }));
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(500));
        assert!(messages.is_empty());
        assert_eq!(cursors.get("wxid_alice"), Some(&500));
    }

    #[test]
    fn subsequent_event_with_new_messages_advances_cursor() {
        let mut cursors = HashMap::new();
        cursors.insert("wxid_alice".to_string(), 500);
        let update = Ok(Some(BridgeUpdate {
            last_sort_seq: Some(800),
            messages: vec![make_msg(600), make_msg(800)],
        }));
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(800));
        assert_eq!(messages.len(), 2);
        assert_eq!(cursors.get("wxid_alice"), Some(&800));
    }

    #[test]
    fn subsequent_event_no_new_messages_keeps_cursor() {
        let mut cursors = HashMap::new();
        cursors.insert("wxid_alice".to_string(), 500);
        let update = Ok(Some(BridgeUpdate {
            last_sort_seq: Some(500),
            messages: vec![],
        }));
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(500));
        assert!(messages.is_empty());
        assert_eq!(cursors.get("wxid_alice"), Some(&500));
    }

    #[test]
    fn query_failed_reuses_baseline() {
        let mut cursors = HashMap::new();
        let update = Ok(Some(BridgeUpdate {
            last_sort_seq: Some(500),
            messages: vec![],
        }));
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(500));
        assert!(messages.is_empty());
        assert_eq!(cursors.get("wxid_alice"), Some(&500));
    }

    #[test]
    fn db_lock_failed_uses_existing_cursor() {
        let mut cursors = HashMap::new();
        cursors.insert("wxid_alice".to_string(), 500);
        let update: Result<Option<BridgeUpdate>, _> = Ok(None);
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, Some(500));
        assert!(messages.is_empty());
        assert_eq!(cursors.get("wxid_alice"), Some(&500));
    }

    #[test]
    fn db_lock_failed_no_prior_cursor() {
        let mut cursors = HashMap::new();
        let update: Result<Option<BridgeUpdate>, _> = Ok(None);
        let (last_sort_seq, messages) = apply_update(&mut cursors, "wxid_alice", update);

        assert_eq!(last_sort_seq, None);
        assert!(messages.is_empty());
        assert!(!cursors.contains_key("wxid_alice"));
    }

    #[test]
    fn hidden_talker_is_not_broadcast() {
        let visibility =
            VisibilityIndex::build(&["wxid_secret".to_string()], &[], &ContactResolver::empty());

        assert!(!should_broadcast_talker(&visibility, "wxid_secret"));
        assert!(should_broadcast_talker(&visibility, "wxid_visible"));
    }

    // --- Phase 2: sender-level tests ---

    #[test]
    fn enrich_messages_filters_hidden_sender_in_group() {
        let visibility =
            VisibilityIndex::build(&["wxid_spam".to_string()], &[], &ContactResolver::empty());
        let msgs = vec![
            wx_db::Message {
                sort_seq: 1,
                server_id: 1,
                msg_type: 1,
                sub_type: 0,
                sender: "wxid_spam".to_string(),
                talker: "group@chatroom".to_string(),
                create_time: 100,
                content: wx_db::MessageContent::Text("spam".into()),
                status: 0,
            },
            wx_db::Message {
                sort_seq: 2,
                server_id: 2,
                msg_type: 1,
                sub_type: 0,
                sender: "wxid_normal".to_string(),
                talker: "group@chatroom".to_string(),
                create_time: 101,
                content: wx_db::MessageContent::Text("hello".into()),
                status: 0,
            },
        ];

        let result = enrich_messages(
            msgs,
            "wxid_me",
            &ContactResolver::empty(),
            "group@chatroom",
            &visibility,
        );
        assert_eq!(result.len(), 1, "hidden sender message should be filtered");
        assert_eq!(result[0].message.sender, "wxid_normal");
    }

    #[test]
    fn session_sender_redaction_in_bridge() {
        use crate::schema::project_session_sender;
        let visibility =
            VisibilityIndex::build(&["wxid_spam".to_string()], &[], &ContactResolver::empty());
        let ev = wx_monitor::SessionEvent {
            username: "group@chatroom".to_string(),
            sort_timestamp: 1,
            detected_at: 2,
            kind: wx_monitor::SessionEventKind::Updated,
            summary: "spam message".to_string(),
            last_msg_type: Some(1),
            last_msg_sender: Some("wxid_spam".to_string()),
            last_sender_display_name: Some("Spammer".to_string()),
        };
        let mut enriched =
            crate::schema::enrich_session_event(ev, "wxid_me", &ContactResolver::empty());
        project_session_sender(&mut enriched, &visibility);

        assert_eq!(enriched.session.summary, "[消息已隐藏]");
        assert!(enriched.session.last_msg_sender.is_none());
        assert!(enriched.direction.is_none());
    }
}
