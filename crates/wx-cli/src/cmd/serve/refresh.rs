use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use wx_context::{
    open_fts_connection, register_mm_fts_tokenizer, DecryptProgress, DecryptRequest,
    PersistentCache,
};
use wx_db::WechatDb;

type Name2IdCache = Arc<std::sync::Mutex<Option<HashMap<i64, String>>>>;

/// Signal sent to the refresh task.
pub enum RefreshTrigger {
    Refresh,
    #[allow(dead_code)]
    Shutdown,
}

/// Background task that runs DecryptRequest when triggered, then refreshes
/// pool connections. Multiple triggers are coalesced: if N signals queue up
/// while a refresh is in progress, only one subsequent refresh runs.
///
/// Epoch is only advanced on successful refresh (decrypt + reopen all succeed).
/// Failed refreshes are logged but do NOT advance the epoch, so bridge waiters
/// will not proceed with stale data.
pub struct RefreshTask {
    trigger_rx: mpsc::Receiver<RefreshTrigger>,
    epoch_tx: watch::Sender<u64>,
    db: Arc<std::sync::Mutex<WechatDb>>,
    cache: Option<Arc<PersistentCache>>,
    shutdown: CancellationToken,
    /// Independent FTS connection to reopen on refresh.
    fts_conn: Option<Arc<std::sync::Mutex<Connection>>>,
    /// Path to FTS DB for reopening.
    fts_path: Option<PathBuf>,
    /// Cache of name2id mapping — cleared when FTS is reopened.
    name2id_cache: Option<Name2IdCache>,
    /// Cache of media DB paths — cleared on every refresh.
    media_db_paths: Option<Arc<std::sync::Mutex<Option<Vec<PathBuf>>>>>,
    /// Cached hardlink.db connection — cleared on refresh so it is reopened lazily.
    hardlink_db_conn: Option<Arc<std::sync::Mutex<Option<Connection>>>>,
}

impl RefreshTask {
    pub fn new(
        trigger_rx: mpsc::Receiver<RefreshTrigger>,
        epoch_tx: watch::Sender<u64>,
        db: Arc<std::sync::Mutex<WechatDb>>,
        cache: Option<Arc<PersistentCache>>,
        shutdown: CancellationToken,
    ) -> Self {
        RefreshTask {
            trigger_rx,
            epoch_tx,
            db,
            cache,
            shutdown,
            fts_conn: None,
            fts_path: None,
            name2id_cache: None,
            media_db_paths: None,
            hardlink_db_conn: None,
        }
    }

    /// Set the independent FTS connection and path for refresh reopening.
    pub fn with_fts(
        mut self,
        fts_conn: Option<Arc<std::sync::Mutex<Connection>>>,
        fts_path: Option<PathBuf>,
    ) -> Self {
        self.fts_conn = fts_conn;
        self.fts_path = fts_path;
        self
    }

    /// Set the caches that should be invalidated on refresh.
    pub fn with_caches(
        mut self,
        name2id_cache: Option<Name2IdCache>,
        media_db_paths: Option<Arc<std::sync::Mutex<Option<Vec<PathBuf>>>>>,
        hardlink_db_conn: Option<Arc<std::sync::Mutex<Option<Connection>>>>,
    ) -> Self {
        self.name2id_cache = name2id_cache;
        self.media_db_paths = media_db_paths;
        self.hardlink_db_conn = hardlink_db_conn;
        self
    }

