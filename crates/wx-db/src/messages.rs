use std::collections::HashMap;

use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::decode::{
    check_column_exists, decode_content, decode_packed_info, msg_table_name, parse_content,
    parse_group_sender, table_exists,
};
use crate::error::{DbError, ShardWarning};
use crate::model::{
    effective_limit, split_local_type, AnchorMode, Message, MessageQuery, MessageQueryResult,
    QueryStats, SortOrder,
};
use crate::open::{MessageShard, SqlcipherKey, WechatDb};

/// Dispatch mode for regular (non-anchor) queries.
enum RegularQueryMode {
    /// Full table scan — used when keyword, filtered_count, or anchor is active.
    FullScan,
    /// SQL LIMIT pushdown — per-shard `ORDER BY sort_seq {order} LIMIT ?`.
    LimitPushdown {
        /// `offset + effective_limit` — each shard fetches at most this many rows.
        sql_limit: usize,
        /// Optional msg_type pushed to SQL WHERE clause.
        msg_type_filter: Option<u32>,
    },
}

/// Result of preparing a shard for querying: connection + column metadata.
enum ShardConnection<'a> {
    Borrowed(&'a Connection),
    Owned(Connection),
}

impl ShardConnection<'_> {
    fn as_conn(&self) -> &Connection {
        match self {
            ShardConnection::Borrowed(conn) => conn,
            ShardConnection::Owned(conn) => conn,
        }
    }
}

struct PreparedShard<'a> {
    conn: ShardConnection<'a>,
    select_cols: String,
    has_ct_col: bool,
    has_compress_col: bool,
}

/// Try to open a shard and check table/column availability.
/// Returns `None` (with a warning pushed) if the shard cannot be used.
fn prepare_shard_query<'a>(
    shard: &MessageShard,
    table_name: &str,
    warnings: &mut Vec<ShardWarning>,
    pooled_conn: Option<&'a Connection>,
    sqlcipher_key: Option<&SqlcipherKey>,
) -> Option<PreparedShard<'a>> {
    let shard_path = shard.path.display().to_string();

    let conn = match pooled_conn {
        Some(conn) => ShardConnection::Borrowed(conn),
        None => match WechatDb::open_shard_with_key(shard, sqlcipher_key) {
            Ok(c) => ShardConnection::Owned(c),
            Err(e) => {
                warnings.push(ShardWarning {
                    path: shard_path,
                    reason: format!("open failed: {e}"),
                });
                return None;
            }
        },
    };
    let conn_ref = conn.as_conn();

    match table_exists(conn_ref, table_name) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => {
            warnings.push(ShardWarning {
                path: shard_path,
                reason: format!("table_exists check failed: {e}"),
            });
            return None;
        }
    }

    let has_ct_col = match check_column_exists(conn_ref, table_name, "WCDB_CT_message_content") {
        Ok(v) => v,
        Err(e) => {
            warnings.push(ShardWarning {
                path: shard_path,
                reason: format!("check WCDB_CT column failed: {e}"),
            });
            return None;
        }
    };

    let has_compress_col = match check_column_exists(conn_ref, table_name, "compress_content") {
        Ok(v) => v,
        Err(e) => {
            warnings.push(ShardWarning {
                path: shard_path,
                reason: format!("check compress_content column failed: {e}"),
            });
            return None;
        }
    };

    let mut select_cols = String::from(
        "m.sort_seq, m.server_id, m.local_type, \
         COALESCE(n.user_name, ''), m.create_time, \
         m.message_content, m.packed_info_data, m.status",
    );
    if has_ct_col {
        select_cols.push_str(", m.WCDB_CT_message_content");
    }
    if has_compress_col {
        select_cols.push_str(", m.compress_content");
    }

    Some(PreparedShard {
        conn,
        select_cols,
        has_ct_col,
        has_compress_col,
    })
}

