use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::KeychainError;

pub const SUPPORTED_VERSION: &str = "4.1.8.21";

/// Version prefixes accepted for LLDB key extraction.
/// Encryption params (PBKDF2-HMAC-SHA512, 256K iterations) are identical across these versions.
const EXTRACTION_VERSION_PREFIXES: &[&str] = &["4.1.7", "4.1.8"];

/// Check whether a version string is compatible with our LLDB key extraction.
fn is_extraction_compatible(version: &str) -> bool {
    EXTRACTION_VERSION_PREFIXES
        .iter()
        .any(|prefix| version == *prefix || version.starts_with(&format!("{prefix}.")))
}

#[derive(Debug, Clone)]
pub struct AccountDirInfo {
    /// Directory name, e.g. "wxid_example123abc_ab12"
    pub account_id: String,
    /// Normalized base wxid, e.g. "wxid_example123abc"
    pub base_wxid: String,
    /// Full path to the account data directory
    pub data_dir: PathBuf,
    /// Path to message_0.db
    pub message_db_path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub enum DetectionSource {
    RunningProcess,
    LoginKeyInfoMtime,
}

impl std::fmt::Display for DetectionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectionSource::RunningProcess => write!(f, "running-process"),
            DetectionSource::LoginKeyInfoMtime => write!(f, "login-key-info-mtime"),
        }
    }
}

pub struct ActiveAccount {
    pub info: AccountDirInfo,
    pub source: DetectionSource,
}

/// Extract base wxid from an account directory name (conservative).
///
/// Only strips suffix for `wxid_*` prefix accounts. For arbitrary strings
/// (e.g. user CLI input), this is safe. For confirmed account directories,
/// use `extract_base_wxid_for_account_dir()` instead.
pub fn extract_base_wxid(account_id: &str) -> String {
    crate::account_id::canonical_base(account_id)
}

/// Extract base wxid from a confirmed account directory name (aggressive).
///
/// Remains conservative for non-`wxid_` inputs unless the caller can provide
/// an independent confirmation signal via `extract_base_wxid_for_account_dir_under_root()`.
pub fn extract_base_wxid_for_account_dir(account_id: &str) -> String {
    crate::account_id::canonical_base_for_account_dir(account_id)
}

/// Extract base wxid from a confirmed account directory using sibling
/// `all_users/login/<base-id>` directories as the confirmation signal.
pub fn extract_base_wxid_for_account_dir_under_root(
    xwechat_root: &Path,
    account_id: &str,
) -> String {
    let account = crate::account_id::AccountId::parse(account_id);
    let confirmed = read_login_names(xwechat_root)
        .into_iter()
        .find(|login| account.alias_candidate() == Some(login.as_str()));
    crate::account_id::canonical_base_for_account_dir_with_confirmed_base(
        account_id,
        confirmed.as_deref(),
    )
}

