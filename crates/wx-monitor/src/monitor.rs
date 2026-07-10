use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use wx_db::{SessionQuery, WechatDb};
use wx_decrypt::{CryptoParams, EncKeyPair, KeyMaterial};

use crate::cache::{DecryptCache, UpdateKind};
use crate::error::MonitorError;
use crate::event::SessionEvent;
use crate::tracker::SessionTracker;
use crate::watcher::{FileWatcher, NotifyWatcher, PollingWatcher};

/// Watcher mode selection.
#[derive(Debug, Clone, Default)]
pub enum WatchMode {
    /// Automatic: polling on macOS, fsnotify on other platforms.
    #[default]
    Auto,
    /// Force polling.
    Poll,
    /// Force fsnotify (opt-in on macOS).
    Fsnotify,
}

/// Which watcher implementation to use (resolved from WatchMode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedWatcher {
    Polling,
    Notify,
}

/// Resolve a `WatchMode` to the concrete watcher implementation.
pub fn resolve_watch_mode(mode: &WatchMode) -> ResolvedWatcher {
    match mode {
        WatchMode::Poll => ResolvedWatcher::Polling,
        WatchMode::Fsnotify => ResolvedWatcher::Notify,
        WatchMode::Auto => {
            #[cfg(target_os = "macos")]
            {
                ResolvedWatcher::Polling
            }
            #[cfg(not(target_os = "macos"))]
            {
                ResolvedWatcher::Notify
            }
        }
    }
}

/// Configuration for the monitor.
pub struct MonitorConfig {
    pub encrypted_session_dir: PathBuf,
    pub key_material: KeyMaterial,
    pub params: &'static CryptoParams,
    /// Watcher mode selection. Default: Auto (polling on macOS, fsnotify elsewhere).
    pub watch_mode: WatchMode,
    pub poll_interval: Duration,
    pub channel_capacity: usize,
    /// Raw key for direct encrypted open (bypasses DecryptCache).
    pub raw_key: Option<[u8; 32]>,
    /// Full db_storage root path (required when raw_key is set).
    pub encrypted_root: Option<PathBuf>,
}

/// The main monitor handle.
///
/// Detects changes to an encrypted WeChat session.db, decrypts incrementally,
/// and emits [`SessionEvent`]s.
pub struct WechatMonitor {
    receiver: Option<tokio::sync::mpsc::Receiver<SessionEvent>>,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    shutdown_flag: Option<Arc<AtomicBool>>,
    _task: Option<tokio::task::JoinHandle<()>>,
}

/// Returns true if a `notify::Error` indicates a backend-unavailable condition
/// (should fall back to polling). Returns false for path/permission/config errors
/// (should propagate as Err).
fn is_backend_unavailable(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::Generic(_) => true,
        notify::ErrorKind::Io(io_err) => !matches!(
            io_err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
        ),
        notify::ErrorKind::PathNotFound => false,
        notify::ErrorKind::WatchNotFound => false,
        notify::ErrorKind::InvalidConfig(_) => false,
        notify::ErrorKind::MaxFilesWatch => true,
    }
}

