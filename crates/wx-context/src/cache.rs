use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use wx_decrypt::KeyMaterial;

use crate::account::AccountContext;
use crate::file_lock::FileLockMap;
use crate::kdf_cache::KdfCache;
use crate::patch_state::{clear_wal_failed_marker, is_wal_patch_failed, write_wal_failed_marker};
use crate::progress::{DbOutcome, DecryptProgress, DecryptStats, WalOutcome};
use crate::wal_patch::{apply_wal_patch, WalPatchResult};
use crate::ContextError;

pub struct PersistentCache {
    cache_root: PathBuf,
    encrypted_root: PathBuf,
    raw_key: Option<[u8; 32]>,
    account_id: String,
    base_wxid: String,
    writeback_enabled: bool,
    kdf_cache: Mutex<KdfCache>,
    params: &'static wx_decrypt::CryptoParams,
    file_locks: Arc<FileLockMap>,
}

impl PersistentCache {
    pub fn new(
        account: &AccountContext,
        params: &'static wx_decrypt::CryptoParams,
    ) -> Result<Self, ContextError> {
        let cache_base = wx_paths::AppPaths::new()
            .map_err(|e| ContextError::Cache(e.to_string()))?
            .account_cache_dir(&account.account_id);

        let encrypted_root = account.data_dir.join("db_storage");
        if !encrypted_root.exists() {
            return Err(ContextError::Cache(format!(
                "db_storage not found in {}",
                account.data_dir.display()
            )));
        }

        // Initialize KdfCache from stored enc_keys (independent of account.key_material).
        let kdf_cache = match wx_keychain::KeyStore::load_default() {
            Ok(store) => Self::load_kdf_cache_from_store(&store, &account.account_id),
            Err(_) => KdfCache::empty(), // Best-effort: don't block on KeyStore failure
        };

        Ok(Self {
            cache_root: cache_base.join("db_storage"),
            encrypted_root,
            raw_key: account.raw_key,
            account_id: account.account_id.clone(),
            base_wxid: account.base_wxid.clone(),
            writeback_enabled: account.writeback_enabled,
            kdf_cache: Mutex::new(kdf_cache),
            params,
            file_locks: Arc::new(FileLockMap::default()),
        })
    }

    /// 返回解密后的 db_storage 根路径。
    pub fn decrypted_root(&self) -> &Path {
        &self.cache_root
    }

    /// Test-only constructor that bypasses AccountContext resolution.
    #[doc(hidden)]
    pub fn new_for_test(
        cache_root: PathBuf,
        encrypted_root: PathBuf,
        raw_key: Option<[u8; 32]>,
        params: &'static wx_decrypt::CryptoParams,
    ) -> Self {
        Self {
            cache_root,
            encrypted_root,
            raw_key,
            account_id: String::new(),
            base_wxid: String::new(),
            writeback_enabled: false,
            kdf_cache: Mutex::new(KdfCache::empty()),
            params,
            file_locks: Arc::new(FileLockMap::default()),
        }
    }

    /// 确保所有 DB 已解密到缓存目录，按 .mtime 标记跳过未变化的文件。
    pub fn ensure_decrypted(&self) -> Result<DecryptStats, ContextError> {
        self.ensure_decrypted_scoped(&crate::DecryptScope::All, |_| {})
    }

    /// Like `ensure_decrypted`, but fires progress events via the callback.
    pub fn ensure_decrypted_with_progress(
        &self,
        on_progress: impl Fn(DecryptProgress) + Send + Sync,
    ) -> Result<DecryptStats, ContextError> {
        self.ensure_decrypted_scoped(&crate::DecryptScope::All, on_progress)
    }

