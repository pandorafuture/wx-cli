use std::collections::HashMap;
use std::fmt;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use rusqlite::Connection;

use crate::error::DbError;
use crate::pool::ShardPool;
use crate::shard_metadata::{now_nanos, ShardMeta, ShardMetadataFile};

/// Metadata for a single message shard database file.
#[derive(Debug)]
pub(crate) struct MessageShard {
    pub path: PathBuf,
    pub start_unix: i64,
    pub end_unix: i64,
}

/// A raw WeChat key plus an in-process cache of SQLCipher's derived keys.
///
/// SQLCipher normally runs its 256k-round PBKDF2 every time a connection is
/// opened. WeChat uses a different salt per database, but the same database is
/// often opened several times during one command (metadata scan, query, count,
/// refresh). Passing SQLCipher's raw keyspec lets us derive once per salt and
/// reuse the result for every subsequent connection.
#[derive(Clone)]
pub(crate) struct SqlcipherKey {
    raw_key: [u8; 32],
    derived_keys: Arc<Mutex<HashMap<[u8; 16], CachedKey>>>,
}

#[derive(Clone, Copy)]
struct CachedKey {
    key: [u8; 32],
    /// Preloaded keys come from the persisted key store and get one raw-key
    /// fallback if validation fails. Keys derived in this process are trusted.
    preloaded: bool,
}

impl SqlcipherKey {
    fn new(raw_key: [u8; 32]) -> Self {
        Self::with_preloaded(raw_key, &[])
    }

    fn with_preloaded(raw_key: [u8; 32], pairs: &[wx_decrypt::EncKeyPair]) -> Self {
        let derived_keys = pairs
            .iter()
            .map(|pair| {
                (
                    pair.salt,
                    CachedKey {
                        key: pair.key,
                        preloaded: true,
                    },
                )
            })
            .collect();
        Self {
            raw_key,
            derived_keys: Arc::new(Mutex::new(derived_keys)),
        }
    }

    fn keyspec_for_path(&self, path: &Path) -> Result<(Vec<u8>, [u8; 16], bool), DbError> {
        let salt = wx_decrypt::read_db_salt(path)
            .map_err(|e| DbError::EncryptionKey(format!("failed to read database salt: {e}")))?;

        let cached = {
            let mut cache = self.derived_keys.lock().map_err(|_| {
                DbError::EncryptionKey("derived-key cache lock was poisoned".into())
            })?;
            *cache.entry(salt).or_insert_with(|| {
                let key = wx_decrypt::kdf::derive_enc_key(
                    &self.raw_key,
                    &salt,
                    &wx_decrypt::MACOS_4_1_7_31,
                );
                CachedKey {
                    key,
                    preloaded: false,
                }
            })
        };

        // SQLCipher raw-key syntax includes the original 16-byte database salt.
        // Supplying this ASCII keyspec to sqlite3_key() skips SQLCipher's PBKDF2.
        let keyspec = format!("x'{}{}'", hex::encode(cached.key), hex::encode(salt)).into_bytes();
        Ok((keyspec, salt, cached.preloaded))
    }

    fn mark_verified(&self, salt: [u8; 16]) -> Result<(), DbError> {
        let mut cache = self
            .derived_keys
            .lock()
            .map_err(|_| DbError::EncryptionKey("derived-key cache lock was poisoned".into()))?;
        if let Some(entry) = cache.get_mut(&salt) {
            entry.preloaded = false;
        }
        Ok(())
    }

    fn rederive_keyspec(&self, salt: [u8; 16]) -> Result<Vec<u8>, DbError> {
        let key =
            wx_decrypt::kdf::derive_enc_key(&self.raw_key, &salt, &wx_decrypt::MACOS_4_1_7_31);
        self.derived_keys
            .lock()
            .map_err(|_| DbError::EncryptionKey("derived-key cache lock was poisoned".into()))?
            .insert(
                salt,
                CachedKey {
                    key,
                    preloaded: false,
                },
            );
        Ok(format!("x'{}{}'", hex::encode(key), hex::encode(salt)).into_bytes())
    }

