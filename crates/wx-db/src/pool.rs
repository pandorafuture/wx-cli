use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::error::DbError;
use crate::open::{MessageShard, SqlcipherKey};

pub(crate) type FtsInitFn = dyn Fn(&Connection) -> Result<(), String> + Send + Sync;

/// Pre-opened connection pool for message shards and FTS.
///
/// All connections are opened at construction time and held persistently.
/// Use [`ShardPool::reopen_all`] to close and reopen everything after
/// a background decrypt cycle.
pub struct ShardPool {
    conns: HashMap<PathBuf, Connection>,
    fts_conn: Option<Connection>,
    fts_path: Option<PathBuf>,
    fts_init: Option<Arc<FtsInitFn>>,
    sqlcipher_key: Option<SqlcipherKey>,
}

impl std::fmt::Debug for ShardPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardPool")
            .field("shard_count", &self.conns.len())
            .field("has_fts", &self.fts_conn.is_some())
            .finish()
    }
}

impl ShardPool {
    /// Open all shard connections and optionally an FTS connection.
    ///
    /// `fts_init` is called on the FTS connection after opening to register
    /// custom tokenizers. The `Arc` is stored internally so that `reopen_fts()`
    /// and `reopen_all()` can re-invoke it without the caller passing it again.
    pub(crate) fn open(
        shards: &[MessageShard],
        fts_path: Option<&Path>,
        fts_init: Option<Arc<FtsInitFn>>,
        sqlcipher_key: Option<SqlcipherKey>,
    ) -> Result<Self, DbError> {
        let mut conns = HashMap::with_capacity(shards.len());
        for shard in shards {
            let conn = crate::open::open_connection(&shard.path, sqlcipher_key.as_ref())?;
            conns.insert(shard.path.clone(), conn);
        }

        let fts_conn = match (fts_path, &fts_init) {
            (Some(path), Some(init)) => {
                let conn = crate::open::open_connection(path, sqlcipher_key.as_ref())?;
                init(&conn).map_err(DbError::FtsInit)?;
                Some(conn)
            }
            (Some(path), None) => {
                let conn = crate::open::open_connection(path, sqlcipher_key.as_ref())?;
                Some(conn)
            }
            _ => None,
        };

        Ok(ShardPool {
            conns,
            fts_conn,
            fts_path: fts_path.map(|p| p.to_path_buf()),
            fts_init,
            sqlcipher_key,
        })
    }

    /// Borrow a shard connection by path.
    pub fn get(&self, path: &Path) -> Option<&Connection> {
        self.conns.get(path)
    }

    /// Close and reopen one shard connection.
    pub fn reopen_shard(&mut self, path: &Path) -> Result<(), DbError> {
        if self.conns.contains_key(path) {
            let conn = crate::open::open_connection(path, self.sqlcipher_key.as_ref())?;
            self.conns.insert(path.to_path_buf(), conn);
        }
        Ok(())
    }

    /// Borrow the FTS connection.
    pub fn fts_conn(&self) -> Option<&Connection> {
        self.fts_conn.as_ref()
    }

    /// Close and reopen the FTS connection, re-registering the tokenizer.
    pub fn reopen_fts(&mut self) -> Result<(), DbError> {
        if let Some(path) = &self.fts_path {
            let conn = crate::open::open_connection(path, self.sqlcipher_key.as_ref())?;
            if let Some(init) = &self.fts_init {
                init(&conn).map_err(DbError::FtsInit)?;
            }
            self.fts_conn = Some(conn);
        }
        Ok(())
    }