    pub async fn run(mut self) {
        let mut epoch: u64 = 0;

        loop {
            // Wait for next trigger or shutdown
            let trigger = tokio::select! {
                t = self.trigger_rx.recv() => t,
                _ = self.shutdown.cancelled() => break,
            };
            match trigger {
                Some(RefreshTrigger::Refresh) => {}
                Some(RefreshTrigger::Shutdown) | None => break,
            }

            // Drain/coalesce any queued Refresh signals
            loop {
                match self.trigger_rx.try_recv() {
                    Ok(RefreshTrigger::Refresh) => continue,
                    Ok(RefreshTrigger::Shutdown) => {
                        // Shutdown takes priority — exit immediately
                        return;
                    }
                    Err(_) => break,
                }
            }

            // Run refresh in spawn_blocking.
            let db = Arc::clone(&self.db);
            let cache = self.cache.clone();
            let fts_conn = self.fts_conn.clone();
            let fts_path = self.fts_path.clone();
            let success = tokio::task::spawn_blocking(move || {
                if let Some(cache) = cache {
                    // Decrypt-cache mode: decrypt then selective reopen
                    let modified_paths: Arc<std::sync::Mutex<Vec<String>>> =
                        Arc::new(std::sync::Mutex::new(Vec::new()));
                    let modified_clone = Arc::clone(&modified_paths);

                    let progress_cb = move |event: DecryptProgress| {
                        match &event {
                            DecryptProgress::Decrypted { path, .. } => {
                                modified_clone.lock().unwrap().push(path.clone());
                            }
                            DecryptProgress::Skipped {
                                path,
                                wal_patched: true,
                            } => {
                                modified_clone.lock().unwrap().push(path.clone());
                            }
                            _ => {}
                        }
                        crate::util::decrypt_progress_callback(event);
                    };

                    if let Err(e) = DecryptRequest::new()
                        .all()
                        .execute_with_progress(&cache, progress_cb)
                    {
                        eprintln!("warn: refresh decrypt failed: {e}");
                        return false;
                    }

                    let modified = modified_paths.lock().unwrap();
                    let decrypted_root = cache.decrypted_root();

                    let mut guard = match db.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("warn: refresh db lock failed: {e}");
                            return false;
                        }
                    };

                    if let Err(e) = guard.reopen_sessions() {
                        eprintln!("warn: refresh reopen_sessions failed: {e}");
                        return false;
                    }

                    let mut contact_reopened = false;
                    let mut shards_reopened = 0u32;
                    let mut fts_changed = false;

                    for rel_path in modified.iter() {
                        if rel_path.ends_with("contact/contact.db") && !contact_reopened {
                            if let Err(e) = guard.reopen_contacts() {
                                eprintln!("warn: refresh reopen_contacts failed: {e}");
                                return false;
                            }
                            contact_reopened = true;
                        } else if is_message_shard_path(rel_path) {
                            let abs_path = decrypted_root.join(rel_path);
                            match guard.reopen_pooled_shard(&abs_path) {
                                Ok(true) => shards_reopened += 1,
                                Ok(false) => {
                                    eprintln!(
                                        "warn: unknown shard path (topology change?): {rel_path} — \
                                         restart server to pick up new shards"
                                    );
                                }
                                Err(e) => {
                                    eprintln!("warn: refresh reopen_pooled_shard failed for {rel_path}: {e}");
                                    return false;
                                }
                            }
                        } else if rel_path.ends_with("message/message_fts.db") {
                            fts_changed = true;
                        }
                    }

                    if fts_changed {
                        if let Err(e) = guard.reopen_fts() {
                            eprintln!("warn: refresh reopen_fts failed: {e}");
                            return false;
                        }
                        if let (Some(fts_mutex), Some(path)) = (&fts_conn, &fts_path) {
                            match open_fts_connection(path) {
                                Ok(new_conn) => {
                                    if let Ok(mut fts_guard) = fts_mutex.lock()
                                        as Result<std::sync::MutexGuard<'_, Connection>, _>
                                    {
                                        *fts_guard = new_conn;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("warn: refresh reopen FTS connection failed: {e}");
                                }
                            }
                        }
                    }

                    if !modified.is_empty() {
                        eprintln!(
                            "info: refresh: {} modified file(s), {} shard(s) reopened{}{}",
                            modified.len(),
                            shards_reopened,
                            if contact_reopened { ", contact reopened" } else { "" },
                            if fts_changed { ", FTS reopened" } else { "" },
                        );
                    }

                    true
                } else {
                    // Direct encrypted mode: just reopen all connections
                    let mut guard = match db.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("warn: refresh db lock failed: {e}");
                            return false;
                        }
                    };

                    if let Err(e) = guard.reopen_sessions() {
                        eprintln!("warn: refresh reopen_sessions failed: {e}");
                        return false;
                    }
                    if let Err(e) = guard.reopen_contacts() {
                        eprintln!("warn: refresh reopen_contacts failed: {e}");
                        return false;
                    }
                    if let Err(e) = guard.reopen_all_pooled() {
                        eprintln!("warn: refresh reopen_all_pooled failed: {e}");
                        return false;
                    }

                    // Reopen independent FTS connection
                    if let (Some(fts_mutex), Some(path)) = (&fts_conn, &fts_path) {
                        match guard.open_related_readonly(path).and_then(|conn| {
                            register_mm_fts_tokenizer(&conn).map_err(wx_db::DbError::FtsInit)?;
                            Ok(conn)
                        }) {
                            Ok(new_conn) => {
                                if let Ok(mut fts_guard) = fts_mutex.lock()
                                    as Result<std::sync::MutexGuard<'_, Connection>, _>
                                {
                                    *fts_guard = new_conn;
                                }
                            }
                            Err(e) => {
                                eprintln!("warn: refresh reopen FTS connection failed: {e}");
                            }
                        }
                    }

                    eprintln!("info: refresh (direct mode): all connections reopened");
                    true
                }
            })
            .await;

            let ok = match success {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("warn: refresh task panicked: {e}");
                    false
                }
            };

            // Only advance epoch on successful refresh
            if ok {
                epoch += 1;
                let _ = self.epoch_tx.send(epoch);

                // Invalidate caches that depend on reopened connections.
                if let Some(cache) = &self.name2id_cache {
                    *cache.lock().unwrap() = None;
                }
                if let Some(cache) = &self.media_db_paths {
                    *cache.lock().unwrap() = None;
                }
                if let Some(cache) = &self.hardlink_db_conn {
                    // Drop the old connection; next media query will reopen lazily.
                    *cache.lock().unwrap() = None;
                }
            }
        }
    }
}

