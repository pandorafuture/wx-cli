use std::collections::HashMap;

use rusqlite::types::Value;

use crate::contact_proto;
use crate::decode;
use crate::error::DbError;
use crate::model::{effective_limit, Contact, ContactQuery, QueryResult, QueryStats};
use crate::open::WechatDb;

impl WechatDb {
    /// Query contacts, optionally filtered by keyword.
    ///
    /// When a keyword is present, filtering is done at the application level
    /// (after decoding extra_buffer) so that phone, labels, signature, and
    /// region can also be searched. SQL-level LIMIT/OFFSET is only used for
    /// the fast path (no keyword).
    pub fn query_contacts(&self, query: &ContactQuery) -> Result<QueryResult<Contact>, DbError> {
        let limit = effective_limit(query.limit);
        // Ensure label_map is loaded (lazy init with error propagation).
        {
            let guard = self.label_cache.read().unwrap();
            if guard.is_none() {
                drop(guard);
                let map = self.load_label_map()?;
                self.label_cache.write().unwrap().replace(map);
            }
        }
        let guard = self.label_cache.read().unwrap();
        let label_map = guard.as_ref().unwrap();

        if query.keyword.is_some() {
            self.query_contacts_with_keyword(query, limit, label_map)
        } else {
            self.query_contacts_fast(query, limit, label_map)
        }
    }

    /// Fast path: no keyword, use SQL-level LIMIT/OFFSET.
    fn query_contacts_fast(
        &self,
        query: &ContactQuery,
        limit: usize,
        label_map: &HashMap<String, String>,
    ) -> Result<QueryResult<Contact>, DbError> {
        let avatar_expr = self.contact_avatar_select_expr()?;
        let total_rows: usize =
            self.contact_conn
                .query_row("SELECT COUNT(*) FROM contact", [], |row| {
                    row.get::<_, i64>(0)
                })? as usize;

        let sql = format!(
            "SELECT username, alias, remark, nick_name, description, extra_buffer, \
             {avatar_expr} AS avatar_url \
             FROM contact ORDER BY username ASC LIMIT ?1 OFFSET ?2"
        );
        let mut stmt = self.contact_conn.prepare(&sql)?;
        let rows = stmt.query_map(
            [
                Value::Integer(limit as i64),
                Value::Integer(query.offset as i64),
            ],
            |row| self.map_contact_row(row, label_map),
        )?;

        let items: Vec<Contact> = rows.filter_map(|r| r.ok()).collect();

        Ok(QueryResult {
            items,
            stats: QueryStats {
                total_rows,
                filtered_count: None,
                skipped: 0,
            },
        })
    }

    /// Keyword path: fetch all contacts, decode extra_buffer, filter in Rust.
    fn query_contacts_with_keyword(
        &self,
        query: &ContactQuery,
        limit: usize,
        label_map: &HashMap<String, String>,
    ) -> Result<QueryResult<Contact>, DbError> {
        let kw_lower = query.keyword.as_ref().unwrap().to_lowercase();
        let avatar_expr = self.contact_avatar_select_expr()?;

        let total_rows: usize =
            self.contact_conn
                .query_row("SELECT COUNT(*) FROM contact", [], |row| {
                    row.get::<_, i64>(0)
                })? as usize;

        let sql = format!(
            "SELECT username, alias, remark, nick_name, description, extra_buffer, \
             {avatar_expr} AS avatar_url \
             FROM contact ORDER BY username ASC"
        );
        let mut stmt = self.contact_conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| self.map_contact_row(row, label_map))?;

        let all_contacts: Vec<Contact> = rows.filter_map(|r| r.ok()).collect();

        let matched: Vec<Contact> = all_contacts
            .into_iter()
            .filter(|c| contact_matches_keyword(c, &kw_lower))
            .collect();

        let filtered_count = matched.len();
        let items: Vec<Contact> = matched.into_iter().skip(query.offset).take(limit).collect();

