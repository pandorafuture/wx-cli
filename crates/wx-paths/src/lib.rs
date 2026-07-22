mod migration;
mod platform;
pub mod sudo;

use std::io;
use std::path::{Path, PathBuf};

use platform::PlatformBaseDirs;

/// Centralized path resolution for wx-cli.
///
/// All implicit/system-managed paths (config, cache, state, logs, temp) are
/// resolved through this struct. User-specified output paths (decrypt --output,
/// export, media) are out of scope.
#[derive(Clone, Debug)]
pub struct AppPaths {
    home: PathBuf,
    config_root: PathBuf,
    cache_root: PathBuf,
    state_root: PathBuf,
    logs_root: PathBuf,
    runtime_root_override: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
pub struct PathsSummary {
    pub platform: &'static str,
    pub config_dir: PathBuf,
    pub keys_file: PathBuf,
    pub settings_file: PathBuf,
    pub cache_root: PathBuf,
    pub state_root: PathBuf,
    pub logs_dir: PathBuf,
    pub server_state_dir: PathBuf,
    pub server_stdout_log: PathBuf,
    pub server_stderr_log: PathBuf,
    pub temp_root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("cannot determine home directory")]
    NoHome,
    #[error("cannot determine config directory")]
    NoConfig,
    #[error("cannot determine cache directory")]
    NoCache,
    #[error("cannot determine state directory")]
    NoState,
    #[error("cannot determine log directory")]
    NoLog,
}

impl AppPaths {
    /// Create a new `AppPaths` by resolving the real user's home directory.
    ///
    /// Under `sudo`, resolves the original user's home via `SUDO_USER` + `getpwnam`.
    pub fn new() -> Result<Self, PathsError> {
        let home = sudo::resolve_real_home()?;
        let dirs = PlatformBaseDirs::resolve(&home)?;

        Ok(Self {
            home,
            config_root: dirs.config_root,
            cache_root: dirs.cache_root,
            state_root: dirs.state_root,
            logs_root: dirs.logs_root,
            runtime_root_override: None,
        })
    }

    /// Create `AppPaths` with a runtime root override for server commands.
    ///
    /// When specified, ALL server runtime files (state, config, lock, logs)
    /// are placed under the given root instead of their platform-default locations.
    pub fn with_runtime_root(root: PathBuf) -> Result<Self, PathsError> {
        let mut ap = Self::new()?;
        ap.runtime_root_override = Some(root);
        Ok(ap)
    }

    /// The resolved home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    // ── Config ──

    /// Config directory.
    ///
    /// macOS: `~/Library/Application Support/wx-cli/config/`
    /// Linux: `~/.config/wx-cli/`
    pub fn config_dir(&self) -> PathBuf {
        self.config_root.clone()
    }

    /// `<config_dir>/keys.toml`
    pub fn keys_file(&self) -> PathBuf {
        self.config_root.join("keys.toml")
    }

    /// `<config_dir>/settings.toml`
    pub fn settings_file(&self) -> PathBuf {
        self.config_root.join("settings.toml")
    }

    // ── Cache ──

    /// Cache root: `~/Library/Caches/wx-cli/` (macOS)
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// `<cache_root>/<account_id>/`
    pub fn account_cache_dir(&self, id: &str) -> PathBuf {
        self.cache_root.join(id)
    }

    /// `<cache_root>/<account_id>/db_storage/`
    pub fn account_db_cache_dir(&self, id: &str) -> PathBuf {
        self.cache_root.join(id).join("db_storage")
    }

    // ── State ──

    /// State root: `~/Library/Application Support/wx-cli/state/` (macOS)
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Server state directory.
    ///
    /// Default: `<state_root>/server/`
    /// With `--runtime-root`: `<runtime_root>/`
    pub fn server_state_dir(&self) -> PathBuf {
        match &self.runtime_root_override {
            Some(root) => root.clone(),
            None => self.state_root.join("server"),
        }
    }

    /// Server lock file: `<server_state_dir>/manager.lock`
    pub fn server_lock_file(&self) -> PathBuf {
        self.server_state_dir().join("manager.lock")
    }

    /// Server config file: `<server_state_dir>/config.json`
    pub fn server_config_file(&self) -> PathBuf {
        self.server_state_dir().join("config.json")
    }

    /// Server state file: `<server_state_dir>/state.json`
    pub fn server_state_file(&self) -> PathBuf {
        self.server_state_dir().join("state.json")
    }

    // ── Logs ──

    /// Logs directory: `~/Library/Logs/wx-cli/` (macOS)
    pub fn logs_dir(&self) -> &Path {
        &self.logs_root
    }

    /// Server stdout log.
    ///
    /// Default: `<logs_root>/server/stdout.log`
    /// With `--runtime-root`: `<runtime_root>/stdout.log`
    pub fn server_stdout_log(&self) -> PathBuf {
        match &self.runtime_root_override {
            Some(root) => root.join("stdout.log"),
            None => self.logs_root.join("server").join("stdout.log"),
        }
    }