impl WechatDb {
    /// Query messages for a given talker (contact or chatroom).
    ///
    /// Pipeline:
    /// 1. Compute the Msg table name from talker via MD5
    /// 2. Find shards overlapping the time range
    /// 3. Decide query mode: LIMIT pushdown (index-backed) or full scan
    /// 4. For each shard: open, check table exists, build SQL, decode rows
    ///    (individual shard failures are recorded as warnings, not errors)
    /// 5. Merge results across shards, sort by (sort_seq, create_time, server_id)
    /// 6. Apply post-filters (keyword, msg_type) in full-scan mode
    /// 7. Apply offset + limit (Rust is the authoritative paginator)
    pub fn query_messages(&self, query: &MessageQuery) -> Result<MessageQueryResult, DbError> {
        let limit = effective_limit(query.limit);
        let table_name = msg_table_name(&query.talker);
        let is_group = crate::model::is_group_chat(&query.talker);

        let shards = self.shards_for_range(query.start_time, query.end_time);
        if self.shards.is_empty() {
            return Err(DbError::NoShards);
        }

        // Decide query mode
        let mode = if query.limit_pushdown_eligible() {
            // Safely compute per-shard SQL LIMIT = offset + limit.
            // Use saturating_add to avoid usize overflow, then cap at i64::MAX
            // so the subsequent `as i64` cast is always non-negative.
            let sql_limit = query.offset.saturating_add(limit).min(i64::MAX as usize);
            RegularQueryMode::LimitPushdown {
                sql_limit,
                msg_type_filter: query.msg_type_filter,
            }
        } else {
            RegularQueryMode::FullScan
        };

        let mut all_messages: Vec<Message> = Vec::new();
        let mut total_rows: usize = 0;
        let mut skipped: usize = 0;
        let mut shard_warnings: Vec<ShardWarning> = Vec::new();

        for shard in &shards {
            let prepared = match prepare_shard_query(
                shard,
                &table_name,
                &mut shard_warnings,
                self.pool().and_then(|pool| pool.get(&shard.path)),
                self.sqlcipher_key.as_ref(),
            ) {
                Some(p) => p,
                None => continue,
            };
            let shard_path = shard.path.display().to_string();

            let (sql, params) = build_regular_shard_sql(
                &mode,
                &prepared.select_cols,
                &table_name,
                query.order,
                query.start_time,
                query.end_time,
            );

            query_shard_sql(
                &prepared,
                &sql,
                &params,
                is_group,
                &query.talker,
                &shard_path,
                &mut all_messages,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );
        }

        // Sort all messages by (sort_seq, create_time, server_id) in requested direction
        match query.order {
            SortOrder::Asc => {
                all_messages.sort_unstable_by_key(|m| (m.sort_seq, m.create_time, m.server_id))
            }
            SortOrder::Desc => all_messages.sort_unstable_by(|a, b| {
                (b.sort_seq, b.create_time, b.server_id).cmp(&(
                    a.sort_seq,
                    a.create_time,
                    a.server_id,
                ))
            }),
        }

        // Apply post-filters based on mode
        match &mode {
            RegularQueryMode::FullScan => {
                // Keyword filter post-SQL (content is compressed in DB)
                if let Some(ref kw) = query.keyword {
                    let kw_lower = kw.to_lowercase();
                    all_messages.retain(|m| content_contains_keyword(&m.content, &kw_lower));
                }
                // msg_type filter
                if let Some(mt) = query.msg_type_filter {
                    all_messages.retain(|m| m.msg_type == mt);
                }
            }
            RegularQueryMode::LimitPushdown { .. } => {
                // keyword is None (precondition), msg_type already in SQL WHERE
                // No post-filters needed
            }
        }

        // Compute filtered_count before pagination (opt-in, only in FullScan mode)
        let filtered_count = if query.with_filtered_count {
            Some(all_messages.len())
        } else {
            None
        };

        // Apply offset + limit — Rust is the authoritative paginator
        let after_offset: Vec<Message> = all_messages
            .into_iter()
            .skip(query.offset)
            .take(limit)
            .collect();

        Ok(MessageQueryResult {
            items: after_offset,
            stats: QueryStats {
                total_rows,
                filtered_count,
                skipped,
            },
            shard_warnings,
        })
    }