    #[cfg(test)]
    fn cached_salt_count(&self) -> usize {
        self.derived_keys.lock().unwrap().len()
    }
}

/// Handle to an opened (decrypted) WeChat database directory.
///
/// Holds connections to contact/session databases and metadata about
/// message shard files. Created via [`WechatDb::open`].
pub struct WechatDb {
    pub(crate) contact_conn: Connection,
    pub(crate) contact_path: PathBuf,
    pub(crate) session_conn: Connection,
    pub(crate) session_path: PathBuf,
    pub(crate) shards: Vec<MessageShard>,
    /// Path to `message/message_fts.db` if it exists.
    pub message_fts_path: Option<PathBuf>,
    /// Path to `message/contact_fts.db` if it exists.
    pub contact_fts_path: Option<PathBuf>,
    /// Optional pre-opened connection pool for serve mode.
    pub(crate) pool: Option<ShardPool>,
    /// Shared raw/derived key state for encrypted direct open and reopen operations.
    pub(crate) sqlcipher_key: Option<SqlcipherKey>,
    /// Lazily initialized cache of label_id -> label_name from contact_label table.
    /// Cleared on `reopen_contacts()` so label changes are visible.
    pub(crate) label_cache: RwLock<Option<HashMap<String, String>>>,
}

impl fmt::Debug for WechatDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WechatDb")
            .field("shards", &self.shards)
            .finish_non_exhaustive()
    }
}

/// Open a read-only connection, optionally applying sqlite3_key for encrypted DBs.
pub fn open_readonly_connection(
    path: &Path,
    raw_key: Option<&[u8; 32]>,
) -> Result<Connection, DbError> {
    let key = raw_key.copied().map(SqlcipherKey::new);
    open_connection(path, key.as_ref())
}

pub(crate) fn open_connection(
    path: &Path,
    sqlcipher_key: Option<&SqlcipherKey>,
) -> Result<Connection, DbError> {
    if let Some(key) = sqlcipher_key {
        let (keyspec, salt, preloaded) = key.keyspec_for_path(path)?;
        match open_connection_with_keyspec(path, &keyspec) {
            Ok(conn) => {
                if preloaded {
                    key.mark_verified(salt)?;
                }
                Ok(conn)
            }
            Err(DbError::EncryptionKey(_)) if preloaded => {
                // Persisted entries are an optimization, never a single point of
                // failure. Re-derive once from the raw key if an entry is stale.
                let keyspec = key.rederive_keyspec(salt)?;
                open_connection_with_keyspec(path, &keyspec)
            }
            Err(err) => Err(err),
        }
    } else {
        Ok(Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    }
}

fn open_connection_with_keyspec(path: &Path, keyspec: &[u8]) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    unsafe {
        let rc = rusqlite::ffi::sqlite3_key(
            conn.handle(),
            keyspec.as_ptr() as *const c_void,
            keyspec.len() as i32,
        );
        if rc != 0 {
            return Err(DbError::EncryptionKey(format!(
                "sqlite3_key failed: rc={rc}"
            )));
        }
    }
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|_| DbError::EncryptionKey("incorrect key or not an encrypted database".into()))?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    Ok(conn)
}