    /// Decrypt only the databases matching `scope`.
    pub fn ensure_decrypted_scoped(
        &self,
        scope: &crate::DecryptScope,
        on_progress: impl Fn(DecryptProgress) + Send + Sync,
    ) -> Result<DecryptStats, ContextError> {
        use crate::progress::AtomicStats;
        use rayon::prelude::*;

        // Phase 1: Discover
        let all_db_files = crate::db_category::discover_db_files(&self.encrypted_root)?;
        if all_db_files.is_empty() {
            return Err(ContextError::Cache("no .db files in db_storage/".into()));
        }

        // Phase 2: Filter
        let db_files: Vec<_> = all_db_files
            .into_iter()
            .filter(|db| scope.matches(db))
            .collect();

        if db_files.is_empty() {
            return Ok(DecryptStats {
                decrypted: 0,
                skipped: 0,
                errors: 0,
                wal_patched: 0,
                warnings: Vec::new(),
            });
        }

        // Phase 3: Parallel execute
        let atomic_stats = AtomicStats::new();

        db_files.par_iter().for_each(|db| {
            let rel = db
                .path
                .strip_prefix(&self.encrypted_root)
                .expect("db path discovered by discover_db_files must be under encrypted_root");
            let dst = self.cache_root.join(rel);
            let rel_str = rel.display().to_string();

            match self.decrypt_one_db(&db.path, &dst, rel) {
                DbOutcome::Decrypted {
                    wal_patched,
                    wal_warnings,
                } => {
                    atomic_stats.inc_decrypted();
                    if wal_patched {
                        atomic_stats.inc_wal_patched();
                    }
                    for w in wal_warnings {
                        atomic_stats.add_warning(w);
                    }
                    on_progress(DecryptProgress::Decrypted {
                        path: rel_str,
                        wal_patched,
                    });
                }
                DbOutcome::Skipped {
                    wal_patched,
                    wal_warnings,
                } => {
                    atomic_stats.inc_skipped();
                    if wal_patched {
                        atomic_stats.inc_wal_patched();
                    }
                    for w in wal_warnings {
                        atomic_stats.add_warning(w);
                    }
                    on_progress(DecryptProgress::Skipped {
                        path: rel_str,
                        wal_patched,
                    });
                }
                DbOutcome::Failed { warning } => {
                    atomic_stats.inc_errors();
                    let error = warning.clone();
                    atomic_stats.add_warning(warning);
                    on_progress(DecryptProgress::Failed {
                        path: rel_str,
                        error,
                    });
                }
            }
        });

        // Phase 4: Aggregate
        let mut stats = atomic_stats.into_stats();

        if stats.errors > 0 && stats.decrypted == 0 && stats.skipped == 0 {
            return Err(ContextError::Cache(format!(
                "all {} database files failed to decrypt",
                stats.errors
            )));
        }

        // Write back newly-derived enc_keys to keys.toml (best-effort).
        if let Err(e) = self.writeback_enc_keys() {
            stats
                .warnings
                .push(format!("enc_keys writeback failed: {e}"));
        }

        Ok(stats)
    }