    /// Server stderr log.
    ///
    /// Default: `<logs_root>/server/stderr.log`
    /// With `--runtime-root`: `<runtime_root>/stderr.log`
    pub fn server_stderr_log(&self) -> PathBuf {
        match &self.runtime_root_override {
            Some(root) => root.join("stderr.log"),
            None => self.logs_root.join("server").join("stderr.log"),
        }
    }

    // ── Server directories ──

    /// Ensure server state and log directories exist.
    pub fn ensure_server_dirs(&self) -> io::Result<()> {
        std::fs::create_dir_all(self.server_state_dir())?;
        match &self.runtime_root_override {
            Some(_) => {} // logs go in the same dir, already created
            None => {
                std::fs::create_dir_all(self.logs_root.join("server"))?;
            }
        }
        Ok(())
    }

    // ── Temp (associated fns — system-level, no &self) ──

    /// Temp root: `std::env::temp_dir()/wx-cli/`
    pub fn temp_root() -> PathBuf {
        std::env::temp_dir().join("wx-cli")
    }

    /// `<temp_root>/lldb/wechat_capture_key.py`
    pub fn lldb_script_file() -> PathBuf {
        Self::temp_root().join("lldb").join("wechat_capture_key.py")
    }

    /// `<temp_root>/lldb/wechat_lldb_output.txt`
    pub fn lldb_output_file() -> PathBuf {
        Self::temp_root()
            .join("lldb")
            .join("wechat_lldb_output.txt")
    }

    /// `<temp_root>/nickname/<pid>_<nanos>.db`
    pub fn nickname_temp_db(pid: u32, nanos: u128) -> PathBuf {
        Self::temp_root()
            .join("nickname")
            .join(format!("{pid}_{nanos}.db"))
    }

    // ── Utility ──

    /// Create directory and all parents, returning the path on success.
    pub fn ensure_dir(path: &Path) -> io::Result<&Path> {
        std::fs::create_dir_all(path)?;
        Ok(path)
    }

    /// One-time config migration from legacy `~/.config/wechat-utils/`.
    ///
    /// Migrates `keys.toml` and `settings.toml` to the new platform-correct
    /// config directory. Idempotent via sentinel file. Called by
    /// `KeyStore::load_default()` and `Settings::load_default()`.
    pub fn migrate_config(&self) -> Result<(), io::Error> {
        let _ = migration::ensure_config_migrated(&self.home, &self.config_root)?;
        Ok(())
    }

