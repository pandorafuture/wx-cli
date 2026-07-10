use std::path::PathBuf;

use crate::cmd::thin_client::{ThinClient, ThinClientError, ThinClientOptions};
use wx_context::{AccountContext, DecryptRequest, DecryptStats, PersistentCache};

type OpenDbAllResult = (
    wx_db::WechatDb,
    Option<PersistentCache>,
    Option<DecryptStats>,
);

/// Open a WechatDb: direct encrypted open if raw_key available, else decrypt+cache (core only).
pub fn open_db_core(
    acct: &AccountContext,
    progress: impl Fn(wx_context::DecryptProgress) + Send + Sync,
) -> Result<(wx_db::WechatDb, Option<DecryptStats>), Box<dyn std::error::Error>> {
    if acct.raw_key.is_some() {
        eprintln!("Direct encrypted open (SQLCipher)");
        let db = wx_context::open_encrypted_db_core(acct)?;
        Ok((db, None))
    } else {
        let params = &wx_decrypt::MACOS_4_1_7_31;
        let cache = PersistentCache::new(acct, params)?;
        let stats = DecryptRequest::new()
            .core()
            .execute_with_progress(&cache, progress)?;
        let db = wx_db::WechatDb::open_core(cache.decrypted_root())?;
        Ok((db, Some(stats)))
    }
}

/// Open a WechatDb: direct encrypted open if raw_key available, else decrypt+cache (all DBs).
pub fn open_db_all(
    acct: &AccountContext,
    progress: impl Fn(wx_context::DecryptProgress) + Send + Sync,
) -> Result<OpenDbAllResult, Box<dyn std::error::Error>> {
    if acct.raw_key.is_some() {
        eprintln!("Direct encrypted open (SQLCipher)");
        let db = wx_context::open_encrypted_db(acct)?;
        Ok((db, None, None))
    } else {
        let params = &wx_decrypt::MACOS_4_1_7_31;
        let cache = PersistentCache::new(acct, params)?;
        let stats = DecryptRequest::new()
            .all()
            .execute_with_progress(&cache, progress)?;
        let db = wx_db::WechatDb::open(cache.decrypted_root())?;
        Ok((db, Some(cache), Some(stats)))
    }
}

/// Print the account detection note (if any) to stderr.
pub fn print_detection_note(acct: &wx_context::AccountContext) {
    if let Some(note) = &acct.detection_note {
        eprintln!("{note}");
    }
}

/// Progress callback for `ensure_decrypted_with_progress` — prints to stderr.
pub fn decrypt_progress_callback(event: wx_context::DecryptProgress) {
    match event {
        wx_context::DecryptProgress::Starting { total } => {
            eprintln!("Decrypting {total} databases...");
        }
        wx_context::DecryptProgress::Decrypting { .. } => {}
        wx_context::DecryptProgress::Decrypted { .. } => {}
        wx_context::DecryptProgress::Skipped { .. } => {}
        wx_context::DecryptProgress::Failed { .. } => {}
        _ => {}
    }
}

/// Print cache decrypt warnings and summary to stderr.
pub fn print_cache_stats(stats: &wx_context::DecryptStats) {
    for w in &stats.warnings {
        eprintln!("  {w}");
    }
    if stats.decrypted > 0 || stats.errors > 0 {
        eprintln!(
            "Cache: {} decrypted, {} cached, {} errors, {} WAL patched",
            stats.decrypted, stats.skipped, stats.errors, stats.wal_patched
        );
    }
}

/// Print FTS index build stats to stderr (silent if index was already fresh).
/// Kept for Task 6: remove self-built FTS index code.
#[allow(dead_code)]
pub fn print_fts_stats(stats: &wx_db::FtsBuildStats) {
    if stats.was_fresh {
        return;
    }
    eprintln!(
        "Search index: {} messages indexed in {:.1}s",
        stats.indexed, stats.duration_secs
    );
}