    /// Count total messages for a talker across all shards (lightweight, no content decoding).
    ///
    /// Uses `SELECT COUNT(*)` per shard, which is fast (index-only scan, no row decoding).
    /// Applies the same time-range and msg_type filters as `query_messages` but skips
    /// keyword post-filters since those require content decoding.
    pub fn count_messages(
        &self,
        talker: &str,
        start_time: i64,
        end_time: i64,
        msg_type_filter: Option<u32>,
    ) -> usize {
        let table_name = msg_table_name(talker);
        let shards = self.shards_for_range(start_time, end_time);
        let mut total: usize = 0;

        let sql = if msg_type_filter.is_some() {
            format!(
                "SELECT COUNT(*) FROM [{table_name}] \
                 WHERE create_time >= ?1 AND create_time <= ?2 \
                   AND (local_type & 4294967295) = ?3"
            )
        } else {
            format!(
                "SELECT COUNT(*) FROM [{table_name}] \
                 WHERE create_time >= ?1 AND create_time <= ?2"
            )
        };

        for shard in &shards {
            let count = if let Some(pool) = self.pool() {
                if let Some(conn) = pool.get(&shard.path) {
                    Self::count_shard(conn, &sql, start_time, end_time, msg_type_filter)
                } else {
                    continue;
                }
            } else {
                match crate::open::open_connection(&shard.path, self.sqlcipher_key.as_ref()) {
                    Ok(conn) => {
                        Self::count_shard(&conn, &sql, start_time, end_time, msg_type_filter)
                    }
                    Err(_) => continue,
                }
            };

            total += count;
        }

        total
    }

