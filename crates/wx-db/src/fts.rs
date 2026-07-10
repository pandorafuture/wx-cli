use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::Serialize;

use crate::decode::{check_column_exists, decode_content, msg_table_name, parse_content, parse_group_sender};
use crate::error::DbError;
use crate::model::{split_local_type, MessageContent};
use crate::open::WechatDb;

// ---------------------------------------------------------------------------
// CJK tokenization
// ---------------------------------------------------------------------------

/// Returns true if the character is in a CJK range that should be space-split.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{303F}'   // CJK punctuation
        | '\u{3400}'..='\u{9FFF}' // CJK Unified + Ext A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{FF00}'..='\u{FFEF}' // Fullwidth Forms
        | '\u{20000}'..='\u{2FA1F}' // CJK Ext B-F + Compat Supplement
    )
}

/// Space-split CJK characters while leaving non-CJK text intact.
///
/// Used both at index-build time (INSERT) and query time (MATCH) to ensure
/// symmetric tokenization.
///
/// ```text
/// "你好world 测试" → "你 好 world 测 试"
/// ```
pub(crate) fn cjk_tokenize(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for c in input.chars() {
        if is_cjk(c) {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push(c);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    // Trim trailing space added by CJK characters
    let trimmed = out.trim_end();
    trimmed.to_string()
}

/// Convert a user keyword into an FTS5 MATCH expression.
///
/// - Split by whitespace into tokens
/// - Each token is `cjk_tokenize`-d, then wrapped in double quotes (phrase query)
/// - Internal double quotes are escaped as `""`
/// - Multiple tokens joined with AND
///
/// ```text
/// "你好 world"      → "你 好" AND "world"
/// "测试"            → "测 试"
/// 'he said "hi"'    → "he" AND "said" AND """hi"""
/// ```
pub(crate) fn build_fts_query(keyword: &str) -> String {
    let tokens: Vec<&str> = keyword.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }

    let phrases: Vec<String> = tokens
        .into_iter()
        .map(|tok| {
            let tokenized = cjk_tokenize(tok);
            let escaped = tokenized.replace('"', "\"\"");
            format!("\"{}\"", escaped)
        })
        .collect();

    phrases.join(" AND ")
}

// ---------------------------------------------------------------------------
// Text extraction
// ---------------------------------------------------------------------------

/// Extract indexable text from a `MessageContent` variant.
///
/// Returns `None` for media types (Image, Voice, Video) that have no text.
pub(crate) fn extract_fts_text(content: &MessageContent) -> Option<String> {
    match content {
        MessageContent::Text(s) => Some(s.clone()),
        MessageContent::Emoji(s) => Some(s.clone()),
        MessageContent::Location(s) => Some(s.clone()),
        MessageContent::System(_) => None,
        MessageContent::Revoke(s) => Some(s.clone()),
        MessageContent::Link {
            title,
            des,
            raw_xml,
            ..
        } => {
            let text = extract_app_fields(raw_xml);
            if text.is_empty() {
                let parts: Vec<&str> = [title.as_deref(), des.as_deref()]
                    .iter()
                    .filter_map(|x| *x)
                    .collect();
                let text = parts.join(" ");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            } else {
                Some(text)
            }
        }
        MessageContent::File { title, raw_xml, .. } => {
            let text = title.as_deref().unwrap_or("");
            if text.is_empty() {
                Some(extract_app_fields(raw_xml)).filter(|s| !s.is_empty())
            } else {
                Some(text.to_string())
            }
        }
        MessageContent::MiniProgram { title, raw_xml, .. } => {
            let text = title.as_deref().unwrap_or("");
            if text.is_empty() {
                Some(extract_app_fields(raw_xml)).filter(|s| !s.is_empty())
            } else {
                Some(text.to_string())
            }
        }
        MessageContent::MergedMessages { title, raw_xml, .. } => {
            let text = title.as_deref().unwrap_or("");
            if text.is_empty() {
                Some(extract_app_fields(raw_xml)).filter(|s| !s.is_empty())
            } else {
                Some(text.to_string())
            }
        }
        MessageContent::Quote {
            reply_text,
            refer_content,
            raw_xml,
            ..
        } => {
            let parts: Vec<&str> = [reply_text.as_deref(), refer_content.as_deref()]
                .iter()
                .filter_map(|x| *x)
                .collect();
            let text = parts.join(" ");
            if text.is_empty() {
                Some(extract_app_fields(raw_xml)).filter(|s| !s.is_empty())
            } else {
                Some(text)
            }
        }
        MessageContent::Transfer {
            amount_desc,
            pay_memo,
            ..
        } => {
            let parts: Vec<&str> = [amount_desc.as_deref(), pay_memo.as_deref()]
                .iter()
                .filter_map(|x| *x)
                .collect();
            let text = parts.join(" ");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        MessageContent::RedEnvelope { title, .. } => title.clone(),
        MessageContent::ChannelVideo { title, .. } => title.clone(),
        MessageContent::Pat { .. } => None,
        MessageContent::AppGeneric {
            title,
            des,
            raw_xml,
            ..
        } => {
            let text = extract_app_fields(raw_xml);
            if text.is_empty() {
                let parts: Vec<&str> = [title.as_deref(), des.as_deref()]
                    .iter()
                    .filter_map(|x| *x)
                    .collect();
                let text = parts.join(" ");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            } else {
                Some(text)
            }
        }
        MessageContent::Image { .. } => None,
        MessageContent::Voice => None,
        MessageContent::Video { .. } => None,
        MessageContent::Unknown { raw, .. } => Some(raw.clone()),
    }
}

/// Extract `<title>` and `<des>` fields from App message XML.
///
/// Handles `<![CDATA[...]]>` wrapping. Returns empty string on failure.
pub(crate) fn extract_app_fields(raw_xml: &str) -> String {
    let title = extract_xml_field(raw_xml, "title");
    let des = extract_xml_field(raw_xml, "des");

    match (title, des) {
        (Some(t), Some(d)) => format!("{}\n{}", t, d),
        (Some(t), None) => t,
        (None, Some(d)) => d,
        (None, None) => String::new(),
    }
}

/// Extract content between `<tag>` and `</tag>`, stripping CDATA if present.
fn extract_xml_field(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);

    let start = xml.find(&open)?;
    let after_open = start + open.len();
    let end = xml[after_open..].find(&close)?;
    let inner = &xml[after_open..after_open + end];

    let content = inner.trim();
    if content.is_empty() {
        return None;
    }

    // Strip CDATA wrapper if present
    let stripped = if content.starts_with("<![CDATA[") && content.ends_with("]]>") {
        &content[9..content.len() - 3]
    } else {
        content
    };

    let stripped = stripped.trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single hit from the FTS search index.
#[derive(Debug, Clone, Serialize)]
pub struct FtsHit {
    pub server_id: i64,
    pub talker: String,
    pub sender: String,
    pub create_time: i64,
    pub sort_seq: i64,
    pub msg_type: u32,
    pub sub_type: u32,
    /// Original (un-tokenized) text, for display.
    pub snippet: String,
}

/// Statistics from an FTS index build operation.
#[derive(Debug, Clone, Serialize)]
pub struct FtsBuildStats {
    pub indexed: usize,
    pub skipped: usize,
    pub duration_secs: f64,
    pub was_fresh: bool,
}

/// FTS search result (does not reuse `QueryResult` to avoid semantic clash
/// with `total_rows` / `scanned`).
#[derive(Debug, Clone)]
pub struct FtsSearchResult {
    pub hits: Vec<FtsHit>,
    /// Total number of FTS MATCH hits (for pagination).
    pub total_hits: usize,
}

// ---------------------------------------------------------------------------
// Paths + schema
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: &str = "1";

fn index_db_path(decrypted_root: &Path) -> PathBuf {
    decrypted_root.join("search_index.db")
}

fn index_tmp_path(decrypted_root: &Path) -> PathBuf {
    decrypted_root.join("search_index.tmp.db")
}

fn ensure_schema(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            body,
            raw_text    UNINDEXED,
            talker      UNINDEXED,
            sender      UNINDEXED,
            create_time UNINDEXED,
            sort_seq    UNINDEXED,
            server_id   UNINDEXED,
            msg_type    UNINDEXED,
            sub_type    UNINDEXED,
            tokenize    = 'unicode61'
        );

        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

        CREATE TABLE IF NOT EXISTS shard_state (
            shard_path TEXT PRIMARY KEY,
            last_mtime TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// Check if the existing index covers all current shards with matching mtimes.
fn is_index_fresh(conn: &Connection, shards: &[crate::open::MessageShard]) -> bool {
    for shard in shards {
        let current_mtime = match std::fs::metadata(&shard.path).and_then(|m| m.modified()) {
            Ok(t) => format!("{:?}", t),
            Err(_) => return false,
        };

        let stored: Result<String, _> = conn.query_row(
            "SELECT last_mtime FROM shard_state WHERE shard_path = ?1",
            [shard.path.to_string_lossy().as_ref()],
            |row| row.get(0),
        );

        match stored {
            Ok(s) if s == current_mtime => {}
            _ => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Index build
// ---------------------------------------------------------------------------

/// Rows between COMMIT + BEGIN cycles.
const COMMIT_INTERVAL: usize = 20_000;

impl WechatDb {
    /// Build (or skip if fresh) the FTS5 search index for all message shards.
    ///
    /// The index is written atomically: `.tmp.db` → `rename` → final path.
    pub fn build_fts_index(&self, decrypted_root: &Path) -> Result<FtsBuildStats, DbError> {
        let start = Instant::now();
        let final_path = index_db_path(decrypted_root);

        // Check freshness of existing index
        if final_path.exists() {
            let conn = Connection::open_with_flags(
                &final_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            if is_index_fresh(&conn, &self.shards) {
                return Ok(FtsBuildStats {
                    indexed: 0,
                    skipped: 0,
                    duration_secs: start.elapsed().as_secs_f64(),
                    was_fresh: true,
                });
            }
        }

        // Build session → table_name lookup
        let talker_map = self.build_talker_map()?;

        // Create tmp db
        let tmp_path = index_tmp_path(decrypted_root);
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }

        let conn = Connection::open(&tmp_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=OFF;
             PRAGMA temp_store=MEMORY;",
        )?;
        ensure_schema(&conn)?;

        let mut indexed: usize = 0;
        let mut skipped: usize = 0;
        let mut rows_in_tx: usize = 0;

        conn.execute_batch("BEGIN")?;

        // Prepare INSERT statements once (reused across all shards + tables)
        let mut insert_stmt = conn.prepare(
            "INSERT INTO message_fts (body, raw_text, talker, sender, create_time, \
             sort_seq, server_id, msg_type, sub_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut shard_state_stmt = conn.prepare(
            "INSERT OR REPLACE INTO shard_state (shard_path, last_mtime) VALUES (?1, ?2)",
        )?;

        for shard in &self.shards {
            let shard_conn =
                WechatDb::open_shard_with_key(shard, self.sqlcipher_key.as_ref())?;

            // List Msg_* tables in this shard
            let mut table_stmt = shard_conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'",
            )?;
            let table_names: Vec<String> = table_stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            for table_name in &table_names {
                let talker = match talker_map.get(table_name.as_str()) {
                    Some(t) => t.as_str(),
                    None => continue, // unknown table, skip
                };
                let is_group = crate::model::is_group_chat(talker);

                let has_ct_col =
                    check_column_exists(&shard_conn, table_name, "WCDB_CT_message_content")?;

                let sql = if has_ct_col {
                    format!(
                        "SELECT m.sort_seq, m.server_id, m.local_type, \
                         COALESCE(n.user_name, ''), m.create_time, \
                         m.message_content, m.packed_info_data, \
                         m.WCDB_CT_message_content \
                         FROM [{table}] m \
                         LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid",
                        table = table_name,
                    )
                } else {
                    format!(
                        "SELECT m.sort_seq, m.server_id, m.local_type, \
                         COALESCE(n.user_name, ''), m.create_time, \
                         m.message_content, m.packed_info_data \
                         FROM [{table}] m \
                         LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid",
                        table = table_name,
                    )
                };

                let mut stmt = shard_conn.prepare(&sql)?;
                let mut rows = stmt.query([])?;

                while let Some(row) = rows.next()? {
                    match Self::index_one_row(&mut insert_stmt, row, has_ct_col, is_group, talker) {
                        Ok(true) => {
                            indexed += 1;
                            rows_in_tx += 1;
                        }
                        Ok(false) => { /* no text to index (image, voice, etc.) */ }
                        Err(_) => {
                            skipped += 1;
                        }
                    }

                    if rows_in_tx >= COMMIT_INTERVAL {
                        conn.execute_batch("COMMIT; BEGIN")?;
                        rows_in_tx = 0;
                    }
                }
            }

            // Record shard mtime
            let mtime = std::fs::metadata(&shard.path)
                .and_then(|m| m.modified())
                .map(|t| format!("{:?}", t))
                .unwrap_or_default();
            shard_state_stmt.execute(rusqlite::params![
                shard.path.to_string_lossy().as_ref(),
                mtime,
            ])?;
        }

        // Drop prepared statements to release borrow on conn before meta writes
        drop(insert_stmt);
        drop(shard_state_stmt);

        // Write meta
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('built_at', ?1)",
            [&now],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('message_count', ?1)",
            [indexed.to_string()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION],
        )?;

        conn.execute_batch("COMMIT")?;
        drop(conn);

        // Atomic rename
        std::fs::rename(&tmp_path, &final_path)?;

        Ok(FtsBuildStats {
            indexed,
            skipped,
            duration_secs: start.elapsed().as_secs_f64(),
            was_fresh: false,
        })
    }

    /// Build a HashMap<table_name, talker_wxid> from session.db.
    fn build_talker_map(&self) -> Result<HashMap<String, String>, DbError> {
        let mut stmt = self
            .session_conn
            .prepare("SELECT username FROM SessionTable")?;
        let mut rows = stmt.query([])?;
        let mut map = HashMap::new();
        while let Some(row) = rows.next()? {
            let username: String = row.get(0)?;
            let table = msg_table_name(&username);
            map.insert(table, username);
        }
        Ok(map)
    }

    /// Decode one message row and insert into the FTS index via a prepared statement.
    /// Returns Ok(true) if a row was inserted, Ok(false) if skipped (no text).
    fn index_one_row(
        insert_stmt: &mut rusqlite::Statement<'_>,
        row: &rusqlite::Row<'_>,
        has_ct_col: bool,
        is_group: bool,
        talker: &str,
    ) -> Result<bool, DbError> {
        let sort_seq: i64 = row.get(0)?;
        let server_id: i64 = row.get(1)?;
        let local_type: u32 = row.get(2)?;
        let sender_from_name2id: String = row.get(3)?;
        let create_time: i64 = row.get(4)?;

        let raw_content: Vec<u8> = match row.get_ref(5)? {
            ValueRef::Blob(b) => b.to_vec(),
            ValueRef::Text(b) => b.to_vec(),
            ValueRef::Null => Vec::new(),
            _ => Vec::new(),
        };

        // packed_info_data — not needed for FTS text extraction, skip decoding
        // WCDB_CT column
        let wcdb_ct: Option<i32> = if has_ct_col {
            row.get::<_, Option<i32>>(7)?
        } else {
            None
        };

        let decoded_text = decode_content(&raw_content, wcdb_ct)?;

        // Group sender parsing
        let (sender, content_text) = parse_group_sender(is_group, decoded_text, sender_from_name2id);

        let (msg_type, sub_type) = split_local_type(local_type as i64);

        // Parse into MessageContent (packed_info not needed for text extraction)
        let content = parse_content(msg_type, sub_type, &content_text, server_id, None, None);

        // Extract indexable text
        let raw_text = match extract_fts_text(&content) {
            Some(t) => t,
            None => return Ok(false),
        };

        let tokenized_body = cjk_tokenize(&raw_text);

        insert_stmt.execute(rusqlite::params![
            tokenized_body,
            raw_text,
            talker,
            sender,
            create_time,
            sort_seq,
            server_id,
            msg_type,
            sub_type,
        ])?;

        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// FTS search
// ---------------------------------------------------------------------------

impl WechatDb {
    /// Search the FTS index. Returns `Ok(None)` if the index file does not exist.
    pub fn search_fts(
        decrypted_root: &Path,
        keyword: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Option<FtsSearchResult>, DbError> {
        let path = index_db_path(decrypted_root);
        if !path.exists() {
            return Ok(None);
        }

        let fts_query = build_fts_query(keyword);
        if fts_query.is_empty() {
            return Ok(Some(FtsSearchResult {
                hits: Vec::new(),
                total_hits: 0,
            }));
        }

        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        // Count total matches
        let total_hits: usize = conn.query_row(
            "SELECT count(*) FROM message_fts WHERE message_fts MATCH ?1",
            [&fts_query],
            |row| row.get::<_, i64>(0),
        )? as usize;

        // Fetch paginated results
        let mut stmt = conn.prepare(
            "SELECT server_id, talker, sender, create_time, sort_seq, \
             msg_type, sub_type, raw_text \
             FROM message_fts WHERE message_fts MATCH ?1 \
             ORDER BY create_time DESC, sort_seq DESC \
             LIMIT ?2 OFFSET ?3",
        )?;

        let hits: Vec<FtsHit> = stmt
            .query_map(
                rusqlite::params![fts_query, limit as i64, offset as i64],
                |row| {
                    Ok(FtsHit {
                        server_id: row.get(0)?,
                        talker: row.get(1)?,
                        sender: row.get(2)?,
                        create_time: row.get(3)?,
                        sort_seq: row.get(4)?,
                        msg_type: row.get::<_, i64>(5)? as u32,
                        sub_type: row.get::<_, i64>(6)? as u32,
                        snippet: row.get(7)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(FtsSearchResult { hits, total_hits }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- cjk_tokenize --

    #[test]
    fn tokenize_pure_chinese() {
        assert_eq!(cjk_tokenize("你好世界"), "你 好 世 界");
    }

    #[test]
    fn tokenize_pure_english() {
        assert_eq!(cjk_tokenize("hello world"), "hello world");
    }

    #[test]
    fn tokenize_mixed() {
        assert_eq!(cjk_tokenize("你好world 测试"), "你 好 world 测 试");
    }

    #[test]
    fn tokenize_empty() {
        assert_eq!(cjk_tokenize(""), "");
    }

    #[test]
    fn tokenize_cjk_punctuation() {
        // U+3001 (、) is in CJK punctuation range
        assert_eq!(cjk_tokenize("你、好"), "你 、 好");
    }

    #[test]
    fn tokenize_ext_b_character() {
        // U+20000 (𠀀) is in CJK Ext B range
        assert_eq!(cjk_tokenize("𠀀test"), "𠀀 test");
    }

    #[test]
    fn tokenize_fullwidth() {
        // U+FF01 (！) is in Fullwidth Forms
        assert_eq!(cjk_tokenize("hello！world"), "hello ！ world");
    }

    // -- build_fts_query --

    #[test]
    fn query_single_chinese() {
        assert_eq!(build_fts_query("测试"), "\"测 试\"");
    }

    #[test]
    fn query_multi_word() {
        assert_eq!(build_fts_query("你好 world"), "\"你 好\" AND \"world\"");
    }

    #[test]
    fn query_pure_english() {
        assert_eq!(build_fts_query("hello"), "\"hello\"");
    }

    #[test]
    fn query_mixed() {
        assert_eq!(build_fts_query("你好world"), "\"你 好 world\"");
    }

    #[test]
    fn query_with_double_quotes() {
        // Input: he said "hi"  (3 whitespace-separated tokens)
        assert_eq!(
            build_fts_query("he said \"hi\""),
            "\"he\" AND \"said\" AND \"\"\"hi\"\"\""
        );
    }

    #[test]
    fn query_empty() {
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("   "), "");
    }

    // -- extract_fts_text --

    #[test]
    fn extract_text_message() {
        let content = MessageContent::Text("hello".into());
        assert_eq!(extract_fts_text(&content), Some("hello".into()));
    }

    #[test]
    fn extract_image_none() {
        let content = MessageContent::Image { md5: None };
        assert_eq!(extract_fts_text(&content), None);
    }

    #[test]
    fn extract_voice_none() {
        assert_eq!(extract_fts_text(&MessageContent::Voice), None);
    }

    #[test]
    fn extract_video_none() {
        let content = MessageContent::Video { md5: None };
        assert_eq!(extract_fts_text(&content), None);
    }

    #[test]
    fn extract_emoji() {
        let content = MessageContent::Emoji("<emoji>".into());
        assert_eq!(extract_fts_text(&content), Some("<emoji>".into()));
    }

    #[test]
    fn extract_location() {
        let content = MessageContent::Location("loc".into());
        assert_eq!(extract_fts_text(&content), Some("loc".into()));
    }

    #[test]
    fn extract_system() {
        let content = MessageContent::System("sys".into());
        assert_eq!(extract_fts_text(&content), None);
    }

    #[test]
    fn extract_revoke() {
        let content = MessageContent::Revoke("revoke".into());
        assert_eq!(extract_fts_text(&content), Some("revoke".into()));
    }

    #[test]
    fn extract_unknown() {
        let content = MessageContent::Unknown {
            msg_type: 999,
            raw: "raw content".into(),
        };
        assert_eq!(extract_fts_text(&content), Some("raw content".into()));
    }

    #[test]
    fn extract_app_with_title_and_des() {
        let content = MessageContent::Link {
            sub_type: 5,
            title: Some("Link Title".into()),
            des: Some("Description text".into()),
            url: None,
            raw_xml: "<msg><appmsg><title><![CDATA[Link Title]]></title><des><![CDATA[Description text]]></des></appmsg></msg>".into(),
        };
        assert_eq!(
            extract_fts_text(&content),
            Some("Link Title\nDescription text".into())
        );
    }

    #[test]
    fn extract_app_title_only() {
        let content = MessageContent::Link {
            sub_type: 5,
            title: Some("Just Title".into()),
            des: None,
            url: None,
            raw_xml: "<msg><appmsg><title>Just Title</title></appmsg></msg>".into(),
        };
        assert_eq!(extract_fts_text(&content), Some("Just Title".into()));
    }

    #[test]
    fn extract_app_empty_xml() {
        let content = MessageContent::Link {
            sub_type: 5,
            title: None,
            des: None,
            url: None,
            raw_xml: "<msg></msg>".into(),
        };
        assert_eq!(extract_fts_text(&content), None);
    }

    // -- extract_app_fields --

    #[test]
    fn app_fields_cdata() {
        let xml = "<title><![CDATA[Hello]]></title><des><![CDATA[World]]></des>";
        assert_eq!(extract_app_fields(xml), "Hello\nWorld");
    }

    #[test]
    fn app_fields_no_cdata() {
        let xml = "<title>Hello</title><des>World</des>";
        assert_eq!(extract_app_fields(xml), "Hello\nWorld");
    }

    #[test]
    fn app_fields_no_title() {
        let xml = "<des>Only Des</des>";
        assert_eq!(extract_app_fields(xml), "Only Des");
    }

    #[test]
    fn app_fields_no_des() {
        let xml = "<title>Only Title</title>";
        assert_eq!(extract_app_fields(xml), "Only Title");
    }

    #[test]
    fn app_fields_empty() {
        assert_eq!(extract_app_fields(""), "");
        assert_eq!(extract_app_fields("<title></title>"), "");
    }

    // ====================================================================
    // Round-trip tests: build_fts_index + search_fts
    // ====================================================================

    use rusqlite::{params, Connection as SqlConn};
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal fixture with session.db, contact.db, and one message shard.
    fn create_fts_fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        // contact/contact.db
        let contact_dir = base.join("contact");
        fs::create_dir_all(&contact_dir).unwrap();
        let conn = SqlConn::open(contact_dir.join("contact.db")).unwrap();
        crate::test_ddl::create_test_contact_table_minimal(&conn);
        conn.execute(
            "INSERT INTO contact VALUES (?1, ?2, ?3, ?4)",
            params!["wxid_alice", "", "", "Alice"],
        )
        .unwrap();
        drop(conn);

        // session/session.db — MUST have rows for session lookup
        let session_dir = base.join("session");
        fs::create_dir_all(&session_dir).unwrap();
        let conn = SqlConn::open(session_dir.join("session.db")).unwrap();
        crate::test_ddl::create_test_session_table(&conn);
        // Insert session for "wxid_alice"
        conn.execute(
            "INSERT INTO SessionTable VALUES (?1, ?2, ?3)",
            params!["wxid_alice", 1700000000_i64, "last msg"],
        )
        .unwrap();
        drop(conn);

        // message/message_0.db
        let msg_dir = base.join("message");
        fs::create_dir_all(&msg_dir).unwrap();

        // md5("wxid_alice") = 29a6db07e8bbdb53f5d54cc3c309f3f1
        let alice_table = "Msg_29a6db07e8bbdb53f5d54cc3c309f3f1";

        let conn = SqlConn::open(msg_dir.join("message_0.db")).unwrap();
        conn.execute_batch("CREATE TABLE Timestamp (timestamp INTEGER);")
            .unwrap();
        conn.execute(
            "INSERT INTO Timestamp VALUES (?1)",
            params![1_700_000_000_i64],
        )
        .unwrap();

        conn.execute_batch("CREATE TABLE Name2Id (rowid INTEGER PRIMARY KEY, user_name TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO Name2Id VALUES (?1, ?2)",
            params![1, "wxid_alice"],
        )
        .unwrap();

        conn.execute_batch(&format!(
            "CREATE TABLE [{table}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER
            );",
            table = alice_table,
        ))
        .unwrap();

        // Row 1: Chinese text "你好世界"
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                100_i64,
                1001_i64,
                1_u32,
                1_i64,
                1700000001_i64,
                "你好世界".as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 2: English text "hello world"
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                101_i64,
                1002_i64,
                1_u32,
                1_i64,
                1700000002_i64,
                "hello world".as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 3: Image (msg_type=3) — should NOT be indexed
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                102_i64,
                1003_i64,
                3_u32,
                1_i64,
                1700000003_i64,
                "img_content".as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 4: App message with XML
        let app_xml = "<msg><appmsg><title><![CDATA[Link Title]]></title><des><![CDATA[Description]]></des></appmsg></msg>";
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                103_i64,
                1004_i64,
                49_u32,
                1_i64,
                1700000004_i64,
                app_xml.as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 5: Mixed CJK+English "我在用 iPhone 15"
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                104_i64,
                1005_i64,
                1_u32,
                1_i64,
                1700000005_i64,
                "我在用 iPhone 15".as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 6: Another text message "hello again" for pagination testing
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                105_i64,
                1006_i64,
                1_u32,
                1_i64,
                1700000006_i64,
                "hello again".as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        // Row 7: System message (msg_type=10000) with sysmsg XML — should NOT be indexed
        let sysmsg_xml = "<sysmsg type=\"pat\"><pat><fromusername>wxid_alice</fromusername><pattedusername>wxid_bob</pattedusername></pat></sysmsg>";
        conn.execute(
            &format!(
                "INSERT INTO [{table}] VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                table = alice_table
            ),
            params![
                106_i64,
                1007_i64,
                10000_u32,
                1_i64,
                1700000007_i64,
                sysmsg_xml.as_bytes(),
                rusqlite::types::Null,
                0_i32
            ],
        )
        .unwrap();

        drop(conn);
        dir
    }

    #[test]
    fn build_and_search_chinese() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();

        let stats = db.build_fts_index(dir.path()).unwrap();
        assert!(!stats.was_fresh);
        assert!(stats.indexed >= 5); // text*3 + app + mixed (not image)
        assert_eq!(stats.skipped, 0);

        let result = crate::open::WechatDb::search_fts(dir.path(), "你好", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert!(result.total_hits >= 1);
        assert!(result.hits.iter().any(|h| h.snippet.contains("你好世界")));
    }

    #[test]
    fn build_and_search_english() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        let result = crate::open::WechatDb::search_fts(dir.path(), "hello", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert!(result.total_hits >= 1);
        assert!(result
            .hits
            .iter()
            .any(|h| h.snippet.contains("hello world")));
    }

    #[test]
    fn search_image_not_indexed() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        let result = crate::open::WechatDb::search_fts(dir.path(), "img_content", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert_eq!(result.total_hits, 0);
    }

    #[test]
    fn search_nonexistent_index() {
        let dir = TempDir::new().unwrap();
        let result = crate::open::WechatDb::search_fts(dir.path(), "test", 10, 0).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn search_pagination() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        // "hello" appears in "hello world" and "hello again" — guaranteed 2 hits
        let all = crate::open::WechatDb::search_fts(dir.path(), "hello", 10, 0)
            .unwrap()
            .unwrap();
        assert_eq!(all.total_hits, 2);
        assert_eq!(all.hits.len(), 2);

        // offset=1 should return 1 hit
        let page2 = crate::open::WechatDb::search_fts(dir.path(), "hello", 10, 1)
            .unwrap()
            .unwrap();
        assert_eq!(page2.total_hits, 2); // total unchanged
        assert_eq!(page2.hits.len(), 1);

        // limit=1 should return 1 hit
        let limited = crate::open::WechatDb::search_fts(dir.path(), "hello", 1, 0)
            .unwrap()
            .unwrap();
        assert_eq!(limited.total_hits, 2); // total unchanged
        assert_eq!(limited.hits.len(), 1);
    }

    #[test]
    fn search_iphone_in_mixed_text() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        let result = crate::open::WechatDb::search_fts(dir.path(), "iPhone", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert!(
            result.total_hits >= 1,
            "iPhone should match in mixed CJK+English text"
        );
    }

    #[test]
    fn index_freshness_skip_rebuild() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();

        let stats1 = db.build_fts_index(dir.path()).unwrap();
        assert!(!stats1.was_fresh);

        let stats2 = db.build_fts_index(dir.path()).unwrap();
        assert!(stats2.was_fresh);
    }

    #[test]
    fn search_app_message() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        let result = crate::open::WechatDb::search_fts(dir.path(), "Description", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert!(result.total_hits >= 1, "Should match App message des field");
    }

    #[test]
    fn search_system_not_indexed() {
        let dir = create_fts_fixture();
        let db = crate::open::WechatDb::open(dir.path()).unwrap();
        db.build_fts_index(dir.path()).unwrap();

        // sysmsg XML keywords should not appear in FTS index
        let result = crate::open::WechatDb::search_fts(dir.path(), "sysmsg", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert_eq!(
            result.total_hits, 0,
            "System messages should not be indexed"
        );

        let result2 = crate::open::WechatDb::search_fts(dir.path(), "pattedusername", 10, 0)
            .unwrap()
            .expect("index should exist");
        assert_eq!(
            result2.total_hits, 0,
            "System message XML content should not be indexed"
        );
    }
}
