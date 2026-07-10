use std::path::Path;

use base64::Engine;

use crate::error::MediaError;

/// Derive V2 AES key from UIN and WXID.
///
/// Formula: `MD5(format!("{uin}{wxid}")).hex()[:16].as_bytes()` → 16 ASCII bytes.
///
/// The V1 fixed key `cfcd208495d565ef` is a special case: `MD5("0")[:16]`.
pub fn derive_v2_aes_key(uin: &str, wxid: &str) -> [u8; 16] {
    let input = format!("{uin}{wxid}");
    let digest = md5::compute(input.as_bytes());
    let hex_str = format!("{digest:x}"); // 32-char lowercase hex
    let mut key = [0u8; 16];
    key.copy_from_slice(&hex_str.as_bytes()[..16]);
    key
}

/// Extract canonical base account ID from a directory name.
///
/// Example: `wxid_example123abc_ab12` → `"wxid_example123abc"`
/// Example: `testuser001_1662` → `"testuser001_1662"` without extra signal
/// Example: `wxid_test` → `"wxid_test"` (not stripped: would leave bare `wxid`)
pub fn extract_wxid(dir_name: &str) -> String {
    wx_keychain::account_id::canonical_base_for_account_dir(dir_name)
}

fn extract_wxid_from_data_dir(data_dir: &Path) -> Result<String, MediaError> {
    let dir_name = data_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| MediaError::InvalidFormat {
            reason: "cannot determine directory name from data_dir".into(),
        })?;

    Ok(data_dir.parent().map_or_else(
        || extract_wxid(dir_name),
        |root| wx_keychain::process::extract_base_wxid_for_account_dir_under_root(root, dir_name),
    ))
}

/// Read UIN from `config.ini` for the given WeChat account data directory.
///
/// Searches for `last_uin=<base64>` in config.ini under the ilink directory.
///
/// On macOS, the directory layout separates account data from shared app_data:
/// ```text
/// Documents/
/// ├── app_data/radium/ilink/<hash>/kvcomm/config.ini
/// └── xwechat_files/<wxid_xxx_XXXX>/   ← data_dir
/// ```
///
/// This function tries two locations:
/// 1. `<data_dir>/app_data/radium/ilink/` (for tests / alternative layouts)
/// 2. `<data_dir>/../../app_data/radium/ilink/` (standard macOS layout)
///
/// When multiple accounts exist under ilink, the `account_suffix` (e.g. `ab12`
/// from `wxid_xxx_ab12`) is used to match the correct subdirectory.
pub fn read_uin(data_dir: &Path) -> Result<String, MediaError> {
    let suffix =
        extract_account_suffix(data_dir.file_name().and_then(|n| n.to_str()).unwrap_or(""));

    // Candidate ilink directories (in priority order)
    let candidates = [
        data_dir.join("app_data").join("radium").join("ilink"),
        data_dir
            .join("..")
            .join("..")
            .join("app_data")
            .join("radium")
            .join("ilink"),
    ];

    for ilink_dir in &candidates {
        if !ilink_dir.is_dir() {
            continue;
        }
        if let Ok(uin) = read_uin_from_ilink(ilink_dir, suffix.as_deref()) {
            return Ok(uin);
        }
    }

    Err(MediaError::NotFound(format!(
        "no config.ini with last_uin found (searched {} candidate paths)",
        candidates.len()
    )))
}

/// Extract the 4-char account hash suffix from a directory name.
///
/// Delegates to `wx_keychain::AccountId::parse` for consistent suffix detection.
///
/// `wxid_example123abc_ab12` → `Some("ab12")`
fn extract_account_suffix(dir_name: &str) -> Option<String> {
    wx_keychain::AccountId::parse(dir_name)
        .suffix()
        .map(|s| s.to_string())
}

/// Scan an ilink directory for config.ini with last_uin.
///
/// If `account_suffix` is provided, prefer subdirectories whose name starts with it.
fn read_uin_from_ilink(
    ilink_dir: &Path,
    account_suffix: Option<&str>,
) -> Result<String, MediaError> {
    let mut entries: Vec<_> = std::fs::read_dir(ilink_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .collect();

    // Sort: matching-suffix directories first
    if let Some(suffix) = account_suffix {
        entries.sort_by_key(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(suffix) {
                0
            } else {
                1
            }
        });
    }

    for entry in entries {
        let config_path = entry.path().join("kvcomm").join("config.ini");
        if !config_path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&config_path)?;
        if let Some(uin) = parse_uin_from_config(&content) {
            return Ok(uin);
        }
    }

    Err(MediaError::NotFound(
        "no config.ini with last_uin found in ilink directory".into(),
    ))
}

/// Parse `last_uin=<base64>` from config.ini content and return the decoded UIN.
fn parse_uin_from_config(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("last_uin=") {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()?;
            let uin = String::from_utf8(decoded).ok()?;
            if !uin.is_empty() && uin.chars().all(|c| c.is_ascii_digit()) {
                return Some(uin);
            }
        }
    }
    None
}

/// Derive V2 AES key automatically from a WeChat account data directory.
///
/// Combines: read UIN from config.ini + extract WXID from directory name + MD5 derivation.
pub fn derive_v2_key_from_dir(data_dir: &Path) -> Result<[u8; 16], MediaError> {
    let wxid = extract_wxid_from_data_dir(data_dir)?;
    let uin = read_uin(data_dir)?;

    Ok(derive_v2_aes_key(&uin, &wxid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_uin_from_config_valid() {
        let config = "[General]\nlast_uin=MTIzNDU2Nzg5MA==\n";
        assert_eq!(
            parse_uin_from_config(config),
            Some("1234567890".to_string())
        );
    }

    #[test]
    fn test_parse_uin_from_config_missing() {
        assert_eq!(parse_uin_from_config("[General]\nsome_key=value\n"), None);
    }

    #[test]
    fn test_parse_uin_from_config_empty_value() {
        assert_eq!(parse_uin_from_config("last_uin=\n"), None);
    }

    #[test]
    fn test_parse_uin_from_config_non_numeric() {
        assert_eq!(parse_uin_from_config("last_uin=YWJj\n"), None); // "abc"
    }

    #[test]
    fn test_extract_account_suffix() {
        assert_eq!(
            extract_account_suffix("wxid_test_ab12"),
            Some("ab12".to_string())
        );
        assert_eq!(
            extract_account_suffix("wxid_test_c3e7"),
            Some("c3e7".to_string())
        );
        assert_eq!(
            extract_account_suffix("wxid_test"),
            Some("test".to_string())
        );
        assert_eq!(extract_account_suffix("wxid_test_abcde"), None);
        // "no_underscore" has rfind('_') at the `_` after `no`, tail = "underscore" (10 chars, not 4)
        assert_eq!(extract_account_suffix("no_underscore"), None);
    }
}