    /// Current platform identifier.
    pub fn platform() -> &'static str {
        #[cfg(target_os = "macos")]
        {
            "macos"
        }
        #[cfg(target_os = "linux")]
        {
            "linux"
        }
        #[cfg(target_os = "windows")]
        {
            "windows"
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            "unknown"
        }
    }

    /// Build a summary of all paths.
    pub fn summary(&self) -> PathsSummary {
        PathsSummary {
            platform: Self::platform(),
            config_dir: self.config_dir(),
            keys_file: self.keys_file(),
            settings_file: self.settings_file(),
            cache_root: self.cache_root.clone(),
            state_root: self.state_root.to_path_buf(),
            logs_dir: self.logs_root.clone(),
            server_state_dir: self.server_state_dir(),
            server_stdout_log: self.server_stdout_log(),
            server_stderr_log: self.server_stderr_log(),
            temp_root: Self::temp_root(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths_new_returns_sensible_paths() {
        let ap = AppPaths::new().expect("AppPaths::new() should succeed in test env");
        assert!(ap.home().is_absolute());
        assert!(ap.config_dir().is_absolute());
        assert!(ap.keys_file().is_absolute());
        assert!(ap.cache_root().is_absolute());
        assert!(ap.state_root().is_absolute());
        assert!(ap.logs_dir().is_absolute());
    }

    #[test]
    fn config_dir_uses_wx_cli_namespace() {
        let ap = AppPaths::new().unwrap();
        let config = ap.config_dir();
        assert!(
            config.to_str().unwrap().contains("wx-cli"),
            "config_dir should contain wx-cli: {:?}",
            config
        );
    }

    #[test]
    fn temp_root_uses_wx_cli_namespace() {
        let temp = AppPaths::temp_root();
        assert!(
            temp.to_str().unwrap().ends_with("wx-cli"),
            "temp_root should end with wx-cli: {:?}",
            temp
        );
    }

    #[test]
    fn lldb_files_under_temp_root() {
        let script = AppPaths::lldb_script_file();
        let output = AppPaths::lldb_output_file();
        assert!(script.starts_with(AppPaths::temp_root()));
        assert!(output.starts_with(AppPaths::temp_root()));
        assert!(script.to_str().unwrap().ends_with("wechat_capture_key.py"));
        assert!(output.to_str().unwrap().ends_with("wechat_lldb_output.txt"));
    }

    #[test]
    fn nickname_temp_db_under_temp_root() {
        let db = AppPaths::nickname_temp_db(1234, 9999);
        assert!(db.starts_with(AppPaths::temp_root()));
        assert!(db.to_str().unwrap().contains("nickname"));
        assert!(db.to_str().unwrap().ends_with("1234_9999.db"));
    }

    #[test]
    fn keys_file_ends_with_keys_toml() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.keys_file().ends_with("keys.toml"));
    }

    #[test]
    fn settings_file_ends_with_settings_toml() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.settings_file().ends_with("settings.toml"));
    }

    #[test]
    fn account_cache_dir_contains_account_id() {
        let ap = AppPaths::new().unwrap();
        let dir = ap.account_cache_dir("wxid_test_ab12");
        assert!(dir.ends_with("wxid_test_ab12"));
    }

    #[test]
    fn account_db_cache_dir_contains_db_storage() {
        let ap = AppPaths::new().unwrap();
        let dir = ap.account_db_cache_dir("wxid_test_ab12");
        assert!(dir.ends_with("wxid_test_ab12/db_storage"));
    }

    #[test]
    fn server_state_dir_under_state_root() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.server_state_dir().starts_with(ap.state_root()));
        assert!(ap.server_state_dir().ends_with("server"));
    }

    #[test]
    fn server_lock_file_under_server_state() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.server_lock_file().starts_with(ap.server_state_dir()));
        assert!(ap.server_lock_file().ends_with("manager.lock"));
    }

    #[test]
    fn server_config_file_under_server_state() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.server_config_file().starts_with(ap.server_state_dir()));
        assert!(ap.server_config_file().ends_with("config.json"));
    }

    #[test]
    fn server_state_file_under_server_state() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.server_state_file().starts_with(ap.server_state_dir()));
        assert!(ap.server_state_file().ends_with("state.json"));
    }

    #[test]
    fn server_logs_under_logs_dir_default() {
        let ap = AppPaths::new().unwrap();
        assert!(ap.server_stdout_log().to_str().unwrap().contains("server"));
        assert!(ap.server_stderr_log().to_str().unwrap().contains("server"));
        assert!(ap.server_stdout_log().ends_with("stdout.log"));
        assert!(ap.server_stderr_log().ends_with("stderr.log"));
    }

    #[test]
    fn with_runtime_root_overrides_server_paths() {
        let root = PathBuf::from("/tmp/test-runtime");
        let ap = AppPaths::with_runtime_root(root.clone()).unwrap();
        assert_eq!(ap.server_state_dir(), root);
        assert_eq!(ap.server_lock_file(), root.join("manager.lock"));
        assert_eq!(ap.server_config_file(), root.join("config.json"));
        assert_eq!(ap.server_state_file(), root.join("state.json"));
        assert_eq!(ap.server_stdout_log(), root.join("stdout.log"));
        assert_eq!(ap.server_stderr_log(), root.join("stderr.log"));
    }

    #[test]
    fn platform_is_known() {
        let p = AppPaths::platform();
        assert!(
            ["macos", "linux", "windows", "unknown"].contains(&p),
            "unexpected platform: {}",
            p
        );
    }

    #[test]
    fn summary_has_all_fields() {
        let ap = AppPaths::new().unwrap();
        let s = ap.summary();
        assert_eq!(s.platform, AppPaths::platform());
        assert_eq!(s.config_dir, ap.config_dir());
        assert_eq!(s.keys_file, ap.keys_file());
        assert_eq!(s.settings_file, ap.settings_file());
        assert_eq!(s.cache_root, ap.cache_root().to_path_buf());
        assert_eq!(s.state_root, ap.state_root().to_path_buf());
        assert_eq!(s.logs_dir, ap.logs_dir().to_path_buf());
        assert_eq!(s.server_state_dir, ap.server_state_dir());
        assert_eq!(s.temp_root, AppPaths::temp_root());
    }

    #[test]
    fn ensure_dir_creates_and_returns() {
        let tmp = std::env::temp_dir().join("wx_paths_test_ensure");
        let _ = std::fs::remove_dir_all(&tmp);
        let result = AppPaths::ensure_dir(&tmp);
        assert!(result.is_ok());
        assert!(tmp.is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(target_os = "macos")]
    mod macos_tests {
        use super::*;

        #[test]
        fn macos_config_under_application_support() {
            let ap = AppPaths::new().unwrap();
            let config = ap.config_dir();
            assert!(
                config
                    .to_str()
                    .unwrap()
                    .contains("Application Support/wx-cli/config"),
                "macOS config should be under Application Support: {:?}",
                config
            );
        }

        #[test]
        fn macos_cache_under_library_caches() {
            let ap = AppPaths::new().unwrap();
            let cache = ap.cache_root();
            assert!(
                cache.to_str().unwrap().contains("Library/Caches/wx-cli"),
                "macOS cache should be under Library/Caches: {:?}",
                cache
            );
        }

        #[test]
        fn macos_state_under_application_support() {
            let ap = AppPaths::new().unwrap();
            let state = ap.state_root();
            assert!(
                state
                    .to_str()
                    .unwrap()
                    .contains("Application Support/wx-cli/state"),
                "macOS state should be under Application Support: {:?}",
                state
            );
        }

        #[test]
        fn macos_logs_under_library_logs() {
            let ap = AppPaths::new().unwrap();
            let logs = ap.logs_dir();
            assert!(
                logs.to_str().unwrap().contains("Library/Logs/wx-cli"),
                "macOS logs should be under Library/Logs: {:?}",
                logs
            );
        }
    }
}