    fn count_shard(
        conn: &Connection,
        sql: &str,
        start_time: i64,
        end_time: i64,
        msg_type_filter: Option<u32>,
    ) -> usize {
        let result = if let Some(mt) = msg_type_filter {
            conn.query_row(
                sql,
                [start_time, end_time, mt as i64],
                |row: &rusqlite::Row<'_>| row.get::<_, i64>(0),
            )
        } else {
            conn.query_row(sql, [start_time, end_time], |row: &rusqlite::Row<'_>| {
                row.get::<_, i64>(0)
            })
        };
        result.unwrap_or(0).max(0) as usize
    }

    /// Query messages using an anchor mode (around/after a specific position).
    ///
    /// Requires `query.anchor` to be `Some`. Uses full-shard scan for correctness.
    pub fn query_messages_anchor(
        &self,
        query: &MessageQuery,
    ) -> Result<MessageQueryResult, DbError> {
        let anchor = query
            .anchor
            .as_ref()
            .expect("query_messages_anchor called without anchor mode");

        if self.shards.is_empty() {
            return Err(DbError::NoShards);
        }

        let table_name = msg_table_name(&query.talker);
        let is_group = crate::model::is_group_chat(&query.talker);

        match anchor {
            AnchorMode::AfterSortSeq(seq) => {
                self.query_after_sort_seq(query, &table_name, is_group, *seq)
            }
            AnchorMode::AroundSortSeq(seq) => {
                self.query_around_sort_seq(query, &table_name, is_group, *seq)
            }
            AnchorMode::AroundServerId(id) => {
                self.query_around_server_id(query, &table_name, is_group, *id)
            }
        }
    }

    fn query_after_sort_seq(
        &self,
        query: &MessageQuery,
        table_name: &str,
        is_group: bool,
        seq: i64,
    ) -> Result<MessageQueryResult, DbError> {
        let limit = effective_limit(query.limit);
        let mut all_messages: Vec<Message> = Vec::new();
        let mut total_rows: usize = 0;
        let mut skipped: usize = 0;
        let mut shard_warnings: Vec<ShardWarning> = Vec::new();

        for shard in self.all_shards() {
            let prepared = match prepare_shard_query(
                shard,
                table_name,
                &mut shard_warnings,
                self.pool().and_then(|pool| pool.get(&shard.path)),
                self.sqlcipher_key.as_ref(),
            ) {
                Some(p) => p,
                None => continue,
            };
            let shard_path = shard.path.display().to_string();

            let sql = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.sort_seq > ?1 \
                 ORDER BY m.sort_seq ASC, m.create_time ASC, m.server_id ASC",
                select_cols = prepared.select_cols,
                table = table_name,
            );

            let mut stmt = match prepared.conn.as_conn().prepare(&sql) {
                Ok(s) => s,
                Err(e) => {
                    shard_warnings.push(ShardWarning {
                        path: shard_path,
                        reason: format!("prepare failed: {e}"),
                    });
                    continue;
                }
            };

            let mut rows = match stmt.query([seq]) {
                Ok(r) => r,
                Err(e) => {
                    shard_warnings.push(ShardWarning {
                        path: shard_path,
                        reason: format!("query failed: {e}"),
                    });
                    continue;
                }
            };

            collect_rows(
                &mut rows,
                &prepared,
                is_group,
                &query.talker,
                &shard_path,
                &mut all_messages,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );
        }

        // Sort ASC by compound key
        all_messages.sort_unstable_by_key(|m| (m.sort_seq, m.create_time, m.server_id));

        // Apply filters then take(limit)
        apply_post_filters(&mut all_messages, &query.keyword, query.msg_type_filter);
        all_messages.truncate(limit);

        Ok(MessageQueryResult {
            items: all_messages,
            stats: QueryStats {
                total_rows,
                filtered_count: None,
                skipped,
            },
            shard_warnings,
        })
    }

    fn query_around_sort_seq(
        &self,
        query: &MessageQuery,
        table_name: &str,
        is_group: bool,
        seq: i64,
    ) -> Result<MessageQueryResult, DbError> {
        let context = query.context;
        let mut before: Vec<Message> = Vec::new();
        let mut pivot: Vec<Message> = Vec::new();
        let mut after: Vec<Message> = Vec::new();
        let mut total_rows: usize = 0;
        let mut skipped: usize = 0;
        let mut shard_warnings: Vec<ShardWarning> = Vec::new();

        for shard in self.all_shards() {
            let prepared = match prepare_shard_query(
                shard,
                table_name,
                &mut shard_warnings,
                self.pool().and_then(|pool| pool.get(&shard.path)),
                self.sqlcipher_key.as_ref(),
            ) {
                Some(p) => p,
                None => continue,
            };
            let shard_path = shard.path.display().to_string();

            // Before segment
            let sql_before = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.sort_seq < ?1 \
                 ORDER BY m.sort_seq DESC, m.create_time DESC, m.server_id DESC \
                 LIMIT ?2",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_before,
                &[seq, context as i64],
                is_group,
                &query.talker,
                &shard_path,
                &mut before,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );

            // Pivot bucket (no LIMIT — return all messages at this sort_seq)
            let sql_pivot = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.sort_seq = ?1 \
                 ORDER BY m.create_time ASC, m.server_id ASC",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_pivot,
                &[seq],
                is_group,
                &query.talker,
                &shard_path,
                &mut pivot,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );

            // After segment
            let sql_after = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.sort_seq > ?1 \
                 ORDER BY m.sort_seq ASC, m.create_time ASC, m.server_id ASC \
                 LIMIT ?2",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_after,
                &[seq, context as i64],
                is_group,
                &query.talker,
                &shard_path,
                &mut after,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );
        }

        // Sort and truncate each segment across shards
        // Before: sort DESC then take(context), then reverse to ASC
        before.sort_unstable_by(|a, b| {
            (b.sort_seq, b.create_time, b.server_id).cmp(&(a.sort_seq, a.create_time, a.server_id))
        });
        before.truncate(context);
        before.reverse();

        // Pivot: sort ASC by (create_time, server_id)
        pivot.sort_unstable_by_key(|m| (m.create_time, m.server_id));

        // After: sort ASC then take(context)
        after.sort_unstable_by_key(|m| (m.sort_seq, m.create_time, m.server_id));
        after.truncate(context);

        // Merge: before + pivot + after
        let mut all_messages = before;
        all_messages.append(&mut pivot);
        all_messages.append(&mut after);

        // Apply post-filters (may reduce count below 2*context + pivot)
        apply_post_filters(&mut all_messages, &query.keyword, query.msg_type_filter);

        Ok(MessageQueryResult {
            items: all_messages,
            stats: QueryStats {
                total_rows,
                filtered_count: None,
                skipped,
            },
            shard_warnings,
        })
    }

    fn query_around_server_id(
        &self,
        query: &MessageQuery,
        table_name: &str,
        is_group: bool,
        target_server_id: i64,
    ) -> Result<MessageQueryResult, DbError> {
        let context = query.context;
        let mut shard_warnings: Vec<ShardWarning> = Vec::new();

        // Phase 1: Locate the target message by server_id across all shards
        let mut pivot_msg: Option<(i64, i64, i64)> = None; // (sort_seq, create_time, server_id)
        for shard in self.all_shards() {
            let prepared = match prepare_shard_query(
                shard,
                table_name,
                &mut shard_warnings,
                self.pool().and_then(|pool| pool.get(&shard.path)),
                self.sqlcipher_key.as_ref(),
            ) {
                Some(p) => p,
                None => continue,
            };

            let sql = format!(
                "SELECT m.sort_seq, m.create_time, m.server_id \
                 FROM [{table}] m \
                 WHERE m.server_id = ?1 \
                 LIMIT 1",
                table = table_name,
            );
            let result: Result<Option<(i64, i64, i64)>, _> = prepared
                .conn
                .as_conn()
                .query_row(&sql, [target_server_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map(Some)
                .or_else(|e| {
                    if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                });
            match result {
                Ok(Some(row)) => {
                    pivot_msg = Some(row);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    shard_warnings.push(ShardWarning {
                        path: shard.path.display().to_string(),
                        reason: format!("locate server_id failed: {e}"),
                    });
                    continue;
                }
            }
        }

        let (pivot_seq, pivot_ct, pivot_sid) = match pivot_msg {
            Some(t) => t,
            None => {
                // server_id not found — return empty result
                return Ok(MessageQueryResult {
                    items: vec![],
                    stats: QueryStats {
                        total_rows: 0,
                        filtered_count: None,
                        skipped: 0,
                    },
                    shard_warnings,
                });
            }
        };

        // Phase 2: Query context around the located message
        let mut pivot_messages: Vec<Message> = Vec::new();
        let mut before: Vec<Message> = Vec::new();
        let mut after: Vec<Message> = Vec::new();
        let mut total_rows: usize = 0;
        let mut skipped: usize = 0;

        for shard in self.all_shards() {
            let prepared = match prepare_shard_query(
                shard,
                table_name,
                &mut shard_warnings,
                self.pool().and_then(|pool| pool.get(&shard.path)),
                self.sqlcipher_key.as_ref(),
            ) {
                Some(p) => p,
                None => continue,
            };
            let shard_path = shard.path.display().to_string();

            // Pivot: exact server_id match
            let sql_pivot = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.server_id = ?1",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_pivot,
                &[target_server_id],
                is_group,
                &query.talker,
                &shard_path,
                &mut pivot_messages,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );

            // Before: messages strictly before the pivot's compound key
            let sql_before = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE (m.sort_seq < ?1) \
                    OR (m.sort_seq = ?1 AND m.create_time < ?2) \
                    OR (m.sort_seq = ?1 AND m.create_time = ?2 AND m.server_id < ?3) \
                 ORDER BY m.sort_seq DESC, m.create_time DESC, m.server_id DESC \
                 LIMIT ?4",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_before,
                &[pivot_seq, pivot_ct, pivot_sid, context as i64],
                is_group,
                &query.talker,
                &shard_path,
                &mut before,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );

            // After: messages strictly after the pivot's compound key
            let sql_after = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE (m.sort_seq > ?1) \
                    OR (m.sort_seq = ?1 AND m.create_time > ?2) \
                    OR (m.sort_seq = ?1 AND m.create_time = ?2 AND m.server_id > ?3) \
                 ORDER BY m.sort_seq ASC, m.create_time ASC, m.server_id ASC \
                 LIMIT ?4",
                select_cols = prepared.select_cols,
                table = table_name,
            );
            query_shard_sql(
                &prepared,
                &sql_after,
                &[pivot_seq, pivot_ct, pivot_sid, context as i64],
                is_group,
                &query.talker,
                &shard_path,
                &mut after,
                &mut total_rows,
                &mut skipped,
                &mut shard_warnings,
            );
        }

        // Sort and truncate
        before.sort_unstable_by(|a, b| {
            (b.sort_seq, b.create_time, b.server_id).cmp(&(a.sort_seq, a.create_time, a.server_id))
        });
        before.truncate(context);
        before.reverse();

        after.sort_unstable_by_key(|m| (m.sort_seq, m.create_time, m.server_id));
        after.truncate(context);

        // Merge: before + pivot + after
        let mut all_messages = before;
        all_messages.append(&mut pivot_messages);
        all_messages.append(&mut after);

        apply_post_filters(&mut all_messages, &query.keyword, query.msg_type_filter);

        Ok(MessageQueryResult {
            items: all_messages,
            stats: QueryStats {
                total_rows,
                filtered_count: None,
                skipped,
            },
            shard_warnings,
        })
    }

    /// Bulk-query `MAX(sort_seq)` per talker across all shards.
    ///
    /// For each shard, opens one connection, discovers which `Msg_*` tables exist,
    /// and queries `MAX(sort_seq)` for each. Results are merged across shards
    /// (per-username max). Shard/table failures are logged and skipped.
    pub fn bulk_max_sort_seq(&self, known_usernames: &[String]) -> HashMap<String, i64> {
        // Build reverse map: table_name -> username
        let mut table_to_username: HashMap<String, &str> = HashMap::new();
        for u in known_usernames {
            let tbl = msg_table_name(u);
            table_to_username.insert(tbl, u.as_str());
        }

        // Pre-fill all known usernames with 0 so sessions without Msg_* tables
        // still get a per-talker baseline (rather than falling back to startup_watermark).
        let mut result: HashMap<String, i64> =
            known_usernames.iter().map(|u| (u.clone(), 0)).collect();

        for shard in self.all_shards() {
            let conn = if let Some(conn) = self.pool().and_then(|pool| pool.get(&shard.path)) {
                ShardConnection::Borrowed(conn)
            } else {
                match WechatDb::open_shard_with_key(shard, self.sqlcipher_key.as_ref()) {
                    Ok(conn) => ShardConnection::Owned(conn),
                    Err(e) => {
                        eprintln!(
                            "warn: bulk_max_sort_seq: open shard {} failed: {e}",
                            shard.path.display()
                        );
                        continue;
                    }
                }
            };
            let conn = conn.as_conn();

            // Discover Msg_* tables in this shard
            let mut stmt = match conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'Msg_%'")
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "warn: bulk_max_sort_seq: list tables in {} failed: {e}",
                        shard.path.display()
                    );
                    continue;
                }
            };

            let table_names: Vec<String> = match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    eprintln!(
                        "warn: bulk_max_sort_seq: query tables in {} failed: {e}",
                        shard.path.display()
                    );
                    continue;
                }
            };

            for tbl in &table_names {
                let username = match table_to_username.get(tbl.as_str()) {
                    Some(u) => *u,
                    None => continue,
                };

                let sql = format!("SELECT MAX(sort_seq) FROM [{}]", tbl);
                match conn.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0)) {
                    Ok(max_seq) => {
                        let seq = max_seq.unwrap_or(0);
                        let entry = result.entry(username.to_string()).or_insert(0);
                        if seq > *entry {
                            *entry = seq;
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "warn: bulk_max_sort_seq: MAX(sort_seq) for {tbl} in {} failed: {e}",
                            shard.path.display()
                        );
                    }
                }
            }
        }

        result
    }
}