/// Check if a relative path looks like a numbered message shard (e.g. `message/message_N.db`).
fn is_message_shard_path(rel_path: &str) -> bool {
    let Some(filename) = rel_path.rsplit('/').next() else {
        return false;
    };
    if !rel_path.contains("message/") {
        return false;
    }
    let Some(stem) = filename.strip_suffix(".db") else {
        return false;
    };
    let Some(suffix) = stem.strip_prefix("message_") else {
        return false;
    };
    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a RefreshTask that will fail on decrypt (no real cache/db).
    /// Used to test channel behavior without needing real WeChat infrastructure.
    fn make_test_task() -> (
        mpsc::Sender<RefreshTrigger>,
        watch::Receiver<u64>,
        CancellationToken,
        RefreshTask,
    ) {
        let (trigger_tx, trigger_rx) = mpsc::channel(64);
        let (epoch_tx, epoch_rx) = watch::channel(0u64);

        // Create a minimal WechatDb fixture that will make decrypt fail
        // (no PersistentCache/AccountContext). We use a temp dir with
        // bare minimum structure so WechatDb::open succeeds.
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("contact")).unwrap();
        rusqlite::Connection::open(base.join("contact/contact.db"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE contact (username TEXT PRIMARY KEY, alias TEXT, remark TEXT, nick_name TEXT, description TEXT, extra_buffer BLOB);",
            )
            .unwrap();
        std::fs::create_dir_all(base.join("session")).unwrap();
        rusqlite::Connection::open(base.join("session/session.db"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE SessionTable (username TEXT, sort_timestamp INTEGER, summary TEXT);",
            )
            .unwrap();
        std::fs::create_dir_all(base.join("message")).unwrap();
        let db = wx_db::WechatDb::open(base).unwrap();
        let db_arc = Arc::new(std::sync::Mutex::new(db));

        // PersistentCache requires AccountContext — we can't easily create one.
        // Instead, create a cache pointing to the temp dir with valid structure.
        let params = &wx_decrypt::MACOS_4_1_7_31;
        let acct = wx_context::AccountContext {
            account_id: "test".to_string(),
            base_wxid: "wxid_test".to_string(),
            data_dir: base.to_path_buf(),
            key_material: wx_decrypt::KeyMaterial::RawKey([0u8; 32]),
            raw_key: Some([0u8; 32]),
            writeback_enabled: false,
            detection_note: None,
        };
        // PersistentCache::new needs encrypted dirs to exist
        std::fs::create_dir_all(base.join("db_storage/contact")).unwrap();
        std::fs::create_dir_all(base.join("db_storage/session")).unwrap();
        std::fs::create_dir_all(base.join("db_storage/message")).unwrap();
        let cache = wx_context::PersistentCache::new(&acct, params).unwrap();
        let cache_arc = Some(Arc::new(cache));

        // Leak the TempDir to keep files alive for the duration of the test
        std::mem::forget(dir);

        let shutdown = CancellationToken::new();
        let task = RefreshTask::new(trigger_rx, epoch_tx, db_arc, cache_arc, shutdown.clone());
        (trigger_tx, epoch_rx, shutdown, task)
    }

    #[tokio::test]
    async fn shutdown_signal_exits_task() {
        let (tx, _rx, _shutdown, task) = make_test_task();
        let handle = tokio::spawn(task.run());

        tx.send(RefreshTrigger::Shutdown).await.unwrap();
        // Task should exit promptly
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task should exit on Shutdown")
            .expect("task should not panic");
    }

    #[tokio::test]
    async fn channel_close_exits_task() {
        let (tx, _rx, _shutdown, task) = make_test_task();
        let handle = tokio::spawn(task.run());

        drop(tx);
        // Task should exit when channel is closed
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task should exit on channel close")
            .expect("task should not panic");
    }

    #[tokio::test]
    async fn refresh_failure_does_not_advance_epoch() {
        let (tx, rx, _shutdown, task) = make_test_task();
        let handle = tokio::spawn(task.run());

        // Send a Refresh signal — decrypt will fail (no encrypted .db files)
        tx.send(RefreshTrigger::Refresh).await.unwrap();

        // Give refresh task time to process
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Epoch should NOT have advanced because decrypt failed
        assert_eq!(*rx.borrow(), 0, "epoch must not advance on decrypt failure");

        tx.send(RefreshTrigger::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiple_refresh_failures_still_dont_advance_epoch() {
        let (tx, rx, _shutdown, task) = make_test_task();
        let handle = tokio::spawn(task.run());

        // Send 3 Refresh signals rapidly — all will fail
        for _ in 0..3 {
            tx.send(RefreshTrigger::Refresh).await.unwrap();
        }

        // Give refresh task time to process
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Epoch should still be 0 — no successful refreshes
        assert_eq!(
            *rx.borrow(),
            0,
            "epoch must not advance on repeated failures"
        );

        tx.send(RefreshTrigger::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn cancellation_token_exits_task() {
        let (_tx, _rx, shutdown, task) = make_test_task();
        let handle = tokio::spawn(task.run());

        // Cancel the token — task should exit promptly
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task should exit on CancellationToken cancel")
            .expect("task should not panic");
    }

    #[test]
    fn is_message_shard_path_recognizes_numbered_shards() {
        assert!(is_message_shard_path("message/message_0.db"));
        assert!(is_message_shard_path("message/message_1.db"));
        assert!(is_message_shard_path("message/message_12.db"));
        assert!(is_message_shard_path("message/message_999.db"));
    }

    #[test]
    fn is_message_shard_path_rejects_non_shards() {
        // FTS database is not a numbered shard
        assert!(!is_message_shard_path("message/message_fts.db"));
        // contact.db is not a message shard
        assert!(!is_message_shard_path("contact/contact.db"));
        // session.db is not a message shard
        assert!(!is_message_shard_path("session/session.db"));
        // No message/ prefix
        assert!(!is_message_shard_path("message_0.db"));
        // Not a .db file
        assert!(!is_message_shard_path("message/message_0.txt"));
        // Empty suffix
        assert!(!is_message_shard_path("message/message_.db"));
        // Non-numeric suffix
        assert!(!is_message_shard_path("message/message_abc.db"));
    }

    #[test]
    fn path_categorization_contact() {
        assert!("contact/contact.db".ends_with("contact/contact.db"));
        assert!(!"message/message_0.db".ends_with("contact/contact.db"));
    }

    #[test]
    fn path_categorization_fts() {
        assert!("message/message_fts.db".ends_with("message/message_fts.db"));
        assert!(!"message/message_0.db".ends_with("message/message_fts.db"));
    }
}
