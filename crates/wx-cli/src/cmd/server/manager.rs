use std::fs::OpenOptions;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use wx_paths::AppPaths;

use super::runtime::{
    acquire_management_lock, base_url, load_launch_config, load_runtime_state, pid_is_running,
    pid_matches_managed_worker, probe_health, remove_runtime_state, save_launch_config,
    save_runtime_state, terminate_pid, wait_for_pid_exit,
};
use super::types::{
    RuntimeAccountState, ServerHealthState, ServerLaunchConfig, ServerRestartArgs, ServerRunArgs,
    ServerRuntimeState, ServerStatusArgs, ServerStatusKind, ServerStatusReport, ServerStopArgs,
    WorkerLifecycle,
};
use crate::version;
use crate::OutputFormat;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

fn resolve_app_paths(
    runtime_root: Option<PathBuf>,
) -> Result<AppPaths, Box<dyn std::error::Error>> {
    match runtime_root {
        Some(root) => Ok(AppPaths::with_runtime_root(root)?),
        None => Ok(AppPaths::new()?),
    }
}

use std::path::PathBuf;

pub async fn cmd_server_run(args: ServerRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let args_runtime_root = args.runtime_root.clone();
    let ap = resolve_app_paths(args_runtime_root.clone())?;
    let _lock = acquire_management_lock(&ap)?;
    let config: ServerLaunchConfig = args.into();

    validate_launch_config(&config)?;
    ap.ensure_server_dirs()?;

    let existing_state = load_runtime_state(&ap)?;
    let report = build_status_report(&ap)?;
    if existing_state
        .as_ref()
        .is_some_and(|state| pid_is_running(state.pid))
    {
        if let Some(existing_config) = load_launch_config(&ap)? {
            if existing_config != config {
                return Err(
                    "server already running with different launch configuration; stop or restart it before changing host/port/token"
                        .into(),
                );
            }
        }
        return match report.status {
            ServerStatusKind::Running | ServerStatusKind::Starting => {
                print_run_summary("server already running", &report);
                Ok(())
            }
            _ => Err(
                "managed server worker is still running but unhealthy; use `wx-cli server stop` or `wx-cli server restart`"
                    .into(),
            ),
        };
    }

    save_launch_config(&ap, &config)?;
    remove_runtime_state(&ap)?;
    let worker_id = generate_worker_id();
    let mut child = spawn_worker(&ap, &config, &worker_id, args_runtime_root.as_ref())?;
    let state = starting_state(&ap, &config, child.id(), worker_id);
    save_runtime_state(&ap, &state)?;

    wait_for_worker_ready(&ap, &config, &mut child)?;
    let report = build_status_report(&ap)?;
    print_run_summary("server started", &report);
    Ok(())
}

pub fn cmd_server_status(args: ServerStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ap = resolve_app_paths(args.runtime_root)?;
    let report = build_status_report(&ap)?;
    match args.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => print_status_text(&report),
    }
    Ok(())
}

pub async fn cmd_server_stop(args: ServerStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ap = resolve_app_paths(args.runtime_root)?;
    let _lock = acquire_management_lock(&ap)?;
    let Some(state) = load_runtime_state(&ap)? else {
        println!("server not running");
        return Ok(());
    };

    if !pid_is_running(state.pid) {
        remove_runtime_state(&ap)?;
        println!(
            "removed stale server state (pid {} is not running)",
            state.pid
        );
        return Ok(());
    }

    terminate_pid(state.pid)?;
    if !wait_for_pid_exit(state.pid, STOP_TIMEOUT) {
        return Err(format!(
            "server pid {} did not exit within {:?}",
            state.pid, STOP_TIMEOUT
        )
        .into());
    }

    remove_runtime_state(&ap)?;
    println!("server stopped");
    Ok(())
}

pub async fn cmd_server_restart(args: ServerRestartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ap = resolve_app_paths(args.runtime_root.clone())?;
    let config = load_launch_config(&ap)?
        .ok_or("no persisted server launch configuration found; run `wx-cli server run` first")?;

    let stop_args = ServerStopArgs {
        runtime_root: args.runtime_root.clone(),
    };
    let status = build_status_report(&ap)?;
    if !matches!(status.status, ServerStatusKind::NotRunning) {
        cmd_server_stop(stop_args).await?;
    }

    cmd_server_run(ServerRunArgs {
        key: config.key,
        data_dir: config.data_dir,
        account: config.account,
        poll: config.poll,
        fsnotify: config.fsnotify,
        poll_ms: config.poll_ms,
        host: config.host,
        port: config.port,
        token: config.token,
        runtime_root: args.runtime_root,
    })
    .await
}