/// Build per-shard SQL and parameters for a regular (non-anchor) query.
fn build_regular_shard_sql(
    mode: &RegularQueryMode,
    select_cols: &str,
    table_name: &str,
    order: SortOrder,
    start_time: i64,
    end_time: i64,
) -> (String, Vec<i64>) {
    match mode {
        RegularQueryMode::FullScan => {
            let sql = format!(
                "SELECT {select_cols} \
                 FROM [{table}] m \
                 LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                 WHERE m.create_time >= ?1 AND m.create_time <= ?2 \
                 ORDER BY m.sort_seq {order}, m.create_time {order}, m.server_id {order}",
                table = table_name,
                order = order.sql_keyword(),
            );
            (sql, vec![start_time, end_time])
        }
        RegularQueryMode::LimitPushdown {
            sql_limit,
            msg_type_filter,
        } => {
            if let Some(mt) = msg_type_filter {
                let sql = format!(
                    "SELECT {select_cols} \
                     FROM [{table}] m \
                     LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                     WHERE m.create_time >= ?1 AND m.create_time <= ?2 \
                       AND (m.local_type & 4294967295) = ?3 \
                     ORDER BY m.sort_seq {order} \
                     LIMIT ?4",
                    table = table_name,
                    order = order.sql_keyword(),
                );
                (
                    sql,
                    vec![start_time, end_time, *mt as i64, *sql_limit as i64],
                )
            } else {
                let sql = format!(
                    "SELECT {select_cols} \
                     FROM [{table}] m \
                     LEFT JOIN Name2Id n ON m.real_sender_id = n.rowid \
                     WHERE m.create_time >= ?1 AND m.create_time <= ?2 \
                     ORDER BY m.sort_seq {order} \
                     LIMIT ?3",
                    table = table_name,
                    order = order.sql_keyword(),
                );
                (sql, vec![start_time, end_time, *sql_limit as i64])
            }
        }
    }
}

