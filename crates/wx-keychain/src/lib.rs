pub mod account_id;
pub mod error;
pub mod lldb;
pub mod mach_vm;
pub mod nickname;
pub mod process;
pub mod script;
pub mod store;

pub use account_id::AccountId;
pub use error::KeychainError;
pub use lldb::{capture_key, CaptureResult};
#[cfg(target_os = "macos")]
pub use mach_vm::{capture_key_mach, MachCaptureResult};
pub use nickname::resolve_nickname;
pub use process::config_dir;
pub use process::detect_active_account;
pub use process::{
    ensure_supported_wechat_version, extract_base_wxid, find_account_dirs, find_account_dirs_under,
    find_wechat_pid, is_xwechat_files_root, AccountDirInfo, ActiveAccount, DetectionSource,
    SUPPORTED_VERSION,
};
pub use store::{AccountKey, EncKeyEntry, KeyStore};
pub use wx_decrypt::read_db_salt;

use std::process::Command;

/// 单项前置条件检查的结果。
pub struct PreflightCheck {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
    pub fix_cmd: Option<String>,
}

pub fn check_sip() -> PreflightCheck {
    let (passed, detail) = match Command::new("csrutil").arg("status").output() {
        Ok(output) => {
            let s = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if s.contains("disabled") {
                (true, "SIP is disabled".into())
            } else {
                (false, "SIP is enabled".into())
            }
        }
        Err(e) => (false, format!("cannot run csrutil: {e}")),
    };
    PreflightCheck {
        name: "SIP disabled",
        passed,
        detail,
        fix_cmd: if passed {
            None
        } else {
            Some("csrutil disable  # run in Recovery Mode".into())
        },
    }
}

pub fn check_dev_tools_security() -> PreflightCheck {
    let (passed, detail) = match Command::new("DevToolsSecurity").arg("-status").output() {
        Ok(output) => {
            let s = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if s.contains("enabled") {
                (true, "DevToolsSecurity is enabled".into())
            } else {
                (false, "DevToolsSecurity is not enabled".into())
            }
        }
        Err(e) => (false, format!("cannot run DevToolsSecurity: {e}")),
    };
    PreflightCheck {
        name: "DevToolsSecurity",
        passed,
        detail,
        fix_cmd: if passed {
            None
        } else {
            Some("sudo DevToolsSecurity -enable".into())
        },
    }
}

pub fn check_developer_group() -> PreflightCheck {
    let sudo_user = std::env::var("SUDO_USER").ok();

    let (passed, detail) = match &sudo_user {
        Some(user) => {
            // Running under sudo — check the invoking user's groups, not root's.
            match Command::new("id").args(["-Gn", user]).output() {
                Ok(output) => {
                    let s = String::from_utf8_lossy(&output.stdout);
                    if s.split_whitespace().any(|g| g == "_developer") {
                        (true, format!("{user} is in _developer group"))
                    } else {
                        (false, format!("{user} is NOT in _developer group"))
                    }
                }
                Err(e) => (false, format!("cannot check groups for {user}: {e}")),
            }
        }
        None => match Command::new("groups").output() {
            Ok(output) => {
                let s = String::from_utf8_lossy(&output.stdout);
                if s.split_whitespace().any(|g| g == "_developer") {
                    (true, "user is in _developer group".into())
                } else {
                    (false, "user is NOT in _developer group".into())
                }
            }
            Err(e) => (false, format!("cannot run groups: {e}")),
        },
    };

    let fix_user = sudo_user.as_deref().unwrap_or("$USER");
    PreflightCheck {
        name: "_developer group",
        passed,
        detail,
        fix_cmd: if passed {
            None
        } else {
            Some(format!(
                "sudo dscl . -append /Groups/_developer GroupMembership {fix_user}"
            ))
        },
    }
}

pub fn check_binary(name: &'static str, version_flag: &str) -> PreflightCheck {
    let (passed, detail) = match Command::new(name).arg(version_flag).output() {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            (
                true,
                if ver.is_empty() {
                    format!("{name} OK")
                } else {
                    ver
                },
            )
        }
        Ok(_) => (false, format!("{name} found but returned error")),
        Err(_) => (false, format!("{name} not found")),
    };
    PreflightCheck {
        name,
        passed,
        detail,
        fix_cmd: if passed {
            None
        } else {
            Some("xcode-select --install".into())
        },
    }
}

/// 运行所有前置条件检查，返回结果列表。
pub fn all_preflight_checks() -> Vec<PreflightCheck> {
    vec![
        check_sip(),
        check_dev_tools_security(),
        check_developer_group(),
        check_binary("lldb", "--version"),
        check_binary("python3", "-V"),
    ]
}

/// Pre-flight checks before key extraction.
///
/// Verifies: SIP disabled, DevToolsSecurity enabled, _developer group membership,
/// LLDB and python3 available.
pub fn preflight_checks() -> Result<(), KeychainError> {
    for check in all_preflight_checks() {
        if !check.passed {
            return Err(match check.name {
                "SIP disabled" => KeychainError::SipEnabled,
                "DevToolsSecurity" => KeychainError::DevToolsSecurityDisabled,
                "_developer group" => {
                    let user = std::env::var("SUDO_USER")
                        .or_else(|_| std::env::var("USER"))
                        .unwrap_or_else(|_| "$USER".into());
                    KeychainError::NotInDeveloperGroup(user)
                }
                "lldb" => KeychainError::LldbNotFound,
                "python3" => KeychainError::Python3NotFound,
                _ => KeychainError::Other(check.detail),
            });
        }
    }
    Ok(())
}
