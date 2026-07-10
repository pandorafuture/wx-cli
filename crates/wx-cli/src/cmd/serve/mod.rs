mod auth;
mod bridge;
mod error;
mod event;
mod handlers;
mod media;
pub(crate) mod refresh;
mod routes;
mod state;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::signal::unix::SignalKind;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use wx_context::{
    register_mm_fts_tokenizer, write_shard_metadata_sidecar, AccountContext, ContactResolver,
    DecryptRequest, PersistentCache, ResolveParams,
};

use crate::util::{print_cache_stats, print_detection_note};
use crate::version;

use self::refresh::{RefreshTask, RefreshTrigger};
use self::state::{AppState, CurrentAccount};
use super::contacts::build_visibility;
use super::server::runtime::{base_url, RuntimeReporter};
use super::server::types::{RuntimeAccountState, ServerRuntimeState, WorkerLifecycle};

pub(crate) fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
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

#[allow(clippy::too_many_arguments)]
pub async fn cmd_serve(
    key_hex: Option<String>,
    data_dir: Option<PathBuf>,
    account: Option<String>,
    poll: bool,
    fsnotify: bool,
    poll_ms: u64,
    host: String,
    port: u16,
    token: Option<String>,
    worker_id: Option<String>,
    runtime_reporter: Option<RuntimeReporter>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Security check: require token when binding to non-loopback
    if !is_loopback(&host) && token.is_none() {
        return Err("--token is required when --host is not loopback".into());
    }

    let t_total = Instant::now();
    let params = &wx_decrypt::MACOS_4_1_7_31;

    // 1. Account resolution + cache
    let t = Instant::now();
    let acct = AccountContext::resolve(&ResolveParams {
        account: account.as_deref(),
        data_dir: data_dir.as_deref(),
        key_hex: key_hex.as_deref(),
    })?;
    print_detection_note(&acct);
    eprintln!(
        "server/timing: account_resolve {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    // 2 & 3. Open DB — direct encrypted or decrypt+cache depending on raw_key
    let direct_mode = acct.raw_key.is_some();
    let (db, cache): (wx_db::WechatDb, Option<PersistentCache>) = if direct_mode {
        let t = Instant::now();
        eprintln!("Direct encrypted open with pool (SQLCipher)");
        let db = wx_context::open_encrypted_db_with_pool(&acct)?;
        eprintln!(
            "server/timing: db_open_encrypted_with_pool {:.0}ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
        (db, None)
    } else {
        let t = Instant::now();
        let cache = PersistentCache::new(&acct, params)?;
        let stats = DecryptRequest::new()
            .all()
            .execute_with_progress(&cache, crate::util::decrypt_progress_callback)?;
        print_cache_stats(&stats);
        eprintln!(
            "server/timing: decrypt {:.0}ms",
            t.elapsed().as_secs_f64() * 1000.0
        );

        let t = Instant::now();
        let db =
            wx_db::WechatDb::open_with_pool(cache.decrypted_root(), register_mm_fts_tokenizer)?;
        eprintln!(
            "server/timing: db_open_with_pool {:.0}ms",
            t.elapsed().as_secs_f64() * 1000.0
        );

        // Write shard metadata sidecar for future routing
        if let Err(e) = write_shard_metadata_sidecar(&db, cache.decrypted_root()) {
            eprintln!("warn: failed to write shard metadata sidecar: {e}");
        }

        (db, Some(cache))
    };

    // 3b. Open independent FTS connection (outside WechatDb Mutex)
    let fts_conn = db.message_fts_path.as_deref().and_then(|fts_path| {
        match db.open_related_readonly(fts_path).and_then(|conn| {
            register_mm_fts_tokenizer(&conn).map_err(wx_db::DbError::FtsInit)?;
            Ok(conn)
        }) {
            Ok(conn) => {
                if let Ok(mode) =
                    conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                {
                    eprintln!("server/fts: journal_mode={mode}");
                }
                Some(Arc::new(std::sync::Mutex::new(conn)))
            }
            Err(e) => {
                eprintln!("warn: cannot open independent FTS connection: {e}");
                None
            }
        }
    });

    // 4. Build contact resolver
    let t = Instant::now();
    let resolver = ContactResolver::build(&db)?;
    let visibility = build_visibility(&acct, &resolver);
    eprintln!(
        "server/timing: resolver_build {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    let self_wxid = acct.base_wxid.clone();
    let current_account = CurrentAccount {
        wxid: self_wxid.clone(),
        name: resolver.display_name(&self_wxid).to_string(),
    };
    let worker_id = worker_id.unwrap_or_else(|| {
        format!(
            "worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        )
    });
    let attach_dir = acct.data_dir.join("msg").join("attach");
    let file_dir = acct.data_dir.join("msg").join("file");
    let video_dir = acct.data_dir.join("msg").join("video");
    let dat_decrypt = wx_media::DatDecryptOptions {
        v2_aes_key: wx_media::derive_v2_key_from_dir(&acct.data_dir).ok(),
        xor_key: None,
    };
    let media_db_dir = if direct_mode {
        acct.data_dir.join("db_storage").join("message")
    } else {
        cache
            .as_ref()
            .map(|c| c.decrypted_root().join("message"))
            .unwrap_or_else(|| acct.data_dir.join("db_storage").join("message"))
    };
    let hardlink_db_path = if direct_mode {
        acct.data_dir
            .join("db_storage")
            .join("hardlink")
            .join("hardlink.db")
    } else {
        cache
            .as_ref()
            .map(|c| c.decrypted_root().join("hardlink").join("hardlink.db"))
            .unwrap_or_else(|| {
                acct.data_dir
                    .join("db_storage")
                    .join("hardlink")
                    .join("hardlink.db")
            })
    };

    // 3c. Open hardlink.db connection (pooled, outside WechatDb Mutex)
    let hardlink_db_conn = if hardlink_db_path.exists() {
        match db.open_related_readonly(&hardlink_db_path) {
            Ok(conn) => {
                eprintln!("server/hardlink: opened pooled connection");
                Some(conn)
            }
            Err(e) => {
                eprintln!("warn: cannot open hardlink.db connection: {e}");
                None
            }
        }
    } else {
        None
    };
    let hardlink_db_conn = Arc::new(std::sync::Mutex::new(hardlink_db_conn));

    // Check encrypted session dir before constructing monitor config
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

    // Capture values before moving db into Mutex
    let fts_path_for_refresh = db.message_fts_path.clone();

    // 5. Create refresh task channels
    let (refresh_tx, refresh_rx) = mpsc::channel::<RefreshTrigger>(64);
    let (epoch_tx, _epoch_rx) = watch::channel(0u64);

    // 6. Build AppState
    let shutdown = CancellationToken::new();
    let (broadcast_tx, _) = broadcast::channel(512);
    let db_arc = Arc::new(std::sync::Mutex::new(db));
    let cache_arc = cache.map(Arc::new);

    let app_state = Arc::new(AppState {
        db: Arc::clone(&db_arc),
        self_wxid,
        current_account,
        worker_id: worker_id.clone(),
        cli_version: version::cli_version_string(),
        resolver: Arc::new(resolver),
        visibility: Arc::new(visibility),
        broadcast_tx,
        auth_token: token.clone(),
        ready: AtomicBool::new(false),
        refresh_tx: refresh_tx.clone(),
        shutdown: shutdown.clone(),
        fts_conn,
        attach_dir,
        media_db_dir,
        file_dir,
        video_dir,
        hardlink_db_path,
        hardlink_db_conn,
        raw_key: acct.raw_key,
        dat_decrypt,
        voice_cache: Arc::new(std::sync::Mutex::new(LruCache::new(
            NonZeroUsize::new(256).unwrap(),
        ))),
        image_xor_cache: Arc::new(std::sync::Mutex::new(LruCache::new(
            NonZeroUsize::new(1024).unwrap(),
        ))),
        name2id_cache: Arc::new(std::sync::Mutex::new(None)),
        media_db_paths: Arc::new(std::sync::Mutex::new(None)),
    });

    // 7. Bind TCP listener BEFORE background init (port available immediately)
    let router = routes::build_router(Arc::clone(&app_state));

    let bind_addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let t = Instant::now();
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    eprintln!(
        "server/timing: tcp_bind {:.0}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    let auth_status = if token.is_some() {
        "Bearer token required"
    } else {
        "disabled"
    };
    eprintln!("wx-cli server worker: listening on http://{bind_addr}");
    eprintln!("  SSE endpoint: GET /api/v1/events");
    eprintln!("  REST endpoints:");
    eprintln!("    GET /api/v1/health");
    eprintln!("    GET /api/v1/sessions");
    eprintln!("    GET /api/v1/contacts");
    eprintln!("    GET /api/v1/messages?contact=<name_or_wxid>");
    eprintln!("    GET /api/v1/timeline?since=<unix>&until=<unix>");
    eprintln!("    GET /api/v1/media?server_id=<id>&talker=<wxid>[&format=ogg|mp3]");
    eprintln!("    GET /api/v1/search?q=<keyword>");
    eprintln!("  Auth: {auth_status}");
    let resolved = wx_monitor::resolve_watch_mode(&watch_mode);
    eprintln!("  Monitor: mode={watch_mode:?} -> {resolved:?}, interval={poll_ms}ms");

    if let Some(reporter) = &runtime_reporter {
        reporter.write_state(ServerRuntimeState {
            pid: std::process::id(),
            worker_id: worker_id.clone(),
            lifecycle: WorkerLifecycle::Starting,
            ready: false,
            host: host.clone(),
            port,
            base_url: base_url(&host, port),
            token_configured: token.is_some(),
            cli_version: app_state.cli_version.clone(),
            current_account: Some(RuntimeAccountState {
                wxid: app_state.current_account.wxid.clone(),
                name: app_state.current_account.name.clone(),
            }),
            stdout_log: reporter.ap().server_stdout_log(),
            stderr_log: reporter.ap().server_stderr_log(),
        })?;
    }

    // 8. Background task: init baselines → start monitor → spawn refresh → spawn bridge → set ready
    //    The background task retains monitor ownership and is responsible for cleanup on shutdown.
    let shutdown_bg = shutdown.clone();
    let bg_state = Arc::clone(&app_state);
    let bg_t_total = t_total;
    let runtime_reporter_bg = runtime_reporter.clone();
    let bg_host = host.clone();
    let bg_handle = tokio::spawn(async move {
        // init_baselines in spawn_blocking (holds db lock briefly)
        let db = Arc::clone(&bg_state.db);
        let baselines = tokio::task::spawn_blocking(move || {
            let t = Instant::now();
            let guard = match db.lock() {
                Ok(g) => g,
                Err(e) => {
                    return Err(format!("db lock failed: {e}"));
                }
            };
            let result = bridge::init_baselines(&guard);
            eprintln!(
                "server/timing: init_baselines {:.0}ms",
                t.elapsed().as_secs_f64() * 1000.0
            );
            result
        })
        .await;

        let (bridge_cursors, startup_watermark) = match baselines {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                eprintln!("error: bridge init_baselines: {e}, SSE will remain unavailable");
                return;
            }
            Err(e) => {
                eprintln!(
                    "error: bridge init_baselines panicked: {e}, SSE will remain unavailable"
                );
                return;
            }
        };

        // Start monitor
        let t = Instant::now();
        let mut monitor = match wx_monitor::WechatMonitor::start(config) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: monitor start failed: {e}, SSE will remain unavailable");
                return;
            }
        };
        eprintln!(
            "server/timing: monitor_start {:.0}ms",
            t.elapsed().as_secs_f64() * 1000.0
        );

        let receiver = monitor.take_receiver().expect("receiver already taken");

        // Spawn refresh task BEFORE bridge (so refresh loop is running when first events arrive)
        let refresh_task = RefreshTask::new(
            refresh_rx,
            epoch_tx.clone(),
            Arc::clone(&db_arc),
            cache_arc.clone(),
            shutdown_bg.clone(),
        )
        .with_fts(bg_state.fts_conn.clone(), fts_path_for_refresh)
        .with_caches(
            Some(Arc::clone(&bg_state.name2id_cache)),
            Some(Arc::clone(&bg_state.media_db_paths)),
            Some(Arc::clone(&bg_state.hardlink_db_conn)),
        );
        let refresh_handle = tokio::spawn(refresh_task.run());

        // Spawn bridge with refresh channels
        let bridge_refresh_watch = epoch_tx.subscribe();
        let bridge_handle = tokio::spawn(bridge::run_bridge(
            receiver,
            Arc::clone(&bg_state),
            bridge_cursors,
            startup_watermark,
            refresh_tx,
            bridge_refresh_watch,
            shutdown_bg.clone(),
        ));

        // Mark ready
        bg_state.ready.store(true, Ordering::Release);
        if let Some(reporter) = &runtime_reporter_bg {
            let _ = reporter.write_state(ServerRuntimeState {
                pid: std::process::id(),
                worker_id: bg_state.worker_id.clone(),
                lifecycle: WorkerLifecycle::Running,
                ready: true,
                host: bg_host.clone(),
                port,
                base_url: base_url(&bg_host, port),
                token_configured: bg_state.auth_token.is_some(),
                cli_version: bg_state.cli_version.clone(),
                current_account: Some(RuntimeAccountState {
                    wxid: bg_state.current_account.wxid.clone(),
                    name: bg_state.current_account.name.clone(),
                }),
                stdout_log: reporter.ap().server_stdout_log(),
                stderr_log: reporter.ap().server_stderr_log(),
            });
        }
        eprintln!(
            "server/timing: TOTAL startup {:.0}ms",
            bg_t_total.elapsed().as_secs_f64() * 1000.0
        );

        // Wait for shutdown signal, then clean up
        shutdown_bg.cancelled().await;
        if let Some(reporter) = &runtime_reporter_bg {
            let _ = reporter.write_state(ServerRuntimeState {
                pid: std::process::id(),
                worker_id: bg_state.worker_id.clone(),
                lifecycle: WorkerLifecycle::Stopping,
                ready: false,
                host: bg_host.clone(),
                port,
                base_url: base_url(&bg_host, port),
                token_configured: bg_state.auth_token.is_some(),
                cli_version: bg_state.cli_version.clone(),
                current_account: Some(RuntimeAccountState {
                    wxid: bg_state.current_account.wxid.clone(),
                    name: bg_state.current_account.name.clone(),
                }),
                stdout_log: reporter.ap().server_stdout_log(),
                stderr_log: reporter.ap().server_stderr_log(),
            });
        }
        monitor.stop();
        let _ = bridge_handle.await;
        let _ = refresh_handle.await;
    });

    // Signal handler: cancel the shutdown token on SIGTERM or SIGINT
    let shutdown_signal = shutdown.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
                .expect("failed to register SIGTERM handler");
            let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())
                .expect("failed to register SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sigint.recv() => {},
            }
            eprintln!("\nShutting down...");
            shutdown_signal.cancel();
        })
        .await?;

    // Wait for background supervisor to finish cleanup (monitor.stop, join bridge/refresh).
    // On timeout, hard-exit to avoid Runtime::drop blocking on in-flight spawn_blocking tasks.
    match tokio::time::timeout(Duration::from_secs(5), bg_handle).await {
        Ok(Ok(())) => eprintln!("Server stopped."),
        Ok(Err(e)) => eprintln!("warn: background task panicked: {e}"),
        Err(_) => {
            eprintln!("warn: shutdown timed out after 5s, exiting anyway");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        }
    }
    if let Some(reporter) = runtime_reporter {
        let _ = reporter.clear_state();
    }
    let _ = std::io::Write::flush(&mut std::io::stderr());
    Ok(())
}