/// Execute a SQL query on a prepared shard and collect decoded message rows.
#[allow(clippy::too_many_arguments)]
fn query_shard_sql(
    prepared: &PreparedShard<'_>,
    sql: &str,
    params: &[i64],
    is_group: bool,
    talker: &str,
    shard_path: &str,
    messages: &mut Vec<Message>,
    total_rows: &mut usize,
    skipped: &mut usize,
    shard_warnings: &mut Vec<ShardWarning>,
) {
    let mut stmt = match prepared.conn.as_conn().prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            shard_warnings.push(ShardWarning {
                path: shard_path.to_string(),
                reason: format!("prepare failed: {e}"),
            });
            return;
        }
    };

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect();

    let mut rows = match stmt.query(param_refs.as_slice()) {
        Ok(r) => r,
        Err(e) => {
            shard_warnings.push(ShardWarning {
                path: shard_path.to_string(),
                reason: format!("query failed: {e}"),
            });
            return;
        }
    };

    collect_rows(
        &mut rows,
        prepared,
        is_group,
        talker,
        shard_path,
        messages,
        total_rows,
        skipped,
        shard_warnings,
    );
}

/// Apply keyword and msg_type post-filters to a message vector.
fn apply_post_filters(
    messages: &mut Vec<Message>,
    keyword: &Option<String>,
    msg_type_filter: Option<u32>,
) {
    if let Some(ref kw) = keyword {
        let kw_lower = kw.to_lowercase();
        messages.retain(|m| content_contains_keyword(&m.content, &kw_lower));
    }
    if let Some(mt) = msg_type_filter {
        messages.retain(|m| m.msg_type == mt);
    }
}