/// Find a running WeChat process PID and validate its version.
///
/// Uses `pgrep` to find the PID and checks the installed WeChat version.
/// Does NOT use `lsof` — suitable for commands that only need PID + version.
pub fn find_wechat_pid() -> Result<(u32, String), KeychainError> {
    let pgrep_output = Command::new("pgrep").args(["-x", "WeChat"]).output()?;

    if !pgrep_output.status.success() {
        return Err(KeychainError::WeChatNotRunning);
    }

    let pid_str = String::from_utf8_lossy(&pgrep_output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let pid: u32 = pid_str
        .parse()
        .map_err(|_| KeychainError::WeChatNotRunning)?;

    let version = ensure_supported_wechat_version()?;

    Ok((pid, version))
}

/// Ensure the installed WeChat version matches the supported target.
pub fn ensure_supported_wechat_version() -> Result<String, KeychainError> {
    let version = get_wechat_version()?;
    if !is_extraction_compatible(&version) {
        return Err(KeychainError::UnsupportedVersion { version });
    }
    Ok(version)
}

/// Get WeChat version from the application bundle.
fn get_wechat_version() -> Result<String, KeychainError> {
    let output = Command::new("defaults")
        .args([
            "read",
            "/Applications/WeChat.app/Contents/Info.plist",
            "CFBundleShortVersionString",
        ])
        .output()?;

    if !output.status.success() {
        return Err(KeychainError::Other(
            "failed to read WeChat version from Info.plist".into(),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Shared config directory resolved by `AppPaths`.
pub fn config_dir() -> Result<PathBuf, KeychainError> {
    let ap = wx_paths::AppPaths::new().map_err(|e| KeychainError::Other(e.to_string()))?;
    Ok(ap.config_dir())
}

/// Default xwechat_files base path.
fn default_xwechat_files_base() -> Result<PathBuf, KeychainError> {
    let ap = wx_paths::AppPaths::new().map_err(|e| KeychainError::Other(e.to_string()))?;
    Ok(ap
        .home()
        .join("Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files"))
}

/// Detect account directories from the filesystem (without WeChat running).
///
/// Scans `~/Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files/`
/// for subdirectories containing `db_storage/message/message_0.db`.
pub fn find_account_dirs() -> Result<Vec<AccountDirInfo>, KeychainError> {
    let base = default_xwechat_files_base()?;
    if !base.exists() {
        return Ok(vec![]);
    }
    find_account_dirs_under(&base)
}

/// Detect account directories under a specified root path.
///
/// Scans `base` for subdirectories containing `db_storage/message/message_0.db`.
pub fn find_account_dirs_under(base: &Path) -> Result<Vec<AccountDirInfo>, KeychainError> {
    let mut accounts = Vec::new();
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            let db_path = entry.path().join("db_storage/message/message_0.db");
            if db_path.exists() {
                accounts.push(AccountDirInfo {
                    base_wxid: extract_base_wxid_for_account_dir_under_root(base, &name),
                    account_id: name,
                    data_dir: entry.path(),
                    message_db_path: db_path,
                });
            }
        }
    }
    Ok(accounts)
}

/// Check if a path is an xwechat_files root directory (contains `all_users/` subdirectory).
pub fn is_xwechat_files_root(path: &Path) -> bool {
    path.join("all_users").is_dir()
}

/// Check whether WeChat is currently running via pgrep.
///
/// Unlike `find_wechat_pid()`, this does NOT perform version validation,
/// so it won't block commands (sessions, query, etc.) that don't depend on version.
fn is_wechat_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "WeChat"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Detect the currently active WeChat account.
///
/// Uses mtime of `all_users/login/*/key_info.db-wal` to identify the active account.
/// `pgrep` is only used to determine `DetectionSource` (running-process vs login-key-info-mtime).
pub fn detect_active_account(accounts: &[AccountDirInfo]) -> Result<ActiveAccount, KeychainError> {
    if accounts.is_empty() {
        return Err(KeychainError::AccountDetectionFailed {
            reason: "no account directories provided".into(),
            candidates: String::new(),
        });
    }

    let wechat_running = is_wechat_running();
    let source = if wechat_running {
        DetectionSource::RunningProcess
    } else {
        DetectionSource::LoginKeyInfoMtime
    };

    // Mtime strategy: find most recent login key_info.db
    let xwechat_root = accounts[0]
        .data_dir
        .parent()
        .ok_or_else(|| KeychainError::Other("cannot determine xwechat_files root".into()))?;

    let login_dir = xwechat_root.join("all_users/login");
    if login_dir.is_dir() {
        if let Ok(best_login) = find_most_recent_login(&login_dir) {
            // Match login name to account directories using alias-aware matching
            let matches: Vec<&AccountDirInfo> = accounts
                .iter()
                .filter(|a| {
                    let id = crate::account_id::AccountId::parse(&a.account_id);
                    id.matches(&best_login)
                })
                .collect();

            match matches.len() {
                1 => {
                    return Ok(ActiveAccount {
                        info: matches[0].clone(),
                        source,
                    });
                }
                n if n > 1 => {
                    // Tiebreak by message_0.db-wal mtime
                    if let Some(best) = tiebreak_by_wal_mtime(&matches) {
                        return Ok(ActiveAccount {
                            info: best.clone(),
                            source,
                        });
                    }
                    // Still ambiguous
                    let candidates = matches
                        .iter()
                        .map(|a| format!("  - {}", a.account_id))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(KeychainError::AccountDetectionFailed {
                        reason: format!("multiple account directories match login '{best_login}'"),
                        candidates,
                    });
                }
                _ => {} // no match, fall through
            }
        }
    }

    // If only one account exists, use it
    if accounts.len() == 1 {
        return Ok(ActiveAccount {
            info: accounts[0].clone(),
            source,
        });
    }

    // All strategies failed
    let candidates = accounts
        .iter()
        .map(|a| format!("  - {}", a.account_id))
        .collect::<Vec<_>>()
        .join("\n");
    Err(KeychainError::AccountDetectionFailed {
        reason: "login mtime detection failed".into(),
        candidates,
    })
}

/// Find the login subdirectory with the most recent key_info.db-wal mtime.
fn find_most_recent_login(login_dir: &Path) -> Result<String, KeychainError> {
    let mut best_name: Option<String> = None;
    let mut best_mtime: Option<std::time::SystemTime> = None;

    for entry in std::fs::read_dir(login_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();

        let wal_path = entry.path().join("key_info.db-wal");
        if let Ok(meta) = std::fs::metadata(&wal_path) {
            if let Ok(mtime) = meta.modified() {
                if best_mtime.is_none_or(|t| mtime > t) {
                    best_mtime = Some(mtime);
                    best_name = Some(name);
                }
            }
        }
    }

    best_name.ok_or_else(|| {
        KeychainError::Other("no login directories with key_info.db-wal found".into())
    })
}

fn read_login_names(xwechat_root: &Path) -> Vec<String> {
    let login_dir = xwechat_root.join("all_users/login");
    let Ok(entries) = std::fs::read_dir(&login_dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect()
}

/// Tiebreak multiple matching accounts by message_0.db-wal mtime.
fn tiebreak_by_wal_mtime<'a>(accounts: &[&'a AccountDirInfo]) -> Option<&'a AccountDirInfo> {
    let mut best: Option<&AccountDirInfo> = None;
    let mut best_mtime: Option<std::time::SystemTime> = None;

    for account in accounts {
        let wal_path = account.data_dir.join("db_storage/message/message_0.db-wal");
        if let Ok(meta) = std::fs::metadata(&wal_path) {
            if let Ok(mtime) = meta.modified() {
                if best_mtime.is_none_or(|t| mtime > t) {
                    best_mtime = Some(mtime);
                    best = Some(account);
                }
            }
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_base_wxid_with_suffix() {
        assert_eq!(
            extract_base_wxid("wxid_example123abc_ab12"),
            "wxid_example123abc"
        );
    }

    #[test]
    fn test_extract_base_wxid_no_suffix() {
        assert_eq!(extract_base_wxid("wxid_xxx"), "wxid_xxx");
    }

    #[test]
    fn test_extract_base_wxid_not_wxid() {
        // Conservative: non-wxid pattern is NOT stripped
        assert_eq!(extract_base_wxid("not_a_wxid"), "not_a_wxid");
    }

    #[test]
    fn test_extract_base_wxid_bare_wxid() {
        assert_eq!(extract_base_wxid("wxid_x"), "wxid_x");
    }

    #[test]
    fn test_extract_base_wxid_multiple_underscores() {
        assert_eq!(
            extract_base_wxid("wxid_foobar456def_c3e7"),
            "wxid_foobar456def"
        );
    }

    #[test]
    fn test_extract_base_wxid_legacy_account_conservative() {
        // Conservative: non-wxid not stripped
        assert_eq!(extract_base_wxid("testuser001_1662"), "testuser001_1662");
    }

    #[test]
    fn test_extract_base_wxid_legacy_account_confirmed_dir() {
        // Confirmed dir without extra signal: still conservative
        assert_eq!(
            extract_base_wxid_for_account_dir("testuser001_1662"),
            "testuser001_1662"
        );
    }

    #[test]
    fn test_find_account_dirs_under_keeps_raw_base_without_login_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let account_dir = tmp.path().join("not_a_wxid");
        let db_dir = account_dir.join("db_storage/message");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("message_0.db"), b"fake").unwrap();

        let accounts = find_account_dirs_under(tmp.path()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "not_a_wxid");
        assert_eq!(accounts[0].base_wxid, "not_a_wxid");
    }

    #[test]
    fn test_find_account_dirs_under_uses_login_dir_as_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let account_dir = tmp.path().join("testuser001_1662");
        let db_dir = account_dir.join("db_storage/message");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("message_0.db"), b"fake").unwrap();
        std::fs::create_dir_all(tmp.path().join("all_users/login/testuser001")).unwrap();

        let accounts = find_account_dirs_under(tmp.path()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "testuser001_1662");
        assert_eq!(accounts[0].base_wxid, "testuser001");
    }

    #[test]
    fn test_extract_base_wxid_wxid_test_not_stripped() {
        assert_eq!(extract_base_wxid("wxid_test"), "wxid_test");
    }

    #[test]
    fn test_is_xwechat_files_root_with_all_users() {
        let tmp = std::env::temp_dir().join("test_xwechat_root");
        let _ = std::fs::create_dir_all(tmp.join("all_users"));
        assert!(is_xwechat_files_root(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_xwechat_files_root_without_all_users() {
        let tmp = std::env::temp_dir().join("test_xwechat_no_root");
        let _ = std::fs::create_dir_all(&tmp);
        assert!(!is_xwechat_files_root(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detection_source_display_all_variants() {
        assert_eq!(
            format!("{}", DetectionSource::RunningProcess),
            "running-process"
        );
        assert_eq!(
            format!("{}", DetectionSource::LoginKeyInfoMtime),
            "login-key-info-mtime"
        );
    }
}
