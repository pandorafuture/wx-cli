use std::path::Path;

use crate::error::MediaError;
use crate::types::HardlinkEntry;

/// Query `hardlink.db` for image/video/file entries by MD5 or file_name prefix.
///
/// Automatically falls back from v3 to v4 table variants.
/// For `"image"` type, results are sorted with `_h.dat` (high quality) first.
pub fn query_hardlink(
    db_path: &Path,
    media_type: &str,
    key: &str,
) -> Result<Vec<HardlinkEntry>, MediaError> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    query_hardlink_with_conn(&conn, media_type, key)
}

pub fn query_hardlink_with_conn(
    conn: &rusqlite::Connection,
    media_type: &str,
    key: &str,
) -> Result<Vec<HardlinkEntry>, MediaError> {
    let table = resolve_table(conn, media_type)?;

    let query = format!(
        "SELECT f.md5, f.file_name, f.file_size, f.modify_time,
                IFNULL(d1.username, ''), IFNULL(d2.username, '')
         FROM {} f
         LEFT JOIN dir2id d1 ON d1.rowid = f.dir1
         LEFT JOIN dir2id d2 ON d2.rowid = f.dir2
         WHERE f.md5 = ?1 OR f.file_name LIKE ?2 || '%'",
        table
    );

    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map(rusqlite::params![key, key], |row| {
        Ok(HardlinkEntry {
            media_type: media_type.to_string(),
            md5: row.get(0)?,
            file_name: row.get(1)?,
            file_size: row.get(2)?,
            modify_time: row.get(3)?,
            dir1: row.get(4)?,
            dir2: row.get(5)?,
        })
    })?;

    let mut entries: Vec<HardlinkEntry> = rows.filter_map(|r| r.ok()).collect();

    if entries.is_empty() {
        return Err(MediaError::LookupMiss(format!(
            "no {} entries found for key '{}'",
            media_type, key,
        )));
    }

    // For images, sort _h.dat (high quality) first
    if media_type == "image" {
        entries.sort_by(|a, b| {
            let a_h = a.file_name.contains("_h.");
            let b_h = b.file_name.contains("_h.");
            b_h.cmp(&a_h) // true (has _h) sorts before false
        });
    }

    Ok(entries)
}

/// Resolve the correct table name, falling back from v3 to v4.
fn resolve_table(conn: &rusqlite::Connection, media_type: &str) -> Result<String, MediaError> {
    let prefix = match media_type {
        "image" => "image",
        "video" => "video",
        "file" => "file",
        other => {
            return Err(MediaError::InvalidFormat {
                reason: format!("unsupported media type: {}", other),
            })
        }
    };

    let v3 = format!("{}_hardlink_info_v3", prefix);
    if table_exists(conn, &v3) {
        return Ok(v3);
    }

    let v4 = format!("{}_hardlink_info_v4", prefix);
    if table_exists(conn, &v4) {
        return Ok(v4);
    }

    Err(MediaError::SchemaMissing(format!(
        "no hardlink table found for type '{}' (tried {} and {})",
        media_type, v3, v4
    )))
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}