/// Collect decoded message rows from a query result set into the accumulator vectors.
#[allow(clippy::too_many_arguments)]
fn collect_rows(
    rows: &mut rusqlite::Rows<'_>,
    prepared: &PreparedShard,
    is_group: bool,
    talker: &str,
    shard_path: &str,
    all_messages: &mut Vec<Message>,
    total_rows: &mut usize,
    skipped: &mut usize,
    shard_warnings: &mut Vec<ShardWarning>,
) {
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                *total_rows += 1;
                match decode_message_row(
                    row,
                    prepared.has_ct_col,
                    prepared.has_compress_col,
                    is_group,
                    talker,
                ) {
                    Ok(msg) => all_messages.push(msg),
                    Err(_) => {
                        *skipped += 1;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                shard_warnings.push(ShardWarning {
                    path: shard_path.to_string(),
                    reason: format!("row iteration failed: {e}"),
                });
                break;
            }
        }
    }
}

/// Decode a single message row from a rusqlite Row reference.
fn decode_message_row(
    row: &rusqlite::Row<'_>,
    has_ct_col: bool,
    has_compress_col: bool,
    is_group: bool,
    talker: &str,
) -> Result<Message, DbError> {
    let sort_seq: i64 = row.get(0)?;
    let server_id: i64 = row.get(1)?;
    let local_type: i64 = row.get(2)?;
    let sender_from_name2id: String = row.get(3)?;
    let create_time: i64 = row.get(4)?;

    // message_content can be Text or Blob
    let raw_content: Vec<u8> = match row.get_ref(5)? {
        ValueRef::Blob(b) => b.to_vec(),
        ValueRef::Text(b) => b.to_vec(),
        ValueRef::Null => Vec::new(),
        _ => Vec::new(),
    };

    // packed_info_data (BLOB, nullable)
    let packed_blob: Option<Vec<u8>> = match row.get_ref(6)? {
        ValueRef::Blob(b) => Some(b.to_vec()),
        ValueRef::Null => None,
        _ => None,
    };

    let status: i32 = row.get(7)?;

    // WCDB_CT column (optional, index=8 if present)
    let wcdb_ct: Option<i32> = if has_ct_col {
        row.get::<_, Option<i32>>(8)?
    } else {
        None
    };

    // compress_content column (optional BLOB, index depends on has_ct_col)
    let compress_content: Option<Vec<u8>> = if has_compress_col {
        let col_idx = 8 + (has_ct_col as usize);
        match row.get_ref(col_idx)? {
            ValueRef::Blob(b) if !b.is_empty() => Some(b.to_vec()),
            _ => None,
        }
    } else {
        None
    };

    // Decode content (zstd decompression if needed)
    let decoded_text = decode_content(&raw_content, wcdb_ct)?;

    // Group sender parsing: extract sender from content prefix
    let (sender, content_text) = parse_group_sender(is_group, decoded_text, sender_from_name2id);

    // Decode packed info
    let packed_info = packed_blob.as_deref().and_then(|b| {
        if b.is_empty() {
            None
        } else {
            decode_packed_info(b)
        }
    });

    // Split local_type into msg_type and sub_type
    let (msg_type, sub_type) = split_local_type(local_type);

    // Parse content into typed enum
    let content = parse_content(
        msg_type,
        sub_type,
        &content_text,
        server_id,
        packed_info.as_ref(),
        compress_content.as_deref(),
    );

    Ok(Message {
        sort_seq,
        server_id,
        msg_type,
        sub_type,
        sender,
        talker: talker.to_string(),
        create_time,
        content,
        status,
    })
}