impl WechatDb {
    /// Open a decrypted WeChat database directory.
    ///
    /// Returns `DbError::NotFound` if the path, contact.db, or session.db
    /// does not exist. Message shards are optional here; message queries will
    /// return `DbError::NoShards` if no numbered shard is available.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_internal(path.as_ref(), None, true)
    }

    /// Open only contact.db and session.db, without scanning message shards.
    /// Useful for contacts, sessions, and monitoring commands that never read messages.
    pub fn open_core(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_internal(path.as_ref(), None, false)
    }

    /// Open a decrypted WeChat database directory with a pre-opened
    /// connection pool for all message shards and FTS.
    ///
    /// `fts_init` is called on the FTS connection to register custom
    /// tokenizers (e.g. `register_mm_fts_tokenizer`).
    pub fn open_with_pool(
        path: impl AsRef<Path>,
        fts_init: impl Fn(&Connection) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Self, DbError> {
        Self::open_with_pool_internal(path, None, fts_init)
    }

    /// Open an encrypted WeChat database directory directly using `sqlite3_key()`.
    pub fn open_encrypted(path: impl AsRef<Path>, raw_key: [u8; 32]) -> Result<Self, DbError> {
        Self::open_internal(path.as_ref(), Some(SqlcipherKey::new(raw_key)), true)
    }

    /// Open an encrypted directory and seed the per-salt cache with persisted
    /// derived keys, falling back to the raw key for missing or stale entries.
    pub fn open_encrypted_with_key_cache(
        path: impl AsRef<Path>,
        raw_key: [u8; 32],
        pairs: &[wx_decrypt::EncKeyPair],
    ) -> Result<Self, DbError> {
        Self::open_internal(
            path.as_ref(),
            Some(SqlcipherKey::with_preloaded(raw_key, pairs)),
            true,
        )
    }

    /// Open only encrypted contact.db and session.db, without scanning message shards.
    pub fn open_encrypted_core(path: impl AsRef<Path>, raw_key: [u8; 32]) -> Result<Self, DbError> {
        Self::open_internal(path.as_ref(), Some(SqlcipherKey::new(raw_key)), false)
    }

    /// Core-only variant of [`WechatDb::open_encrypted_with_key_cache`].
    pub fn open_encrypted_core_with_key_cache(
        path: impl AsRef<Path>,
        raw_key: [u8; 32],
        pairs: &[wx_decrypt::EncKeyPair],
    ) -> Result<Self, DbError> {
        Self::open_internal(
            path.as_ref(),
            Some(SqlcipherKey::with_preloaded(raw_key, pairs)),
            false,
        )
    }

    /// Open an encrypted WeChat database directory with a pre-opened
    /// connection pool for all message shards and FTS.
    pub fn open_encrypted_with_pool(
        path: impl AsRef<Path>,
        raw_key: [u8; 32],
        fts_init: impl Fn(&Connection) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Self, DbError> {
        Self::open_with_pool_internal(path, Some(SqlcipherKey::new(raw_key)), fts_init)
    }

    /// Pool variant seeded with persisted per-salt derived keys.
    pub fn open_encrypted_with_pool_and_key_cache(
        path: impl AsRef<Path>,
        raw_key: [u8; 32],
        pairs: &[wx_decrypt::EncKeyPair],
        fts_init: impl Fn(&Connection) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Self, DbError> {
        Self::open_with_pool_internal(
            path,
            Some(SqlcipherKey::with_preloaded(raw_key, pairs)),
            fts_init,
        )
    }

    fn open_internal(
        path: &Path,
        sqlcipher_key: Option<SqlcipherKey>,
        scan_message_shards: bool,
    ) -> Result<Self, DbError> {
        if !path.exists() {
            return Err(DbError::NotFound(path.display().to_string()));
        }

        let key_ref = sqlcipher_key.as_ref();

        // Open contact.db
        let contact_path = path.join("contact").join("contact.db");
        if !contact_path.exists() {
            return Err(DbError::NotFound(contact_path.display().to_string()));
        }
        let contact_conn = open_connection(&contact_path, key_ref)?;

        // Open session.db
        let session_path = path.join("session").join("session.db");
        if !session_path.exists() {
            return Err(DbError::NotFound(session_path.display().to_string()));
        }
        let session_conn = open_connection(&session_path, key_ref)?;

        // Scan message shards
        let msg_dir = path.join("message");
        let mut shards = Vec::new();

        if scan_message_shards && msg_dir.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&msg_dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| is_numbered_message_shard(p))
                .collect();
            entries.sort();

            for shard_path in entries {
                let start_unix = read_shard_timestamp(&shard_path, key_ref);
                shards.push(MessageShard {
                    path: shard_path,
                    start_unix,
                    end_unix: 0, // assigned below
                });
            }
        }

        // Sort shards by start_unix ASC
        shards.sort_by_key(|s| s.start_unix);

        // Assign end_unix: each shard ends at next shard's start - 1; last = i64::MAX
        let n = shards.len();
        for i in 0..n {
            if i + 1 < n {
                shards[i].end_unix = shards[i + 1].start_unix - 1;
            } else {
                shards[i].end_unix = i64::MAX;
            }
        }

        Ok(WechatDb {
            contact_conn,
            contact_path,
            session_conn,
            session_path,
            shards,
            message_fts_path: {
                let p = msg_dir.join("message_fts.db");
                if p.exists() {
                    Some(p)
                } else {
                    None
                }
            },
            contact_fts_path: {
                let p = msg_dir.join("contact_fts.db");
                if p.exists() {
                    Some(p)
                } else {
                    None
                }
            },
            pool: None,
            sqlcipher_key,
            label_cache: RwLock::new(None),
        })
    }

    fn open_with_pool_internal(
        path: impl AsRef<Path>,
        sqlcipher_key: Option<SqlcipherKey>,
        fts_init: impl Fn(&Connection) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Self, DbError> {
        let mut db = Self::open_internal(path.as_ref(), sqlcipher_key.clone(), true)?;
        let fts_init_arc: Arc<crate::pool::FtsInitFn> = Arc::new(fts_init);
        let pool = ShardPool::open(
            &db.shards,
            db.message_fts_path.as_deref(),
            Some(fts_init_arc),
            sqlcipher_key,
        )?;
        db.pool = Some(pool);
        Ok(db)
    }

    /// Re-open the session.db connection to pick up external changes.
    pub fn reopen_sessions(&mut self) -> Result<(), DbError> {
        self.session_conn = open_connection(&self.session_path, self.sqlcipher_key.as_ref())?;
        Ok(())
    }

    /// Re-open the contact.db connection to pick up external changes.
    /// Also invalidates the label cache so it is reloaded on next query.
    pub fn reopen_contacts(&mut self) -> Result<(), DbError> {
        self.contact_conn = open_connection(&self.contact_path, self.sqlcipher_key.as_ref())?;
        *self.label_cache.write().unwrap() = None;
        Ok(())
    }

    /// Reopen a specific pooled shard connection.
    /// Returns `Ok(true)` if the path was in the pool (and reopened),
    /// `Ok(false)` if the path was not in the pool (unknown shard — possible topology change).
    /// No-op if pool is not initialized (returns `Ok(false)`).
    pub fn reopen_pooled_shard(&mut self, path: &Path) -> Result<bool, DbError> {
        if let Some(pool) = &mut self.pool {
            if pool.get(path).is_some() {
                pool.reopen_shard(path)?;
                return Ok(true);
            }
            return Ok(false);
        }
        Ok(false)
    }

    /// Reopen all pooled connections (shards + FTS).
    /// No-op if pool is not initialized.
    pub fn reopen_all_pooled(&mut self) -> Result<(), DbError> {
        if let Some(pool) = &mut self.pool {
            pool.reopen_all()?;
        }
        Ok(())
    }

    /// Reopen only the FTS connection in the pool.
    /// No-op if pool is not initialized.
    pub fn reopen_fts(&mut self) -> Result<(), DbError> {
        if let Some(pool) = &mut self.pool {
            pool.reopen_fts()?;
        }
        Ok(())
    }

    /// Borrow the connection pool, if initialized.
    pub fn pool(&self) -> Option<&ShardPool> {
        self.pool.as_ref()
    }

    /// Open another database from the same encrypted account while reusing this
    /// handle's derived-key cache. This is used by serve-mode auxiliary FTS and
    /// media connections so refreshes do not re-run PBKDF2.
    pub fn open_related_readonly(&self, path: &Path) -> Result<Connection, DbError> {
        open_connection(path, self.sqlcipher_key.as_ref())
    }

    /// Return shards whose time range overlaps `[start, end]`.
    pub(crate) fn shards_for_range(&self, start: i64, end: i64) -> Vec<&MessageShard> {
        self.shards
            .iter()
            .filter(|s| s.start_unix <= end && s.end_unix >= start)
            .collect()
    }

    /// Return all message shards (for full-shard scan in anchor queries).
    pub(crate) fn all_shards(&self) -> &[MessageShard] {
        &self.shards
    }

    /// Build a `ShardMetadataFile` from the current shard metadata.
    /// Callers can persist this to a sidecar file for future routing.
    pub fn shard_metadata(&self) -> ShardMetadataFile {
        let shards = self
            .shards
            .iter()
            .filter_map(|s| {
                let shard_id = extract_shard_id(&s.path)?;
                Some(ShardMeta {
                    shard_id,
                    start_unix: s.start_unix,
                    end_unix: s.end_unix,
                })
            })
            .collect();
        ShardMetadataFile {
            shards,
            written_at_ns: now_nanos(),
        }
    }

    /// Open a SQLite connection to a specific shard, optionally encrypted.
    pub(crate) fn open_shard_with_key(
        shard: &MessageShard,
        sqlcipher_key: Option<&SqlcipherKey>,
    ) -> Result<Connection, DbError> {
        open_connection(&shard.path, sqlcipher_key)
    }
}

/// Extract the numeric shard ID from a path like `message_N.db`.
fn extract_shard_id(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("message_")?;
    suffix.parse().ok()
}

fn is_numbered_message_shard(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    if ext != "db" {
        return false;
    }

    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };

    let Some(suffix) = stem.strip_prefix("message_") else {
        return false;
    };

    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
}