pub fn build_status_report(
    ap: &AppPaths,
) -> Result<ServerStatusReport, Box<dyn std::error::Error>> {
    let state = load_runtime_state(ap)?;
    let config = load_launch_config(ap)?;

    let mut notes = Vec::new();

    let mut report = ServerStatusReport {
        status: ServerStatusKind::NotRunning,
        runtime_root: ap.server_state_dir(),
        state_file: ap.server_state_file(),
        config_file: ap.server_config_file(),
        stdout_log: ap.server_stdout_log(),
        stderr_log: ap.server_stderr_log(),
        pid: state.as_ref().map(|s| s.pid),
        base_url: state
            .as_ref()
            .map(|s| s.base_url.clone())
            .or_else(|| config.as_ref().map(|c| base_url(&c.host, c.port))),
        ready: state.as_ref().is_some_and(|s| s.ready),
        health: ServerHealthState::Skipped,
        cli_version: state.as_ref().map(|s| s.cli_version.clone()),
        current_account: state.as_ref().and_then(|s| s.current_account.clone()),
        notes: Vec::new(),
    };

    match state {
        None => {
            if config.is_some() {
                notes.push(
                    "persisted launch configuration exists, but no active runtime state"
                        .to_string(),
                );
            }
        }
        Some(state) if !pid_is_running(state.pid) => {
            report.status = ServerStatusKind::Stale;
            notes.push(format!(
                "runtime state references pid {} but that process is not running",
                state.pid
            ));
        }
        Some(state) => match probe_health(
            &state.base_url,
            config.as_ref().and_then(|c| c.token.as_deref()),
        ) {
            Ok(health) if health.ready => {
                report.status = ServerStatusKind::Running;
                report.health = ServerHealthState::Healthy;
                report.ready = true;
                report.cli_version = Some(health.cli_version);
                report.current_account = Some(health.current_account);
            }
            Ok(health) if health.worker_id != state.worker_id => {
                report.status = ServerStatusKind::Broken;
                report.health = ServerHealthState::Unreachable;
                notes.push(format!(
                    "health probe returned worker_id {} but runtime state expected {}",
                    health.worker_id, state.worker_id
                ));
                if !pid_matches_managed_worker(state.pid, &state.worker_id) {
                    notes.push(format!(
                        "pid {} also does not match the stored managed worker identity",
                        state.pid
                    ));
                }
            }
            Ok(health) => {
                report.status = match state.lifecycle {
                    WorkerLifecycle::Starting => ServerStatusKind::Starting,
                    WorkerLifecycle::Stopping => ServerStatusKind::Stopping,
                    WorkerLifecycle::Running => ServerStatusKind::Broken,
                };
                report.health = ServerHealthState::NotReady;
                report.ready = false;
                report.cli_version = Some(health.cli_version);
                report.current_account = Some(health.current_account);
                notes.push("health probe returned ready=false".to_string());
            }
            Err(err) => {
                report.status = match state.lifecycle {
                    WorkerLifecycle::Starting => ServerStatusKind::Starting,
                    WorkerLifecycle::Stopping => ServerStatusKind::Stopping,
                    WorkerLifecycle::Running => ServerStatusKind::Broken,
                };
                report.health = ServerHealthState::Unreachable;
                notes.push(format!("health probe failed: {err}"));
                if !pid_matches_managed_worker(state.pid, &state.worker_id) {
                    notes.push(format!(
                        "pid {} does not match the stored managed worker identity",
                        state.pid
                    ));
                }
            }
        },
    }

    report.notes = notes;
    Ok(report)
}

fn validate_launch_config(config: &ServerLaunchConfig) -> Result<(), Box<dyn std::error::Error>> {
    if !crate::cmd::serve::is_loopback(&config.host) && config.token.is_none() {
        return Err("--token is required when --host is not loopback".into());
    }
    Ok(())
}