pub fn parse_hex_key_32(
    hex_key: &str,
    source: &str,
) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let bytes = hex::decode(hex_key).map_err(|e| format!("invalid {source}: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("{source} must be 32 bytes, got {}", bytes.len()).into());
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

pub fn find_db_files(dir: &std::path::Path) -> Result<Vec<PathBuf>, std::io::Error> {
    wx_context::discover_db_files(dir)
        .map(|files| files.into_iter().map(|f| f.path).collect())
        .map_err(|e| std::io::Error::other(e.to_string()))
}

pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// When `all` is true, return [`wx_db::MAX_QUERY_LIMIT`]; otherwise delegate to
/// [`wx_db::effective_limit`] which clamps `limit` into `[DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT]`.
pub fn effective_limit_all(all: bool, limit: usize) -> usize {
    if all {
        wx_db::MAX_QUERY_LIMIT
    } else {
        wx_db::effective_limit(limit)
    }
}

/// Attempt a remote API call via ThinClient. In auto mode, fall back locally only when
/// the initial health probe cannot reach/authenticate with a usable server. Once health
/// succeeds, a failed business request is returned to the caller instead of launching an
/// expensive local SQLCipher query after waiting for the remote timeout.
pub fn try_remote_or_local<T>(
    options: &ThinClientOptions,
    remote_fn: impl FnOnce(&ThinClient) -> Result<T, ThinClientError>,
    local_fn: impl FnOnce() -> Result<T, Box<dyn std::error::Error>>,
    label: &str,
) -> Result<T, Box<dyn std::error::Error>> {
    if options.is_enabled() {
        let client = ThinClient::new(options.clone());
        match client.probe_health() {
            Ok(()) => return remote_fn(&client).map_err(Into::into),
            Err(err) if err.should_fallback(options.mode) => {
                eprintln!(
                    "note: remote server unavailable, falling back to local {label} ({})",
                    err.fallback_detail()
                );
            }
            Err(err) => return Err(err.into()),
        }
    }
    local_fn()
}

pub fn format_month(create_time: i64) -> String {
    chrono::DateTime::from_timestamp(create_time, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m").to_string())
        .unwrap_or_else(|| "1970-01".to_string())
}

pub fn walkdir_dat_files(dir: &std::path::Path) -> Vec<std::fs::DirEntry> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    results.extend(walkdir_dat_files(&entry.path()));
                } else if ft.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".dat") {
                        results.push(entry);
                    }
                }
            }
        }
    }
    results
}

pub fn lookup_or_resolve_nickname(
    store: &mut wx_keychain::KeyStore,
    account: &wx_keychain::AccountDirInfo,
) -> Option<String> {
    if let Some(existing) = store
        .get(&account.account_id)
        .and_then(|k| k.nickname.as_ref())
        .cloned()
    {
        return Some(existing);
    }

    let key_material = store.resolve_key_material(&account.account_id)?;
    let nickname =
        wx_keychain::resolve_nickname(&account.data_dir, &key_material, &account.base_wxid)
            .ok()
            .flatten()?;

    if let Some(key) = store.accounts.get_mut(&account.account_id) {
        key.nickname = Some(nickname.clone());
        // Opportunistically repair base_wxid with canonical value
        key.base_wxid = Some(account.base_wxid.clone());
    }

    Some(nickname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_basic() {
        assert_eq!(sanitize_filename("hello world.txt"), "hello_world.txt");
    }

    #[test]
    fn sanitize_filename_cjk() {
        // CJK chars are alphanumeric in Rust's Unicode definition
        assert_eq!(sanitize_filename("你好世界"), "你好世界");
    }

    #[test]
    fn sanitize_filename_path_separators() {
        assert_eq!(sanitize_filename("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn sanitize_filename_empty() {
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn sanitize_filename_allowed_chars() {
        assert_eq!(sanitize_filename("foo-bar_baz.qux"), "foo-bar_baz.qux");
    }

    #[test]
    fn effective_limit_all_true_returns_max() {
        assert_eq!(effective_limit_all(true, 0), wx_db::MAX_QUERY_LIMIT);
        assert_eq!(effective_limit_all(true, 50), wx_db::MAX_QUERY_LIMIT);
    }

    #[test]
    fn effective_limit_all_false_delegates() {
        assert_eq!(effective_limit_all(false, 0), wx_db::DEFAULT_QUERY_LIMIT);
        assert_eq!(effective_limit_all(false, 50), 50);
        assert_eq!(
            effective_limit_all(false, wx_db::MAX_QUERY_LIMIT + 1),
            wx_db::MAX_QUERY_LIMIT
        );
    }

    #[test]
    fn format_month_zero() {
        assert_eq!(format_month(0), "1970-01");
    }

    #[test]
    fn format_month_normal() {
        // 2024-03-15 12:00:00 UTC
        let result = format_month(1710504000);
        assert!(result.starts_with("2024-03"), "got: {result}");
    }

    #[test]
    fn format_month_leap_year() {
        // 2024-02-29 00:00:00 UTC
        let result = format_month(1709164800);
        assert!(result.starts_with("2024-02"), "got: {result}");
    }
}
