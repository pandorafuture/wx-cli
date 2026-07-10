use std::fs;
use std::path::Path;
use tempfile::TempDir;
use wx_media::MediaError;

/// Create a media_N.db with VoiceInfo table.
fn create_media_db(path: &Path, rows: &[(&str, &[u8])]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE VoiceInfo (
            svr_id TEXT,
            voice_data BLOB
        );",
    )
    .unwrap();
    let mut stmt = conn
        .prepare("INSERT INTO VoiceInfo (svr_id, voice_data) VALUES (?, ?)")
        .unwrap();
    for &(svr_id, data) in rows {
        stmt.execute(rusqlite::params![svr_id, data]).unwrap();
    }
}

fn create_indexed_media_db(path: &Path, rows: &[(i64, i64, i64, &str, &[u8])]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE VoiceInfo (
            chat_name_id INTEGER,
            create_time INTEGER,
            local_id INTEGER,
            svr_id TEXT,
            voice_data BLOB,
            data_index TEXT DEFAULT '0'
        );
        CREATE INDEX VoiceInfo_INDEX ON VoiceInfo(chat_name_id, svr_id);",
    )
    .unwrap();
    let mut stmt = conn
        .prepare(
            "INSERT INTO VoiceInfo (chat_name_id, create_time, local_id, svr_id, voice_data)
             VALUES (?, ?, ?, ?, ?)",
        )
        .unwrap();
    for &(chat_name_id, create_time, local_id, svr_id, data) in rows {
        stmt.execute(rusqlite::params![
            chat_name_id,
            create_time,
            local_id,
            svr_id,
            data
        ])
        .unwrap();
    }
}

#[test]
fn extract_voice_single_db() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();

    let silk_data = b"\x02\x23\x21SILK_V3";
    create_media_db(&media_dir.join("media_0.db"), &[("srv_001", silk_data)]);

    let blob = wx_media::extract_voice(&media_dir, "srv_001").unwrap();
    assert_eq!(blob.svr_id, "srv_001");
    assert_eq!(blob.data, silk_data);
}

#[test]
fn extract_voice_multi_db_scan() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();

    // Voice is in media_1.db, not media_0.db
    create_media_db(&media_dir.join("media_0.db"), &[]);
    create_media_db(
        &media_dir.join("media_1.db"),
        &[("srv_002", b"silk_audio_bytes")],
    );

    let blob = wx_media::extract_voice(&media_dir, "srv_002").unwrap();
    assert_eq!(blob.svr_id, "srv_002");
    assert_eq!(blob.data, b"silk_audio_bytes");
}

#[test]
fn extract_voice_not_found() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();
    create_media_db(&media_dir.join("media_0.db"), &[]);

    let result = wx_media::extract_voice(&media_dir, "nonexistent");
    assert!(matches!(result, Err(MediaError::LookupMiss(_))));
}

#[test]
fn extract_voice_skips_empty_blob() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();

    // First db has empty blob, second has real data
    create_media_db(
        &media_dir.join("media_0.db"),
        &[("srv_003", b"")], // empty blob
    );
    create_media_db(&media_dir.join("media_1.db"), &[("srv_003", b"real_data")]);

    let blob = wx_media::extract_voice(&media_dir, "srv_003").unwrap();
    assert_eq!(blob.data, b"real_data");
}

#[test]
fn extract_voice_no_media_dbs() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();
    // No media_*.db files

    let result = wx_media::extract_voice(&media_dir, "srv_001");
    assert!(matches!(result, Err(MediaError::NoMediaDbs(_))));
}

#[test]
fn extract_voice_compat_media_db() {
    // Test `media.db` (no numeric suffix) is also scanned
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    fs::create_dir_all(&media_dir).unwrap();

    create_media_db(&media_dir.join("media.db"), &[("srv_004", b"compat_voice")]);

    let blob = wx_media::extract_voice(&media_dir, "srv_004").unwrap();
    assert_eq!(blob.data, b"compat_voice");
}

#[test]
fn voice_query_with_conn_returns_non_empty_blob() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("media.db");
    create_media_db(&db_path, &[("srv_conn_1", b"blob_one")]);
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let blob = wx_media::extract_voice_with_conn(&conn, "srv_conn_1").unwrap();
    assert_eq!(blob.svr_id, "srv_conn_1");
    assert_eq!(blob.data, b"blob_one");
    assert_eq!(blob.chat_name_id, None);
}

#[test]
fn voice_query_with_conn_skips_empty_blob_rows() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("media.db");
    create_media_db(
        &db_path,
        &[("srv_conn_2", b""), ("srv_conn_2", b"blob_two")],
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let blob = wx_media::extract_voice_with_conn(&conn, "srv_conn_2").unwrap();
    assert_eq!(blob.data, b"blob_two");
    assert_eq!(blob.chat_name_id, None);
}

#[test]
fn voice_query_with_conn_returns_stable_not_found_errors() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("media.db");
    create_media_db(&db_path, &[]);
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let result = wx_media::extract_voice_with_conn(&conn, "missing");
    assert!(matches!(result, Err(MediaError::LookupMiss(s)) if s.contains("svr_id missing")));

    let no_table = rusqlite::Connection::open(tmp.path().join("no-table.db")).unwrap();
    let result = wx_media::extract_voice_with_conn(&no_table, "missing");
    assert!(matches!(result, Err(MediaError::SchemaMissing(s)) if s == "VoiceInfo table missing"));
}

#[test]
fn voice_query_with_conn_hint_returns_chat_name_id_from_indexed_lookup() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("media.db");
    create_indexed_media_db(&db_path, &[(55, 1000, 1, "srv_hint_1", b"hinted_blob")]);
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let blob = wx_media::extract_voice_with_conn_hint(&conn, "srv_hint_1", Some(55)).unwrap();
    assert_eq!(blob.svr_id, "srv_hint_1");
    assert_eq!(blob.data, b"hinted_blob");
    assert_eq!(blob.chat_name_id, Some(55));
}

#[test]
fn voice_query_with_conn_hint_falls_back_to_scan_when_hint_is_wrong() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("media.db");
    create_indexed_media_db(
        &db_path,
        &[
            (99, 1000, 1, "srv_hint_2", b""),
            (77, 1001, 2, "srv_hint_2", b"fallback_blob"),
        ],
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let blob = wx_media::extract_voice_with_conn_hint(&conn, "srv_hint_2", Some(55)).unwrap();
    assert_eq!(blob.svr_id, "srv_hint_2");
    assert_eq!(blob.data, b"fallback_blob");
    assert_eq!(blob.chat_name_id, Some(77));
}