        Ok(QueryResult {
            items,
            stats: QueryStats {
                total_rows,
                filtered_count: Some(filtered_count),
                skipped: 0,
            },
        })
    }

    /// Map a single row to a Contact, decoding extra_buffer and resolving labels.
    fn map_contact_row(
        &self,
        row: &rusqlite::Row<'_>,
        label_map: &HashMap<String, String>,
    ) -> rusqlite::Result<Contact> {
        let user_name: String = row.get(0)?;
        let alias: String = row.get::<_, String>(1).unwrap_or_default();
        let remark: String = row.get::<_, String>(2).unwrap_or_default();
        let nick_name: String = row.get::<_, String>(3).unwrap_or_default();
        let memo: Option<String> = row.get::<_, Option<String>>(4).unwrap_or(None);
        let extra_buffer: Vec<u8> = row.get::<_, Vec<u8>>(5).unwrap_or_default();
        let avatar_url: Option<String> = row
            .get::<_, Option<String>>(6)
            .unwrap_or(None)
            .filter(|value| !value.is_empty());

        let extra = contact_proto::decode_extra_buffer(&extra_buffer);

        let labels: Vec<String> = extra
            .label_ids
            .iter()
            .filter_map(|id| label_map.get(id).cloned())
            .collect();

        Ok(Contact {
            user_name,
            alias,
            remark,
            nick_name,
            memo: memo.filter(|s| !s.is_empty()),
            gender: extra.gender,
            signature: extra.signature,
            region: extra.region,
            source_scene: extra.source_scene,
            phone: extra.phone,
            labels,
            avatar_url,
        })
    }

    /// Build a compatible avatar expression for WeChat database variants.
    /// Older fixtures and database versions may not contain either column.
    fn contact_avatar_select_expr(&self) -> Result<&'static str, DbError> {
        let has_small =
            decode::check_column_exists(&self.contact_conn, "contact", "small_head_url")?;
        let has_big = decode::check_column_exists(&self.contact_conn, "contact", "big_head_url")?;

        Ok(match (has_small, has_big) {
            (true, true) => "COALESCE(NULLIF(small_head_url, ''), NULLIF(big_head_url, ''))",
            (true, false) => "NULLIF(small_head_url, '')",
            (false, true) => "NULLIF(big_head_url, '')",
            (false, false) => "NULL",
        })
    }

    /// Load `contact_label` table into a HashMap<label_id, label_name>.
    /// Returns empty map if the table does not exist.
    fn load_label_map(&self) -> Result<HashMap<String, String>, DbError> {
        if !decode::table_exists(&self.contact_conn, "contact_label")? {
            return Ok(HashMap::new());
        }

        let mut stmt = self
            .contact_conn
            .prepare("SELECT label_id_, label_name_ FROM contact_label")?;
        let rows = stmt.query_map([], |row| {
            let id_val: Value = row.get(0)?;
            let id = match id_val {
                Value::Integer(n) => n.to_string(),
                Value::Text(s) => s,
                _ => String::new(),
            };
            let name: String = row.get::<_, String>(1).unwrap_or_default();
            Ok((id, name))
        })?;

        let mut map = HashMap::new();
        for (id, name) in rows.flatten() {
            if !id.is_empty() {
                map.insert(id, name);
            }
        }
        Ok(map)
    }
}

/// Check if a contact matches a keyword (case-insensitive) across all fields.
fn contact_matches_keyword(c: &Contact, kw_lower: &str) -> bool {
    if c.user_name.to_lowercase().contains(kw_lower) {
        return true;
    }
    if c.alias.to_lowercase().contains(kw_lower) {
        return true;
    }
    if c.remark.to_lowercase().contains(kw_lower) {
        return true;
    }
    if c.nick_name.to_lowercase().contains(kw_lower) {
        return true;
    }
    if let Some(ref memo) = c.memo {
        if memo.to_lowercase().contains(kw_lower) {
            return true;
        }
    }
    if let Some(ref phone) = c.phone {
        if phone.to_lowercase().contains(kw_lower) {
            return true;
        }
    }
    if let Some(ref sig) = c.signature {
        if sig.to_lowercase().contains(kw_lower) {
            return true;
        }
    }
    if let Some(ref region) = c.region {
        if region.to_lowercase().contains(kw_lower) {
            return true;
        }
    }
    for label in &c.labels {
        if label.to_lowercase().contains(kw_lower) {
            return true;
        }
    }
    false
}