/// Try to read the timestamp from a message shard's Timestamp table.
/// Returns 0 if the table does not exist or is empty.
fn read_shard_timestamp(path: &Path, sqlcipher_key: Option<&SqlcipherKey>) -> i64 {
    let conn = match open_connection(path, sqlcipher_key) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    conn.query_row("SELECT timestamp FROM Timestamp LIMIT 1", [], |row| {
        row.get(0)
    })
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_void;
    use tempfile::TempDir;

    /// Create an encrypted SQLite DB at `path` using SQLCipher's `sqlite3_key()`.
    fn create_encrypted_db(path: &Path, raw_key: &[u8; 32], setup_sql: &str) {
        let conn = Connection::open(path).unwrap();
        unsafe {
            let rc =
                rusqlite::ffi::sqlite3_key(conn.handle(), raw_key.as_ptr() as *const c_void, 32);
            assert_eq!(rc, 0, "sqlite3_key failed during test DB creation");
        }
        conn.execute_batch(setup_sql).unwrap();
    }

    /// Build a minimal encrypted db_storage directory for `open_encrypted` tests.
    fn build_encrypted_db_storage(root: &Path, raw_key: &[u8; 32]) {
        std::fs::create_dir_all(root.join("contact")).unwrap();
        std::fs::create_dir_all(root.join("session")).unwrap();
        std::fs::create_dir_all(root.join("message")).unwrap();

        create_encrypted_db(
            &root.join("contact").join("contact.db"),
            raw_key,
            "CREATE TABLE contact (username TEXT PRIMARY KEY, alias TEXT, remark TEXT, nick_name TEXT, description TEXT, extra_buffer BLOB);",
        );
        create_encrypted_db(
            &root.join("session").join("session.db"),
            raw_key,
            "CREATE TABLE SessionTable (username TEXT, sort_timestamp INTEGER, summary TEXT);",
        );
        create_encrypted_db(
            &root.join("message").join("message_0.db"),
            raw_key,
            "CREATE TABLE Timestamp (timestamp INTEGER); INSERT INTO Timestamp VALUES (1700000000);",
        );
    }

    #[test]
    fn open_encrypted_succeeds_with_correct_key() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("db_storage");
        let raw_key = [0xAB_u8; 32];
        build_encrypted_db_storage(&root, &raw_key);

        let db = WechatDb::open_encrypted(&root, raw_key).unwrap();
        assert_eq!(db.shards.len(), 1);
        assert_eq!(db.shards[0].start_unix, 1700000000);
    }

    #[test]
    fn open_encrypted_fails_with_wrong_key() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("db_storage");
        let raw_key = [0xAB_u8; 32];
        build_encrypted_db_storage(&root, &raw_key);

        let wrong_key = [0xCD_u8; 32];
        let result = WechatDb::open_encrypted(&root, wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn open_encrypted_reopen_sessions_works() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("db_storage");
        let raw_key = [0xAB_u8; 32];
        build_encrypted_db_storage(&root, &raw_key);

        let mut db = WechatDb::open_encrypted(&root, raw_key).unwrap();
        let key = db.sqlcipher_key.clone().unwrap();
        let cached_before = key.cached_salt_count();
        assert_eq!(
            cached_before, 3,
            "contact, session, and message salts cached"
        );
        // Reopen should succeed (re-applies sqlite3_key)
        db.reopen_sessions().unwrap();
        db.reopen_contacts().unwrap();
        assert_eq!(
            key.cached_salt_count(),
            cached_before,
            "reopen must reuse derived keys instead of deriving again"
        );
    }

    #[test]
    fn open_core_does_not_scan_message_shards() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("db_storage");
        std::fs::create_dir_all(root.join("contact")).unwrap();
        std::fs::create_dir_all(root.join("session")).unwrap();
        std::fs::create_dir_all(root.join("message")).unwrap();
        Connection::open(root.join("contact/contact.db")).unwrap();
        Connection::open(root.join("session/session.db")).unwrap();
        std::fs::write(root.join("message/message_0.db"), b"not a sqlite database").unwrap();

        let db = WechatDb::open_core(&root).unwrap();
        assert!(db.shards.is_empty());
    }

    #[test]
    fn open_connection_plaintext_works() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (id INTEGER)")
            .unwrap();

        let conn = open_connection(&path, None).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn open_connection_encrypted_works() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("enc.db");
        let raw_key = [0xAB_u8; 32];
        create_encrypted_db(
            &path,
            &raw_key,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (42);",
        );

        let key = SqlcipherKey::new(raw_key);
        let conn = open_connection(&path, Some(&key)).unwrap();
        let val: i64 = conn
            .query_row("SELECT id FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn preloaded_derived_key_opens_encrypted_database() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("enc.db");
        let raw_key = [0xAB_u8; 32];
        create_encrypted_db(
            &path,
            &raw_key,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (42);",
        );
        let salt = wx_decrypt::read_db_salt(&path).unwrap();
        let enc_key = wx_decrypt::kdf::derive_enc_key(&raw_key, &salt, &wx_decrypt::MACOS_4_1_7_31);
        let key =
            SqlcipherKey::with_preloaded(raw_key, &[wx_decrypt::EncKeyPair { key: enc_key, salt }]);

        let conn = open_connection(&path, Some(&key)).unwrap();
        let val: i64 = conn
            .query_row("SELECT id FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(val, 42);
        assert_eq!(key.cached_salt_count(), 1);
    }

    #[test]
    fn stale_preloaded_key_falls_back_to_raw_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("enc.db");
        let raw_key = [0xAB_u8; 32];
        create_encrypted_db(
            &path,
            &raw_key,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (42);",
        );
        let salt = wx_decrypt::read_db_salt(&path).unwrap();
        let key = SqlcipherKey::with_preloaded(
            raw_key,
            &[wx_decrypt::EncKeyPair {
                key: [0xCD; 32],
                salt,
            }],
        );

        let conn = open_connection(&path, Some(&key)).unwrap();
        let val: i64 = conn
            .query_row("SELECT id FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(val, 42);
    }
}