    /// Per-DB decrypt logic. Acquires per-file lock, performs authoritative needs_decrypt
    /// check inside the lock, decrypts if needed, and applies WAL patch.
    fn decrypt_one_db(&self, src: &Path, dst: &Path, rel: &Path) -> DbOutcome {
        // Acquire per-file lock before any needs_decrypt check (P7 dedup)
        let lock = self.file_locks.lock_for(dst);
        let _guard = lock.lock().unwrap();

        // Authoritative needs_decrypt check inside the lock
        if !self.needs_decrypt(src, dst) {
            // DB unchanged — but check if WAL was updated independently
            let (wal_patched, wal_warnings) = self.try_wal_patch(src, dst, rel);
            return DbOutcome::Skipped {
                wal_patched,
                wal_warnings,
            };
        }

        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return DbOutcome::Failed {
                    warning: format!("mkdir err {}: {e}", rel.display()),
                };
            }
        }

        match self.decrypt_db(src, dst) {
            Ok(()) => {
                if let Err(e) = self.write_mtime_marker(src, dst) {
                    return DbOutcome::Failed {
                        warning: format!("mtime marker err {}: {e}", rel.display()),
                    };
                }
                let (wal_patched, wal_warnings) = self.try_wal_patch(src, dst, rel);
                DbOutcome::Decrypted {
                    wal_patched,
                    wal_warnings,
                }
            }
            Err(wx_decrypt::DecryptError::AlreadyDecrypted) => {
                if let Err(e) = std::fs::copy(src, dst) {
                    return DbOutcome::Failed {
                        warning: format!("copy err {}: {e}", rel.display()),
                    };
                }
                if let Err(e) = self.write_mtime_marker(src, dst) {
                    return DbOutcome::Failed {
                        warning: format!("mtime marker err {}: {e}", rel.display()),
                    };
                }
                DbOutcome::Decrypted {
                    wal_patched: false,
                    wal_warnings: Vec::new(),
                }
            }
            Err(e) => DbOutcome::Failed {
                warning: format!("decrypt err {}: {e}", rel.display()),
            },
        }
    }

    /// Try WAL patch if WAL file exists. Returns (patched, warnings).
    /// Caller must hold the per-file lock.
    fn try_wal_patch(&self, src: &Path, dst: &Path, rel: &Path) -> (bool, Vec<String>) {
        let wal = src.with_extension("db-wal");
        if !wal.exists() {
            return (false, Vec::new());
        }
        match self.guarded_wal_patch(src, &wal, dst, rel) {
            WalOutcome::Patched => (true, Vec::new()),
            WalOutcome::NothingToDo => (false, Vec::new()),
            WalOutcome::Warning(w) => (false, vec![w]),
        }
    }

    /// Apply WAL patch with failed-marker awareness. Caller must hold the per-file lock.
    /// Returns `WalOutcome` instead of mutating stats directly.
    fn guarded_wal_patch(&self, src: &Path, wal: &Path, dst: &Path, rel: &Path) -> WalOutcome {
        // Check failed marker — skip if previous attempt failed with same mtimes
        if is_wal_patch_failed(src, wal, dst) {
            return WalOutcome::Warning(format!(
                "WAL skipped (previously failed) {}",
                rel.display()
            ));
        }

        // Check mtime — skip if WAL hasn't changed since last successful patch
        if !self.needs_wal_patch(wal, dst) {
            return WalOutcome::NothingToDo;
        }

        // Resolve enc_key for this WAL's parent DB salt via KdfCache.
        let salt = match wx_decrypt::read_main_db_salt_for_path(wal) {
            Ok(s) => s,
            Err(wx_decrypt::DecryptError::AlreadyDecrypted) => {
                return WalOutcome::NothingToDo;
            }
            Err(e) => {
                return WalOutcome::Warning(format!(
                    "WAL skipped (salt read err: {e}) {}",
                    rel.display()
                ));
            }
        };

        let km = {
            let mut cache_guard = self.kdf_cache.lock().unwrap();
            match (cache_guard.lookup(&salt), &self.raw_key) {
                (Some(cached), _) => KeyMaterial::EncKey { key: cached, salt },
                (None, Some(raw)) => {
                    let enc_key = cache_guard.get_or_derive(&salt, raw, self.params);
                    KeyMaterial::EncKey { key: enc_key, salt }
                }
                (None, None) => {
                    return WalOutcome::Warning(format!(
                        "WAL skipped (no raw_key for derivation) {}",
                        rel.display()
                    ));
                }
            }
        };

        self.apply_wal_and_handle_retry(src, wal, dst, rel, &salt, &km)
    }

    /// Core WAL patch logic with stale-key retry. Extracted to avoid duplication.
    /// Translate a `WalPatchResult` into a `WalOutcome`, handling bookkeeping
    /// (clearing/setting WAL markers, writing mtime markers).
    fn handle_wal_result(
        &self,
        result: WalPatchResult,
        src: &Path,
        wal: &Path,
        dst: &Path,
        rel: &Path,
    ) -> WalOutcome {
        match result {
            WalPatchResult::Patched(n) => {
                clear_wal_failed_marker(dst);
                let _ = self.write_wal_mtime_marker(wal, dst);
                if n > 0 {
                    WalOutcome::Patched
                } else {
                    WalOutcome::NothingToDo
                }
            }
            WalPatchResult::NoFrames => {
                clear_wal_failed_marker(dst);
                let _ = self.write_wal_mtime_marker(wal, dst);
                WalOutcome::NothingToDo
            }
            WalPatchResult::ContentFailed(e) => {
                let _ = write_wal_failed_marker(src, wal, dst);
                WalOutcome::Warning(format!("WAL content err {}: {e}", rel.display()))
            }
            WalPatchResult::IoFailed(e) => {
                WalOutcome::Warning(format!("WAL IO err {}: {e}", rel.display()))
            }
        }
    }

    fn apply_wal_and_handle_retry(
        &self,
        src: &Path,
        wal: &Path,
        dst: &Path,
        rel: &Path,
        salt: &[u8; 16],
        km: &KeyMaterial,
    ) -> WalOutcome {
        let result = apply_wal_patch(wal, dst, km, self.params);
        match result {
            WalPatchResult::ContentFailed(ref e)
                if e.contains("incorrect key") && self.raw_key.is_some() =>
            {
                // Stale enc_key — re-derive from raw_key and retry once
                let raw = self.raw_key.as_ref().unwrap();
                let fresh_enc_key = wx_decrypt::kdf::derive_enc_key(raw, salt, self.params);
                {
                    let mut cache_guard = self.kdf_cache.lock().unwrap();
                    cache_guard.insert(salt, &fresh_enc_key);
                }
                let fresh_km = KeyMaterial::EncKey {
                    key: fresh_enc_key,
                    salt: *salt,
                };
                self.apply_wal_final(src, wal, dst, rel, &fresh_km)
            }
            _ => self.handle_wal_result(result, src, wal, dst, rel),
        }
    }

    /// Final WAL patch attempt (after retry). No further retries.
    fn apply_wal_final(
        &self,
        src: &Path,
        wal: &Path,
        dst: &Path,
        rel: &Path,
        km: &KeyMaterial,
    ) -> WalOutcome {
        let result = apply_wal_patch(wal, dst, km, self.params);
        self.handle_wal_result(result, src, wal, dst, rel)
    }

    fn decrypt_db(&self, src: &Path, dst: &Path) -> Result<(), wx_decrypt::DecryptError> {
        let salt = wx_decrypt::read_db_salt(src)?;

        let enc_key = {
            let mut cache_guard = self.kdf_cache.lock().unwrap();
            match (cache_guard.lookup(&salt), &self.raw_key) {
                (Some(cached), _) => cached,
                (None, Some(raw)) => cache_guard.get_or_derive(&salt, raw, self.params),
                (None, None) => return Err(wx_decrypt::DecryptError::NoMatchingEncKey),
            }
        }; // Mutex guard dropped here

        // Try direct decrypt with cached/derived enc_key
        match wx_decrypt::decrypt_db_direct(src, dst, &enc_key, &salt, self.params) {
            Ok(()) => Ok(()),
            Err(wx_decrypt::DecryptError::IncorrectKey) if self.raw_key.is_some() => {
                // Stale enc_key — re-derive from raw_key and retry once
                let raw = self.raw_key.as_ref().unwrap();
                let fresh_enc_key = wx_decrypt::kdf::derive_enc_key(raw, &salt, self.params);
                {
                    let mut cache_guard = self.kdf_cache.lock().unwrap();
                    cache_guard.insert(&salt, &fresh_enc_key);
                }
                wx_decrypt::decrypt_db_direct(src, dst, &fresh_enc_key, &salt, self.params)
            }
            Err(e) => Err(e),
        }
    }

    fn writeback_enc_keys(&self) -> Result<(), ContextError> {
        if !self.writeback_enabled {
            return Ok(());
        }

        let cache_guard = self.kdf_cache.lock().unwrap();
        if !cache_guard.has_new_derivations() {
            return Ok(());
        }

        let session_pairs = cache_guard.all_pairs();
        drop(cache_guard); // Release before file I/O

        // Union merge with existing enc_keys on disk
        let mut store = wx_keychain::KeyStore::load_default()?;
        let mut merged_pairs = session_pairs;
        if let Some(entry) = store.get(&self.account_id) {
            for existing in &entry.enc_keys {
                if let (Ok(key_bytes), Ok(salt_bytes)) =
                    (hex::decode(&existing.enc_key), hex::decode(&existing.salt))
                {
                    if key_bytes.len() == 32 && salt_bytes.len() == 16 {
                        let mut key = [0u8; 32];
                        let mut salt = [0u8; 16];
                        key.copy_from_slice(&key_bytes);
                        salt.copy_from_slice(&salt_bytes);
                        merged_pairs.push(wx_decrypt::EncKeyPair { key, salt });
                    }
                }
            }
        }
        let version = store
            .get(&self.account_id)
            .map(|e| e.wechat_version.clone())
            .unwrap_or_default();
        // Opportunistically repair base_wxid with canonical value
        store.set_enc_keys(
            &self.account_id,
            &merged_pairs,
            &version,
            None,
            Some(self.base_wxid.clone()),
        );
        store.save_default()?;
        Ok(())
    }

    fn load_kdf_cache_from_store(store: &wx_keychain::KeyStore, account_id: &str) -> KdfCache {
        let entry = match store.get(account_id) {
            Some(e) => e,
            None => return KdfCache::empty(),
        };

        // Try new per-DB enc_keys format first.
        if !entry.enc_keys.is_empty() {
            let pairs: Vec<wx_decrypt::EncKeyPair> = entry
                .enc_keys
                .iter()
                .filter_map(|e| {
                    let key_bytes = hex::decode(&e.enc_key).ok()?;
                    let salt_bytes = hex::decode(&e.salt).ok()?;
                    if key_bytes.len() == 32 && salt_bytes.len() == 16 {
                        let mut key = [0u8; 32];
                        let mut salt = [0u8; 16];
                        key.copy_from_slice(&key_bytes);
                        salt.copy_from_slice(&salt_bytes);
                        Some(wx_decrypt::EncKeyPair { key, salt })
                    } else {
                        None
                    }
                })
                .collect();
            if !pairs.is_empty() {
                return KdfCache::from_pairs(&pairs);
            }
        }

        // Legacy single enc_key path.
        if let (Some(ek), Some(es)) = (&entry.enc_key, &entry.enc_key_salt) {
            if !ek.is_empty() && !es.is_empty() {
                if let (Ok(key_bytes), Ok(salt_bytes)) = (hex::decode(ek), hex::decode(es)) {
                    if key_bytes.len() == 32 && salt_bytes.len() == 16 {
                        let mut key = [0u8; 32];
                        let mut salt = [0u8; 16];
                        key.copy_from_slice(&key_bytes);
                        salt.copy_from_slice(&salt_bytes);
                        return KdfCache::from_pairs(&[wx_decrypt::EncKeyPair { key, salt }]);
                    }
                }
            }
        }

        KdfCache::empty()
    }

    /// 检查源文件是否需要重新解密。
    fn needs_decrypt(&self, src: &Path, dst: &Path) -> bool {
        if !dst.exists() {
            return true;
        }
        let marker = mtime_marker_path(dst);
        let recorded = match std::fs::read_to_string(&marker) {
            Ok(s) => s,
            Err(_) => return true,
        };
        let src_mtime = match src.metadata().and_then(|m| m.modified()) {
            Ok(t) => format_system_time(t),
            Err(_) => return false,
        };
        recorded.trim() != src_mtime
    }

    fn write_mtime_marker(&self, src: &Path, dst: &Path) -> Result<(), ContextError> {
        let src_mtime = src.metadata()?.modified()?;
        let marker = mtime_marker_path(dst);
        std::fs::write(&marker, format_system_time(src_mtime))?;
        Ok(())
    }

    /// Check if the WAL file has changed since the last patch.
    fn needs_wal_patch(&self, wal: &Path, dst: &Path) -> bool {
        let marker = wal_mtime_marker_path(dst);
        let recorded = match std::fs::read_to_string(&marker) {
            Ok(s) => s,
            Err(_) => return true,
        };
        let wal_mtime = match wal.metadata().and_then(|m| m.modified()) {
            Ok(t) => format_system_time(t),
            Err(_) => return false,
        };
        recorded.trim() != wal_mtime
    }

    fn write_wal_mtime_marker(&self, wal: &Path, dst: &Path) -> Result<(), ContextError> {
        let wal_mtime = wal.metadata()?.modified()?;
        let marker = wal_mtime_marker_path(dst);
        std::fs::write(&marker, format_system_time(wal_mtime))?;
        Ok(())
    }
}