    /// Close and reopen all shard connections and FTS.
    pub fn reopen_all(&mut self) -> Result<(), DbError> {
        let paths: Vec<PathBuf> = self.conns.keys().cloned().collect();
        for path in paths {
            let conn = crate::open::open_connection(&path, self.sqlcipher_key.as_ref())?;
            self.conns.insert(path, conn);
        }
        self.reopen_fts()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_shard(dir: &Path, name: &str) -> MessageShard {
        let path = dir.join(name);
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE test (id INTEGER PRIMARY KEY)")
            .unwrap();
        MessageShard {
            path,
            start_unix: 0,
            end_unix: i64::MAX,
        }
    }

    #[test]
    fn open_and_get_connections() {
        let tmp = TempDir::new().unwrap();
        let s1 = create_test_shard(tmp.path(), "message_0.db");
        let s2 = create_test_shard(tmp.path(), "message_1.db");
        let shards = vec![s1, s2];

        let pool = ShardPool::open(&shards, None, None, None).unwrap();

        assert!(pool.get(&shards[0].path).is_some());
        assert!(pool.get(&shards[1].path).is_some());
        assert!(pool.get(Path::new("/nonexistent")).is_none());
        assert!(pool.fts_conn().is_none());
    }

    #[test]
    fn reopen_shard_picks_up_changes() {
        let tmp = TempDir::new().unwrap();
        let shard = create_test_shard(tmp.path(), "message_0.db");
        let shards = vec![shard];

        let mut pool = ShardPool::open(&shards, None, None, None).unwrap();

        // Write new data outside the pool
        {
            let ext_conn = Connection::open(&shards[0].path).unwrap();
            ext_conn
                .execute("INSERT INTO test (id) VALUES (42)", [])
                .unwrap();
        }

        // Before reopen, read-only conn may or may not see it (WAL mode).
        // After reopen, it must see it.
        pool.reopen_shard(&shards[0].path).unwrap();
        let conn = pool.get(&shards[0].path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM test", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn reopen_all_reopens_everything() {
        let tmp = TempDir::new().unwrap();
        let s1 = create_test_shard(tmp.path(), "message_0.db");
        let s2 = create_test_shard(tmp.path(), "message_1.db");
        let shards = vec![s1, s2];

        let mut pool = ShardPool::open(&shards, None, None, None).unwrap();
        pool.reopen_all().unwrap();

        assert!(pool.get(&shards[0].path).is_some());
        assert!(pool.get(&shards[1].path).is_some());
    }

    #[test]
    fn fts_connection_with_init() {
        let tmp = TempDir::new().unwrap();
        let fts_path = tmp.path().join("message_fts.db");
        Connection::open(&fts_path)
            .unwrap()
            .execute_batch("CREATE TABLE fts_test (id INTEGER)")
            .unwrap();

        let init_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let init_called_clone = Arc::clone(&init_called);
        let fts_init: Arc<FtsInitFn> = Arc::new(move |_conn| {
            init_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        let pool = ShardPool::open(&[], Some(&fts_path), Some(fts_init), None).unwrap();
        assert!(pool.fts_conn().is_some());
        assert!(init_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn reopen_fts_reinvokes_init() {
        let tmp = TempDir::new().unwrap();
        let fts_path = tmp.path().join("message_fts.db");
        Connection::open(&fts_path).unwrap();

        let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = Arc::clone(&call_count);
        let fts_init: Arc<FtsInitFn> = Arc::new(move |_conn| {
            call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        let mut pool = ShardPool::open(&[], Some(&fts_path), Some(fts_init), None).unwrap();
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        pool.reopen_fts().unwrap();
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn reopen_fts_after_file_deleted() {
        let tmp = TempDir::new().unwrap();
        let fts_path = tmp.path().join("message_fts.db");
        Connection::open(&fts_path).unwrap();

        let fts_init: Arc<FtsInitFn> = Arc::new(|_conn| Ok(()));
        let mut pool = ShardPool::open(&[], Some(&fts_path), Some(fts_init), None).unwrap();
        assert!(pool.fts_conn().is_some());

        // Delete the FTS file
        std::fs::remove_file(&fts_path).unwrap();

        // reopen_fts should fail because the file no longer exists
        let result = pool.reopen_fts();
        assert!(
            result.is_err(),
            "reopen_fts should fail when file is deleted"
        );
    }

    #[test]
    fn reopen_fts_after_file_replaced() {
        let tmp = TempDir::new().unwrap();
        let fts_path = tmp.path().join("message_fts.db");

        // Create original FTS file with a marker table
        {
            let conn = Connection::open(&fts_path).unwrap();
            conn.execute_batch("CREATE TABLE marker (id INTEGER)")
                .unwrap();
        }

        let fts_init: Arc<FtsInitFn> = Arc::new(|_conn| Ok(()));
        let mut pool = ShardPool::open(&[], Some(&fts_path), Some(fts_init), None).unwrap();
        assert!(pool.fts_conn().is_some());

        // Replace the file with a new one containing a different table
        std::fs::remove_file(&fts_path).unwrap();
        {
            let conn = Connection::open(&fts_path).unwrap();
            conn.execute_batch("CREATE TABLE replaced_marker (id INTEGER)")
                .unwrap();
        }

        // reopen_fts should succeed and connect to the new file
        pool.reopen_fts().unwrap();
        let conn = pool.fts_conn().unwrap();

        // Verify we see the replaced_marker table (proving we connected to the new file)
        let has_replaced: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='replaced_marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            has_replaced,
            "reopened connection should see replaced_marker table"
        );
    }
}