/// Check if a MessageContent matches a keyword (case-insensitive).
fn content_contains_keyword(content: &crate::model::MessageContent, kw_lower: &str) -> bool {
    use crate::model::MessageContent;
    match content {
        MessageContent::Text(s) => s.to_lowercase().contains(kw_lower),
        MessageContent::Image { .. } => false,
        MessageContent::Voice => false,
        MessageContent::Video { .. } => false,
        MessageContent::Emoji(s) => s.to_lowercase().contains(kw_lower),
        MessageContent::Location(s) => s.to_lowercase().contains(kw_lower),
        MessageContent::Link { title, des, .. } => matches_any_opt(&[title, des], kw_lower),
        MessageContent::File { title, .. } => matches_opt(title, kw_lower),
        MessageContent::MiniProgram { title, .. } => matches_opt(title, kw_lower),
        MessageContent::MergedMessages { title, .. } => matches_opt(title, kw_lower),
        MessageContent::Quote {
            reply_text,
            refer_content,
            ..
        } => matches_any_opt(&[reply_text, refer_content], kw_lower),
        MessageContent::Transfer {
            amount_desc,
            pay_memo,
            ..
        } => matches_any_opt(&[amount_desc, pay_memo], kw_lower),
        MessageContent::RedEnvelope { title, .. } => matches_opt(title, kw_lower),
        MessageContent::ChannelVideo { title, .. } => matches_opt(title, kw_lower),
        MessageContent::Pat { .. } => false,
        MessageContent::AppGeneric { title, des, .. } => matches_any_opt(&[title, des], kw_lower),
        MessageContent::System(s) => s.to_lowercase().contains(kw_lower),
        MessageContent::Revoke(s) => s.to_lowercase().contains(kw_lower),
        MessageContent::Unknown { raw, .. } => raw.to_lowercase().contains(kw_lower),
    }
}

fn matches_opt(opt: &Option<String>, kw_lower: &str) -> bool {
    opt.as_deref()
        .is_some_and(|s| s.to_lowercase().contains(kw_lower))
}

fn matches_any_opt(opts: &[&Option<String>], kw_lower: &str) -> bool {
    opts.iter().any(|o| matches_opt(o, kw_lower))
}