fn mtime_marker_path(dst: &Path) -> PathBuf {
    dst.with_extension(format!(
        "{}.mtime",
        dst.extension().unwrap_or_default().to_string_lossy()
    ))
}

fn wal_mtime_marker_path(dst: &Path) -> PathBuf {
    dst.with_extension("db.wal_mtime")
}

pub(crate) fn format_system_time(t: SystemTime) -> String {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Test helper: construct a PersistentCache with optional pre-loaded KdfCache entries.
    fn test_cache(
        cache_root: PathBuf,
        encrypted_root: PathBuf,
        raw_key: Option<[u8; 32]>,
        kdf_pairs: &[wx_decrypt::EncKeyPair],
        params: &'static wx_decrypt::CryptoParams,
    ) -> PersistentCache {
        let kdf_cache = if kdf_pairs.is_empty() {
            KdfCache::empty()
        } else {
            KdfCache::from_pairs(kdf_pairs)
        };
        PersistentCache {
            cache_root,
            encrypted_root,
            raw_key,
            account_id: String::new(),
            base_wxid: String::new(),
            writeback_enabled: false,
            kdf_cache: Mutex::new(kdf_cache),
            params,
            file_locks: Arc::new(FileLockMap::default()),
        }
    }

    #[test]
    fn needs_decrypt_no_marker() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("test.db");
        let dst = tmp.path().join("out/test.db");
        std::fs::create_dir_all(tmp.path().join("out")).unwrap();
        std::fs::write(&src, b"data").unwrap();
        std::fs::write(&dst, b"cached").unwrap();
        let cache = test_cache(
            tmp.path().join("out"),
            tmp.path().to_path_buf(),
            None,
            &[],
            &wx_decrypt::MACOS_4_1_7_31,
        );
        assert!(cache.needs_decrypt(&src, &dst));
    }

    #[test]
    fn needs_decrypt_matching_marker() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("test.db");
        let dst = tmp.path().join("out/test.db");
        std::fs::create_dir_all(tmp.path().join("out")).unwrap();
        std::fs::write(&src, b"data").unwrap();
        std::fs::write(&dst, b"cached").unwrap();
        let mtime = src.metadata().unwrap().modified().unwrap();
        let marker = mtime_marker_path(&dst);
        std::fs::write(&marker, format_system_time(mtime)).unwrap();

        let cache = test_cache(
            tmp.path().join("out"),
            tmp.path().to_path_buf(),
            None,
            &[],
            &wx_decrypt::MACOS_4_1_7_31,
        );
        assert!(!cache.needs_decrypt(&src, &dst));
    }

    #[test]
    fn mtime_marker_path_format() {
        let p = PathBuf::from("/cache/message_0.db");
        assert_eq!(
            mtime_marker_path(&p),
            PathBuf::from("/cache/message_0.db.mtime")
        );
    }

    #[test]
    fn enc_keys_decrypt_db_selects_matching_pair() {
        use wx_decrypt::EncKeyPair;

        // Build two encrypted DBs with different salts, same raw key
        let raw_key = [0xABu8; 32];
        let salt1 = [0x01u8; 16];
        let salt2 = [0x02u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let enc_key1 = derive_enc_key(&raw_key, &salt1, params);
        let enc_key2 = derive_enc_key(&raw_key, &salt2, params);

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        let sub = enc_root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(cache_root.join("sub")).unwrap();

        build_encrypted_db(&sub.join("a.db"), &raw_key, &salt1, params);
        build_encrypted_db(&sub.join("b.db"), &raw_key, &salt2, params);

        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            None,
            &[
                EncKeyPair {
                    key: enc_key1,
                    salt: salt1,
                },
                EncKeyPair {
                    key: enc_key2,
                    salt: salt2,
                },
            ],
            params,
        );

        // decrypt a.db (salt1 → enc_key1)
        let src_a = sub.join("a.db");
        let dst_a = cache_root.join("sub").join("a.db");
        cache.decrypt_db(&src_a, &dst_a).unwrap();
        let data = std::fs::read(&dst_a).unwrap();
        assert_eq!(
            &data[..16],
            b"SQLite format 3\0",
            "a.db should be valid SQLite"
        );

        // decrypt b.db (salt2 → enc_key2)
        let src_b = sub.join("b.db");
        let dst_b = cache_root.join("sub").join("b.db");
        cache.decrypt_db(&src_b, &dst_b).unwrap();
        let data = std::fs::read(&dst_b).unwrap();
        assert_eq!(
            &data[..16],
            b"SQLite format 3\0",
            "b.db should be valid SQLite"
        );
    }

    #[test]
    fn enc_keys_decrypt_db_no_match_returns_error() {
        use wx_decrypt::EncKeyPair;

        let raw_key = [0xABu8; 32];
        let salt_db = [0x01u8; 16];
        let salt_wrong = [0x99u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let enc_key_wrong = derive_enc_key(&raw_key, &salt_wrong, params);

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        std::fs::create_dir_all(&enc_root).unwrap();
        build_encrypted_db(&enc_root.join("test.db"), &raw_key, &salt_db, params);

        let cache = test_cache(
            tmp.path().join("cache"),
            enc_root.clone(),
            None,
            &[EncKeyPair {
                key: enc_key_wrong,
                salt: salt_wrong,
            }],
            params,
        );

        let err = cache
            .decrypt_db(
                &enc_root.join("test.db"),
                &tmp.path().join("cache").join("test.db"),
            )
            .unwrap_err();
        assert!(matches!(err, wx_decrypt::DecryptError::NoMatchingEncKey));
    }

    #[test]
    fn guarded_wal_patch_skips_when_failed_marker_exists() {
        use crate::patch_state::{
            is_wal_patch_failed, wal_failed_marker_path, write_wal_failed_marker,
        };

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        let src = enc_root.join("test.db");
        let wal = enc_root.join("test.db-wal");
        let dst = cache_root.join("test.db");
        let rel = Path::new("test.db");

        std::fs::write(&src, b"db-data").unwrap();
        std::fs::write(&wal, b"wal-data").unwrap();
        std::fs::write(&dst, b"cached-db").unwrap();

        // Write a failed marker for the current mtimes
        write_wal_failed_marker(&src, &wal, &dst).unwrap();
        assert!(is_wal_patch_failed(&src, &wal, &dst));

        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            None,
            &[],
            &wx_decrypt::MACOS_4_1_7_31,
        );

        let outcome = cache.guarded_wal_patch(&src, &wal, &dst, rel);

        // Should have been skipped with a warning
        match outcome {
            WalOutcome::Warning(w) => assert!(w.contains("previously failed")),
            other => panic!("expected Warning, got {other:?}"),
        }

        // The failed marker should still exist
        assert!(wal_failed_marker_path(&dst).exists());
    }

    #[test]
    fn guarded_wal_patch_retries_after_mtime_change() {
        use crate::patch_state::{is_wal_patch_failed, write_wal_failed_marker};
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        let src = enc_root.join("test.db");
        let wal = enc_root.join("test.db-wal");
        let dst = cache_root.join("test.db");

        std::fs::write(&src, b"db-data").unwrap();
        std::fs::write(&wal, b"wal-data").unwrap();
        std::fs::write(&dst, b"cached-db").unwrap();

        // Write a failed marker for the current mtimes
        write_wal_failed_marker(&src, &wal, &dst).unwrap();
        assert!(is_wal_patch_failed(&src, &wal, &dst));

        // Advance the WAL mtime — the failed marker should no longer match
        let new_mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() + Duration::from_secs(2),
        );
        filetime::set_file_mtime(&wal, new_mtime).unwrap();

        assert!(!is_wal_patch_failed(&src, &wal, &dst));
        // This proves that guarded_wal_patch would proceed past the failed-marker check
        // and attempt the actual patch (which would fail on our dummy data, but that's
        // testing apply_wal_patch, not the guard logic).
    }

    #[test]
    fn guarded_wal_patch_noframes_clears_failed_marker() {
        use crate::patch_state::{
            is_wal_patch_failed, wal_failed_marker_path, write_wal_failed_marker,
        };
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        let src = enc_root.join("test.db");
        let wal = enc_root.join("test.db-wal");
        let dst = cache_root.join("test.db");
        let rel = Path::new("test.db");

        // Use pre-loaded KdfCache with enc_key for salt [0u8; 16] (the src file's salt).
        // src must be at least page_size (4096) for read_db_salt, but for the WAL patch
        // we use read_main_db_salt_for_path which reads from the .db companion.
        // The WAL's parent DB is "test.db" → salt comes from src's first 16 bytes.
        std::fs::write(&src, [0u8; 4096]).unwrap();
        std::fs::write(&dst, b"cached-db").unwrap();

        // Minimal valid WAL: 32-byte header, zero frames → dispatch_decrypt_wal returns Ok(0)
        let mut wal_header = vec![0u8; 32];
        wal_header[..4].copy_from_slice(&0x377f0682u32.to_be_bytes()); // WAL magic
        wal_header[4..8].copy_from_slice(&3007000u32.to_be_bytes()); // version
        wal_header[8..12].copy_from_slice(&4096u32.to_be_bytes()); // page size
        std::fs::write(&wal, &wal_header).unwrap();

        // Write a wal_failed marker for current mtimes
        write_wal_failed_marker(&src, &wal, &dst).unwrap();
        assert!(is_wal_patch_failed(&src, &wal, &dst));

        // Advance WAL mtime so the marker becomes stale (otherwise guarded_wal_patch skips)
        let new_mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() + Duration::from_secs(2),
        );
        filetime::set_file_mtime(&wal, new_mtime).unwrap();
        assert!(!is_wal_patch_failed(&src, &wal, &dst));

        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            None,
            &[wx_decrypt::EncKeyPair {
                key: [0u8; 32],
                salt: [0u8; 16],
            }],
            &wx_decrypt::MACOS_4_1_7_31,
        );

        let outcome = cache.guarded_wal_patch(&src, &wal, &dst, rel);

        // NoFrames should have cleared the failed marker
        assert!(
            !wal_failed_marker_path(&dst).exists(),
            "wal_failed marker should have been cleared by NoFrames path"
        );
        // Should be NothingToDo (no warnings)
        assert!(
            matches!(outcome, WalOutcome::NothingToDo),
            "expected NothingToDo, got {outcome:?}",
        );
    }

    // --- test helper ---
    fn derive_enc_key(
        raw_key: &[u8; 32],
        salt: &[u8; 16],
        params: &wx_decrypt::CryptoParams,
    ) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(raw_key, salt, params.kdf_iter, &mut key);
        key
    }

    fn build_encrypted_db(
        path: &std::path::Path,
        raw_key: &[u8; 32],
        salt: &[u8; 16],
        params: &wx_decrypt::CryptoParams,
    ) {
        use aes::cipher::{BlockModeEncrypt, KeyIvInit};
        use hmac::{Hmac, Mac};
        use sha2::Sha512;

        let enc_key = derive_enc_key(raw_key, salt, params);
        let mut mac_salt = [0u8; 16];
        for (i, b) in salt.iter().enumerate() {
            mac_salt[i] = b ^ 0x3a;
        }
        let mut mac_key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha512>(&enc_key, &mac_salt, 2, &mut mac_key);

        let iv = [0x42u8; 16];
        let data_size = params.page_size - params.reserve - params.salt_size;
        let plaintext = vec![0u8; data_size];

        let mut ciphertext = plaintext;
        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        Aes256CbcEnc::new((&enc_key).into(), (&iv).into())
            .encrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut ciphertext, data_size)
            .unwrap();

        let mut page = Vec::with_capacity(params.page_size);
        page.extend_from_slice(salt);
        page.extend_from_slice(&ciphertext);
        page.extend_from_slice(&iv);
        page.resize(params.page_size, 0);

        let hmac_data_end = params.page_size - params.reserve + params.iv_size;
        let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(&mac_key).unwrap();
        mac.update(&page[params.salt_size..hmac_data_end]);
        mac.update(&1u32.to_le_bytes());
        let hmac_result = mac.finalize().into_bytes();
        let hmac_start = params.page_size - params.reserve + params.iv_size;
        page[hmac_start..hmac_start + params.hmac_size]
            .copy_from_slice(&hmac_result[..params.hmac_size]);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, &page).unwrap();
    }

    #[test]
    fn ensure_decrypted_with_progress_emits_decrypted_when_files_need_decrypt() {
        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted").join("db_storage");
        let cache_root = tmp.path().join("cache").join("db_storage");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        // Build two encrypted DBs
        let raw_key = [0xABu8; 32];
        let salt = [0x01u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;
        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt, params);
        build_encrypted_db(&enc_root.join("b.db"), &raw_key, &salt, params);

        let enc_key = derive_enc_key(&raw_key, &salt, params);
        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            None,
            &[wx_decrypt::EncKeyPair { key: enc_key, salt }],
            params,
        );

        let events = Mutex::new(Vec::<DecryptProgress>::new());
        let stats = cache
            .ensure_decrypted_with_progress(|e| {
                events.lock().unwrap().push(e);
            })
            .unwrap();
        let events = events.into_inner().unwrap();

        assert_eq!(stats.decrypted, 2);
        let decrypted_count = events
            .iter()
            .filter(|e| matches!(e, DecryptProgress::Decrypted { .. }))
            .count();
        assert_eq!(decrypted_count, 2, "should emit 2 Decrypted events");
        assert_eq!(
            decrypted_count
                + events
                    .iter()
                    .filter(|e| matches!(e, DecryptProgress::Skipped { .. }))
                    .count(),
            2,
            "Decrypted + Skipped should equal total DB count"
        );
    }

    #[test]
    fn ensure_decrypted_with_progress_only_skipped_when_all_cached() {
        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted").join("db_storage");
        let cache_root = tmp.path().join("cache").join("db_storage");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        let raw_key = [0xABu8; 32];
        let salt = [0x01u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;
        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt, params);

        let enc_key = derive_enc_key(&raw_key, &salt, params);
        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            None,
            &[wx_decrypt::EncKeyPair { key: enc_key, salt }],
            params,
        );

        // First run: decrypt everything
        cache.ensure_decrypted().unwrap();

        // Second run with progress: only Skipped events, no Decrypted or Failed
        let events = Mutex::new(Vec::<DecryptProgress>::new());
        let stats = cache
            .ensure_decrypted_with_progress(|e| {
                events.lock().unwrap().push(e);
            })
            .unwrap();
        let events = events.into_inner().unwrap();

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.decrypted, 0);
        assert!(
            events
                .iter()
                .all(|e| matches!(e, DecryptProgress::Skipped { .. })),
            "all events should be Skipped when all cached, got: {events:?}"
        );
        assert_eq!(events.len(), 1, "should emit exactly 1 Skipped event");
    }

    // --- Task 5g integration tests ---

    #[test]
    fn rawkey_path_caches_across_files() {
        // Decrypt two DBs with same raw_key but different salts via RawKey.
        // KdfCache should have 2 entries after, both newly derived.
        let raw_key = [0xABu8; 32];
        let salt1 = [0x01u8; 16];
        let salt2 = [0x02u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt1, params);
        build_encrypted_db(&enc_root.join("b.db"), &raw_key, &salt2, params);

        // No pre-loaded pairs, only raw_key → all derivations go through get_or_derive
        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            Some(raw_key),
            &[],
            params,
        );

        cache
            .decrypt_db(&enc_root.join("a.db"), &cache_root.join("a.db"))
            .unwrap();
        cache
            .decrypt_db(&enc_root.join("b.db"), &cache_root.join("b.db"))
            .unwrap();

        let guard = cache.kdf_cache.lock().unwrap();
        assert!(guard.has_new_derivations());
        assert_eq!(
            guard.new_pairs().len(),
            2,
            "two unique salts should be derived"
        );
        assert_eq!(guard.all_pairs().len(), 2);
    }

    #[test]
    fn enc_keys_miss_falls_back_to_raw_key() {
        // Pre-load KdfCache with enc_key for salt1 only.
        // Decrypt a DB with salt2 → should derive via raw_key fallback.
        let raw_key = [0xABu8; 32];
        let salt1 = [0x01u8; 16];
        let salt2 = [0x02u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let enc_key1 = derive_enc_key(&raw_key, &salt1, params);

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        build_encrypted_db(&enc_root.join("b.db"), &raw_key, &salt2, params);

        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            Some(raw_key),
            &[wx_decrypt::EncKeyPair {
                key: enc_key1,
                salt: salt1,
            }],
            params,
        );

        // Decrypt DB with salt2 (not in pre-loaded pairs) → should succeed via raw_key
        cache
            .decrypt_db(&enc_root.join("b.db"), &cache_root.join("b.db"))
            .unwrap();

        let guard = cache.kdf_cache.lock().unwrap();
        assert!(guard.has_new_derivations(), "salt2 should be newly derived");
        assert_eq!(guard.new_pairs().len(), 1);
        assert_eq!(guard.all_pairs().len(), 2, "pre-loaded + newly derived");
    }

    #[test]
    fn stale_enc_key_triggers_raw_key_fallback() {
        // Pre-load KdfCache with an INCORRECT enc_key for salt1.
        // With raw_key present, decrypt_db should retry with fresh derivation.
        let raw_key = [0xABu8; 32];
        let salt1 = [0x01u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt1, params);

        // Pre-load with wrong enc_key for this salt
        let wrong_enc_key = [0xFFu8; 32];
        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            Some(raw_key),
            &[wx_decrypt::EncKeyPair {
                key: wrong_enc_key,
                salt: salt1,
            }],
            params,
        );

        // Should succeed — stale enc_key triggers re-derive from raw_key
        cache
            .decrypt_db(&enc_root.join("a.db"), &cache_root.join("a.db"))
            .unwrap();

        let guard = cache.kdf_cache.lock().unwrap();
        // The stale entry should have been refreshed
        let cached = guard.lookup(&salt1).unwrap();
        let correct = derive_enc_key(&raw_key, &salt1, params);
        assert_eq!(cached, correct, "cache should now have the correct enc_key");
    }

    #[test]
    fn newly_derived_pairs_tracked_in_kdf_cache() {
        // After RawKey derivation, has_new_derivations should be true
        // and new_pairs should contain the derived entry.
        let raw_key = [0xABu8; 32];
        let salt = [0x01u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt, params);

        let cache = test_cache(
            cache_root.clone(),
            enc_root.clone(),
            Some(raw_key),
            &[],
            params,
        );

        cache
            .decrypt_db(&enc_root.join("a.db"), &cache_root.join("a.db"))
            .unwrap();

        let guard = cache.kdf_cache.lock().unwrap();
        assert!(guard.has_new_derivations());
        let pairs = guard.new_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].salt, salt);
        let expected_key = derive_enc_key(&raw_key, &salt, params);
        assert_eq!(pairs[0].key, expected_key);
    }

    #[test]
    fn cli_key_flag_does_not_trigger_writeback() {
        // When writeback_enabled is false, writeback_enc_keys should be a no-op.
        let raw_key = [0xABu8; 32];
        let salt = [0x01u8; 16];
        let params = &wx_decrypt::MACOS_4_1_7_31;

        let tmp = TempDir::new().unwrap();
        let enc_root = tmp.path().join("encrypted");
        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&enc_root).unwrap();
        std::fs::create_dir_all(&cache_root).unwrap();

        build_encrypted_db(&enc_root.join("a.db"), &raw_key, &salt, params);

        let cache = PersistentCache {
            cache_root: cache_root.clone(),
            encrypted_root: enc_root.clone(),
            raw_key: Some(raw_key),
            account_id: "wxid_test_cli".into(),
            base_wxid: "wxid_test_cli".into(),
            writeback_enabled: false, // CLI --key flag
            kdf_cache: Mutex::new(KdfCache::empty()),
            params,
            file_locks: Arc::new(FileLockMap::default()),
        };

        cache
            .decrypt_db(&enc_root.join("a.db"), &cache_root.join("a.db"))
            .unwrap();

        // writeback should be a no-op (returns Ok immediately)
        assert!(cache.writeback_enc_keys().is_ok());
        // The KdfCache has new derivations, but writeback_enabled is false
        let guard = cache.kdf_cache.lock().unwrap();
        assert!(guard.has_new_derivations(), "derivation happened");
    }
}