fn spawn_worker(
    ap: &AppPaths,
    config: &ServerLaunchConfig,
    worker_id: &str,
    runtime_root: Option<&PathBuf>,
) -> Result<Child, Box<dyn std::error::Error>> {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ap.server_stdout_log())?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ap.server_stderr_log())?;

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("server")
        .arg("_worker")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    // Only pass --runtime-root when the user explicitly specified it.
    // In default mode, the worker calls AppPaths::new() and uses
    // platform-native paths (separate state and logs directories).
    if let Some(root) = runtime_root {
        command.arg("--runtime-root").arg(root);
    }

    command.arg("--worker-id").arg(worker_id);

    if let Some(key) = &config.key {
        command.arg("--key").arg(key);
    }
    if let Some(data_dir) = &config.data_dir {
        command.arg("--data-dir").arg(data_dir);
    }
    if let Some(account) = &config.account {
        command.arg("--account").arg(account);
    }
    if config.poll {
        command.arg("--poll");
    }
    if config.fsnotify {
        command.arg("--fsnotify");
    }
    command.arg("--poll-ms").arg(config.poll_ms.to_string());
    command.arg("--host").arg(&config.host);
    command.arg("--port").arg(config.port.to_string());
    if let Some(token) = &config.token {
        command.arg("--token").arg(token);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    Ok(command.spawn()?)
}

fn wait_for_worker_ready(
    ap: &AppPaths,
    _config: &ServerLaunchConfig,
    child: &mut Child,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "server worker exited early with {status}; inspect {}",
                ap.server_stderr_log().display()
            )
            .into());
        }

        if let Ok(report) = build_status_report(ap) {
            if matches!(report.status, ServerStatusKind::Running) {
                return Ok(());
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    let _ = child.kill();
    Err(format!(
        "server worker did not become ready within {:?}; inspect {}",
        START_TIMEOUT,
        ap.server_stderr_log().display()
    )
    .into())
}

fn starting_state(
    ap: &AppPaths,
    config: &ServerLaunchConfig,
    pid: u32,
    worker_id: String,
) -> ServerRuntimeState {
    ServerRuntimeState {
        pid,
        worker_id,
        lifecycle: WorkerLifecycle::Starting,
        ready: false,
        host: config.host.clone(),
        port: config.port,
        base_url: base_url(&config.host, config.port),
        token_configured: config.token.is_some(),
        cli_version: version::cli_version_string(),
        current_account: None,
        stdout_log: ap.server_stdout_log(),
        stderr_log: ap.server_stderr_log(),
    }
}

fn generate_worker_id() -> String {
    format!(
        "worker-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

fn print_run_summary(prefix: &str, report: &ServerStatusReport) {
    println!("{prefix}");
    print_status_text(report);
}

fn print_status_text(report: &ServerStatusReport) {
    let status = match report.status {
        ServerStatusKind::NotRunning => "not running",
        ServerStatusKind::Starting => "starting",
        ServerStatusKind::Running => "running",
        ServerStatusKind::Stopping => "stopping",
        ServerStatusKind::Stale => "stale",
        ServerStatusKind::Broken => "broken",
    };
    println!("Server:      {status}");
    if let Some(pid) = report.pid {
        println!("PID:         {pid}");
    }
    if let Some(base_url) = &report.base_url {
        println!("Base URL:    {base_url}");
    }
    println!("Ready:       {}", if report.ready { "yes" } else { "no" });
    println!(
        "Health:      {}",
        match report.health {
            ServerHealthState::Healthy => "healthy",
            ServerHealthState::NotReady => "not ready",
            ServerHealthState::Unreachable => "unreachable",
            ServerHealthState::Skipped => "skipped",
        }
    );
    if let Some(version) = &report.cli_version {
        println!("CLI:         {version}");
    }
    if let Some(RuntimeAccountState { wxid, name }) = &report.current_account {
        println!("Account:     {wxid} ({name})");
    }
    println!("Runtime root: {}", report.runtime_root.display());
    println!("State file:   {}", report.state_file.display());
    println!("Stdout log:   {}", report.stdout_log.display());
    println!("Stderr log:   {}", report.stderr_log.display());
    for note in &report.notes {
        println!("Note:        {note}");
    }
}