impl WechatMonitor {
    /// Start monitoring the encrypted session directory.
    pub fn start(config: MonitorConfig) -> Result<Self, MonitorError> {
        // 1 & 2. Open DB — direct encrypted or decrypt+cache
        let (db, cache) = if let (Some(raw_key), Some(ref encrypted_root)) =
            (config.raw_key, &config.encrypted_root)
        {
            let derived_keys: Vec<EncKeyPair> = match &config.key_material {
                KeyMaterial::EncKeys(pairs) => pairs.clone(),
                KeyMaterial::EncKey { key, salt } => vec![EncKeyPair {
                    key: *key,
                    salt: *salt,
                }],
                KeyMaterial::RawKey(_) => Vec::new(),
            };
            let db = WechatDb::open_encrypted_core_with_key_cache(
                encrypted_root,
                raw_key,
                &derived_keys,
            )?;
            (db, None)
        } else {
            let mut cache = DecryptCache::new(
                config.encrypted_session_dir.clone(),
                config.key_material.clone(),
                config.params,
            )?;
            cache.initial_decrypt()?;
            let db = WechatDb::open_core(cache.decrypted_root())?;
            (db, Some(cache))
        };

        let initial_sessions = db
            .query_sessions(&SessionQuery::new().limit(10_000))
            .map_err(MonitorError::Db)?;

        // 3. Initialize tracker with initial snapshot
        let mut tracker = SessionTracker::new();
        tracker.diff(&initial_sessions.items);

        // 4. Shutdown channels
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        // 5. File watcher + event channel
        let (file_tx, file_rx) = std::sync::mpsc::channel();

        // Watcher stored as Box<dyn FileWatcher> and moved into the task
        let resolved = resolve_watch_mode(&config.watch_mode);
        tracing::info!(watch_mode = ?config.watch_mode, resolved = ?resolved, "watcher mode selected");

        let session_db = config.encrypted_session_dir.join("session.db");
        let session_wal = config.encrypted_session_dir.join("session.db-wal");
        let poll_interval = config.poll_interval;

        let make_polling =
            |file_tx: std::sync::mpsc::Sender<_>| -> Result<Box<dyn FileWatcher>, MonitorError> {
                let mut pw = PollingWatcher::new(poll_interval, file_tx, shutdown_flag.clone());
                pw.watch(&session_db)?;
                pw.watch(&session_wal)?;
                Ok(Box::new(pw))
            };

        let watcher: Box<dyn FileWatcher> = match resolved {
            ResolvedWatcher::Polling => make_polling(file_tx)?,
            ResolvedWatcher::Notify => match NotifyWatcher::new(file_tx.clone()) {
                Ok(mut nw) => match nw.watch(&config.encrypted_session_dir) {
                    Ok(()) => Box::new(nw),
                    Err(ref e) if is_backend_unavailable_monitor(e) => {
                        tracing::warn!(error = %e, "notify watcher failed to watch directory, falling back to polling");
                        make_polling(file_tx)?
                    }
                    Err(e) => return Err(e),
                },
                Err(ref e) if is_backend_unavailable_monitor(e) => {
                    tracing::warn!(error = %e, "notify watcher init failed, falling back to polling");
                    make_polling(file_tx)?
                }
                Err(e) => return Err(e),
            },
        };

        // 6. Output channel
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(config.channel_capacity);

        // 7. Spawn monitor loop — watcher is moved in to keep it alive
        tracing::info!(
            encrypted_session_dir = %config.encrypted_session_dir.display(),
            poll_interval_ms = poll_interval.as_millis() as u64,
            resolved_watcher = ?resolved,
            "monitor loop starting"
        );

        let task = tokio::task::spawn_blocking(move || {
            let mut db = db;
            let mut cache = cache;
            let shutdown_rx = shutdown_rx;
            let _watcher = watcher; // held alive for the duration of the loop

            loop {
                let got_event = file_rx.recv_timeout(Duration::from_millis(500));

                if shutdown_rx.has_changed().unwrap_or(true) {
                    break;
                }

                if got_event.is_ok() {
                    tracing::debug!("file event received");

                    if let Some(ref mut c) = cache {
                        // Decrypt-cache mode
                        match c.update() {
                            Ok(UpdateKind::WalPatched) => {
                                tracing::debug!(update = "WalPatched", "cache update result");
                                if let Err(e) = db.reopen_sessions() {
                                    tracing::warn!(error = %e, "reopen_sessions failed");
                                    continue;
                                }
                            }
                            Ok(UpdateKind::FullDecrypt) => {
                                tracing::debug!(update = "FullDecrypt", "cache update result");
                                match WechatDb::open(c.decrypted_root()) {
                                    Ok(new_db) => db = new_db,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "WechatDb::open failed");
                                        continue;
                                    }
                                }
                            }
                            Ok(UpdateKind::NoChange) => {
                                tracing::debug!(update = "NoChange", "cache update result");
                                continue;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "cache update failed");
                                continue;
                            }
                        }
                    } else {
                        // Direct encrypted mode — just reopen session connection
                        tracing::debug!("direct mode: reopening session connection");
                        if let Err(e) = db.reopen_sessions() {
                            tracing::warn!(error = %e, "reopen_sessions failed");
                            continue;
                        }
                    }

                    // Query sessions and emit events
                    let sessions = match db.query_sessions(&SessionQuery::new().limit(10_000)) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "query_sessions failed");
                            continue;
                        }
                    };
                    let events = tracker.diff(&sessions.items);
                    tracing::debug!(count = events.len(), "session events emitted");
                    for ev in events {
                        if event_tx.blocking_send(ev).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Ok(Self {
            receiver: Some(event_rx),
            shutdown_tx: Some(shutdown_tx),
            shutdown_flag: Some(shutdown_flag),
            _task: Some(task),
        })
    }

    /// Take the mpsc receiver, allowing an external task to consume events directly.
    ///
    /// After calling this, [`recv()`](Self::recv) will always return `None`.
    pub fn take_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<SessionEvent>> {
        self.receiver.take()
    }

    /// Receive the next session event.
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        self.receiver.as_mut()?.recv().await
    }

    /// Signal the monitor to stop.
    pub fn stop(&self) {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(true);
        }
        if let Some(flag) = &self.shutdown_flag {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Convert into a `Stream` of session events, consuming the monitor handle.
    pub fn into_stream(mut self) -> MonitorStream {
        MonitorStream {
            receiver: self.receiver.take(),
            shutdown_tx: self.shutdown_tx.take(),
            shutdown_flag: self.shutdown_flag.take(),
            _task: self._task.take(),
        }
    }
}

impl Drop for WechatMonitor {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(flag) = self.shutdown_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// A `Stream` of `SessionEvent`s.
///
/// Created by [`WechatMonitor::into_stream()`]. Sends shutdown signal on drop.
pub struct MonitorStream {
    receiver: Option<tokio::sync::mpsc::Receiver<SessionEvent>>,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    shutdown_flag: Option<Arc<AtomicBool>>,
    _task: Option<tokio::task::JoinHandle<()>>,
}

impl Stream for MonitorStream {
    type Item = SessionEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match &mut self.receiver {
            Some(rx) => rx.poll_recv(cx),
            None => std::task::Poll::Ready(None),
        }
    }
}

impl Drop for MonitorStream {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(flag) = self.shutdown_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Check if a MonitorError wraps a backend-unavailable notify error.
fn is_backend_unavailable_monitor(err: &MonitorError) -> bool {
    match err {
        MonitorError::Watcher(notify_err) => is_backend_unavailable(notify_err),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_mode_poll_resolves_to_polling() {
        assert_eq!(
            resolve_watch_mode(&WatchMode::Poll),
            ResolvedWatcher::Polling
        );
    }

    #[test]
    fn watch_mode_fsnotify_resolves_to_notify() {
        assert_eq!(
            resolve_watch_mode(&WatchMode::Fsnotify),
            ResolvedWatcher::Notify
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn watch_mode_auto_resolves_to_polling_on_macos() {
        assert_eq!(
            resolve_watch_mode(&WatchMode::Auto),
            ResolvedWatcher::Polling
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn watch_mode_auto_resolves_to_notify_on_non_macos() {
        assert_eq!(
            resolve_watch_mode(&WatchMode::Auto),
            ResolvedWatcher::Notify
        );
    }
}
