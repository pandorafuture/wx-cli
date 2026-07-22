use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use tempfile::TempDir;
use wx_db::test_ddl;
use wx_db::{ContactQuery, DbError, MessageQuery, SessionQuery, SortOrder, WechatDb};

// ---- helpers ----

/// Create a minimal fixture directory with contact.db, session.db, and message_0.db.
fn create_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();

    // contact/contact.db
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));

    // session/session.db
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    create_session_db(&session_dir.join("session.db"));

    // message/message_0.db
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    dir
}

fn create_contact_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    test_ddl::create_test_contact_table(&conn);
    test_ddl::create_test_contact_label_table(&conn);
    conn.execute(
        "INSERT INTO contact (username, alias, remark, nick_name) VALUES (?1, ?2, ?3, ?4)",
        params!["wxid_alice", "alice_a", "Alice Remark", "Alice"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO contact (username, alias, remark, nick_name) VALUES (?1, ?2, ?3, ?4)",
        params!["wxid_bob", "bob_b", "Bob Test Remark", "Bob"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO contact (username, alias, remark, nick_name) VALUES (?1, ?2, ?3, ?4)",
        params!["wxid_test_user", "", "Charlie", "Charlie Nick"],
    )
    .unwrap();
}

fn create_session_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    test_ddl::create_test_session_table_extended(&conn);

    // Session 1: plain text summary
    conn.execute(
        "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
        params!["wxid_alice", 2000, "hello from alice"],
    )
    .unwrap();

    // Session 2: zstd-compressed blob summary
    let compressed = zstd::encode_all(&b"compressed summary"[..], 0).unwrap();
    conn.execute(
        "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
        params!["wxid_bob", 3000, compressed],
    )
    .unwrap();
}

fn create_message_shard(path: &Path, timestamp: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE Timestamp (timestamp INTEGER);")
        .unwrap();
    conn.execute("INSERT INTO Timestamp VALUES (?1)", params![timestamp])
        .unwrap();
}

fn create_auxiliary_message_db(path: &Path) {
    let _conn = Connection::open(path).unwrap();
}

// ---- existing smoke test ----

#[test]
fn open_nonexistent_dir_returns_not_found() {
    let result = WechatDb::open("/tmp/nonexistent-wx-db-dir-that-does-not-exist");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, DbError::NotFound(_)),
        "expected NotFound, got: {err:?}"
    );
}

// ---- open tests ----

#[test]
fn contacts_sessions_open_valid_dir() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();
    // Should have exactly 1 shard
    assert!(format!("{db:?}").contains("shards"));
}

#[test]
fn contacts_sessions_open_ignores_auxiliary_message_dbs() {
    let dir = create_fixture();
    let msg_dir = dir.path().join("message");
    create_auxiliary_message_db(&msg_dir.join("message_fts.db"));
    create_auxiliary_message_db(&msg_dir.join("message_resource.db"));

    let db = WechatDb::open(dir.path()).unwrap();
    let debug = format!("{db:?}");
    assert!(debug.contains("message_0.db"));
    assert!(!debug.contains("message_fts.db"));
    assert!(!debug.contains("message_resource.db"));
}

#[test]
fn contacts_sessions_open_only_auxiliary_message_dbs_has_empty_shards() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    create_session_db(&session_dir.join("session.db"));
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_auxiliary_message_db(&msg_dir.join("message_fts.db"));
    create_auxiliary_message_db(&msg_dir.join("message_resource.db"));

    let db = WechatDb::open(base).unwrap();
    assert!(format!("{db:?}").contains("shards: []"));
}

#[test]
fn contacts_sessions_query_messages_with_only_auxiliary_dbs_returns_no_shards() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    create_session_db(&session_dir.join("session.db"));
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_auxiliary_message_db(&msg_dir.join("message_fts.db"));
    create_auxiliary_message_db(&msg_dir.join("message_resource.db"));

    let db = WechatDb::open(base).unwrap();
    let result = db.query_messages(&MessageQuery::for_talker("wxid_alice"));
    assert!(matches!(result, Err(DbError::NoShards)));
}

#[test]
fn contacts_sessions_open_missing_contact_db() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    // Create session and message but NOT contact
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    create_session_db(&session_dir.join("session.db"));
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    let result = WechatDb::open(base);
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

#[test]
fn contacts_sessions_open_missing_session_db() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    // Create contact and message but NOT session
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    let result = WechatDb::open(base);
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

#[test]
fn contacts_sessions_open_no_shards() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    create_session_db(&session_dir.join("session.db"));
    // message dir exists but is empty
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();

    let db = WechatDb::open(base).unwrap();
    assert!(format!("{db:?}").contains("shards: []"));
}

// ---- contacts tests ----

#[test]
fn contacts_sessions_query_contacts_all() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_contacts(&ContactQuery::new()).unwrap();
    assert_eq!(result.items.len(), 3);
    assert_eq!(result.stats.total_rows, 3);
    assert_eq!(result.stats.skipped, 0);
}

#[test]
fn contacts_sessions_query_contacts_keyword_username() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    // "test" should match wxid_test_user (userName) and Bob Test Remark (remark)
    let result = db
        .query_contacts(&ContactQuery::new().keyword("test"))
        .unwrap();
    assert_eq!(result.items.len(), 2); // wxid_test_user + Bob Test Remark
}

#[test]
fn contacts_sessions_query_contacts_keyword_alias() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    // "alice_a" should match alias of first contact
    let result = db
        .query_contacts(&ContactQuery::new().keyword("alice_a"))
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].user_name, "wxid_alice");
}

#[test]
fn contacts_sessions_query_contacts_keyword_remark() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    // "Charlie" should match remark of wxid_test_user
    let result = db
        .query_contacts(&ContactQuery::new().keyword("Charlie"))
        .unwrap();
    assert!(result.items.iter().any(|c| c.user_name == "wxid_test_user"));
}

#[test]
fn contacts_sessions_query_contacts_keyword_nick_name() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    // "Nick" should match nick_name of wxid_test_user ("Charlie Nick")
    let result = db
        .query_contacts(&ContactQuery::new().keyword("Nick"))
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].user_name, "wxid_test_user");
}

#[test]
fn contacts_sessions_query_contacts_limit() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_contacts(&ContactQuery::new().limit(1)).unwrap();
    assert_eq!(result.items.len(), 1);
}

#[test]
fn contacts_sessions_query_contacts_avatar_url() {
    let dir = create_fixture();
    let contact_path = dir.path().join("contact").join("contact.db");
    let conn = Connection::open(contact_path).unwrap();
    conn.execute_batch(
        "ALTER TABLE contact ADD COLUMN small_head_url TEXT;
         ALTER TABLE contact ADD COLUMN big_head_url TEXT;",
    )
    .unwrap();
    conn.execute(
        "UPDATE contact SET small_head_url = ?1, big_head_url = ?2 WHERE username = ?3",
        params![
            "https://avatar.example/alice-small.jpg",
            "https://avatar.example/alice-big.jpg",
            "wxid_alice"
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE contact SET big_head_url = ?1 WHERE username = ?2",
        params!["https://avatar.example/bob-big.jpg", "wxid_bob"],
    )
    .unwrap();
    drop(conn);

    let db = WechatDb::open(dir.path()).unwrap();
    let result = db.query_contacts(&ContactQuery::new()).unwrap();
    let alice = result
        .items
        .iter()
        .find(|contact| contact.user_name == "wxid_alice")
        .unwrap();
    let bob = result
        .items
        .iter()
        .find(|contact| contact.user_name == "wxid_bob")
        .unwrap();

    assert_eq!(
        alice.avatar_url.as_deref(),
        Some("https://avatar.example/alice-small.jpg")
    );
    assert_eq!(
        bob.avatar_url.as_deref(),
        Some("https://avatar.example/bob-big.jpg")
    );
}

// ---- sessions tests ----

#[test]
fn contacts_sessions_query_sessions_all() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.stats.skipped, 0);

    // Should be sorted by sort_timestamp DESC (bob=3000 first, alice=2000 second)
    assert_eq!(result.items[0].username, "wxid_bob");
    assert_eq!(result.items[0].sort_timestamp, 3000);
    assert_eq!(result.items[1].username, "wxid_alice");
    assert_eq!(result.items[1].sort_timestamp, 2000);
}

#[test]
fn contacts_sessions_query_sessions_text_summary() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    // Alice has plain text summary
    let alice = result
        .items
        .iter()
        .find(|s| s.username == "wxid_alice")
        .unwrap();
    assert_eq!(alice.summary, "hello from alice");
}

#[test]
fn contacts_sessions_query_sessions_zstd_summary() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    // Bob has zstd-compressed summary
    let bob = result
        .items
        .iter()
        .find(|s| s.username == "wxid_bob")
        .unwrap();
    assert_eq!(bob.summary, "compressed summary");
}

#[test]
fn contacts_sessions_query_sessions_limit() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db.query_sessions(&SessionQuery::new().limit(1)).unwrap();
    assert_eq!(result.items.len(), 1);
    // Should be the most recent (bob, 3000)
    assert_eq!(result.items[0].username, "wxid_bob");
}

#[test]
fn reopen_sessions_refreshes_connection() {
    let dir = create_fixture();
    let mut db = WechatDb::open(dir.path()).unwrap();

    // Initial query
    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    assert_eq!(result.items.len(), 2);

    // Modify session.db externally (add a new session)
    let session_path = dir.path().join("session").join("session.db");
    let conn = Connection::open(&session_path).unwrap();
    conn.execute(
        "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
        params!["wxid_charlie", 5000, "hello from charlie"],
    )
    .unwrap();
    drop(conn);

    // Without reopen, the old connection won't see the new row (read-only + WAL)
    // After reopen, it should see the new data
    db.reopen_sessions().unwrap();

    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    assert_eq!(result.items.len(), 3);
    assert_eq!(result.items[0].username, "wxid_charlie");
    assert_eq!(result.items[0].sort_timestamp, 5000);
}

#[test]
fn query_sessions_with_content_fields() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();

    // contact.db (required by WechatDb::open)
    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));

    // session.db with content fields populated
    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    {
        let conn = Connection::open(session_dir.join("session.db")).unwrap();
        test_ddl::create_test_session_table_extended(&conn);
        conn.execute(
            "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "wxid_alice",
                1000,
                "hi there",
                1,
                "wxid_sender",
                "Sender Name"
            ],
        )
        .unwrap();
    }

    // message shard (required by WechatDb::open)
    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    let db = WechatDb::open(base).unwrap();
    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    assert_eq!(result.items.len(), 1);

    let session = &result.items[0];
    assert_eq!(session.last_msg_type, Some(1));
    assert_eq!(session.last_msg_sender.as_deref(), Some("wxid_sender"));
    assert_eq!(
        session.last_sender_display_name.as_deref(),
        Some("Sender Name")
    );
}

#[test]
fn query_sessions_strips_group_summary_prefix() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();

    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));

    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    {
        let conn = Connection::open(session_dir.join("session.db")).unwrap();
        test_ddl::create_test_session_table_extended(&conn);
        // Group chat: prefix should be stripped
        conn.execute(
            "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
            params!["group@chatroom", 2000, "wxid_abc:\nhello"],
        )
        .unwrap();
        // 1-on-1 chat: prefix should NOT be stripped
        conn.execute(
            "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
            params!["wxid_alice", 1000, "some_id:\nwhatever"],
        )
        .unwrap();
    }

    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    let db = WechatDb::open(base).unwrap();
    let result = db.query_sessions(&SessionQuery::new().limit(10)).unwrap();
    assert_eq!(result.items.len(), 2);

    // Group chat (sort_timestamp=2000 is first, DESC order)
    let group = result
        .items
        .iter()
        .find(|s| s.username == "group@chatroom")
        .unwrap();
    assert_eq!(
        group.summary, "hello",
        "group summary prefix should be stripped"
    );

    // 1-on-1 chat
    let dm = result
        .items
        .iter()
        .find(|s| s.username == "wxid_alice")
        .unwrap();
    assert_eq!(
        dm.summary, "some_id:\nwhatever",
        "non-group summary should NOT be stripped"
    );
}

#[test]
fn query_sessions_asc_order() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    let result = db
        .query_sessions(&SessionQuery::new().limit(10).order(SortOrder::Asc))
        .unwrap();
    assert_eq!(result.items.len(), 2);
    // ASC: alice (2000) first, bob (3000) second
    assert_eq!(result.items[0].username, "wxid_alice");
    assert_eq!(result.items[0].sort_timestamp, 2000);
    assert_eq!(result.items[1].username, "wxid_bob");
    assert_eq!(result.items[1].sort_timestamp, 3000);
}

#[test]
fn contacts_pagination_stability() {
    let dir = create_fixture();
    let db = WechatDb::open(dir.path()).unwrap();

    // Page 1: offset=0, limit=2
    let page1 = db
        .query_contacts(&ContactQuery::new().limit(2).offset(0))
        .unwrap();
    // Page 2: offset=2, limit=2
    let page2 = db
        .query_contacts(&ContactQuery::new().limit(2).offset(2))
        .unwrap();

    assert_eq!(page1.items.len(), 2);
    assert_eq!(page2.items.len(), 1); // 3 total, offset 2 → 1 remaining

    // Ensure no overlap
    let page1_names: Vec<&str> = page1.items.iter().map(|c| c.user_name.as_str()).collect();
    let page2_names: Vec<&str> = page2.items.iter().map(|c| c.user_name.as_str()).collect();
    for name in &page2_names {
        assert!(
            !page1_names.contains(name),
            "contact {name} appears in both pages"
        );
    }
}

#[test]
fn sessions_pagination_stability_same_timestamp() {
    // Sessions with identical sort_timestamp must still paginate deterministically
    let dir = TempDir::new().unwrap();
    let base = dir.path();

    let contact_dir = base.join("contact");
    fs::create_dir_all(&contact_dir).unwrap();
    create_contact_db(&contact_dir.join("contact.db"));

    let session_dir = base.join("session");
    fs::create_dir_all(&session_dir).unwrap();
    {
        let conn = Connection::open(session_dir.join("session.db")).unwrap();
        test_ddl::create_test_session_table_extended(&conn);
        // 3 sessions all with the same timestamp
        for name in &["wxid_aaa", "wxid_bbb", "wxid_ccc"] {
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
                params![name, 5000_i64, format!("summary for {name}")],
            )
            .unwrap();
        }
    }

    let msg_dir = base.join("message");
    fs::create_dir_all(&msg_dir).unwrap();
    create_message_shard(&msg_dir.join("message_0.db"), 1000);

    let db = WechatDb::open(base).unwrap();

    // Page 1: offset=0, limit=2
    let page1 = db
        .query_sessions(&SessionQuery::new().limit(2).offset(0))
        .unwrap();
    // Page 2: offset=2, limit=2
    let page2 = db
        .query_sessions(&SessionQuery::new().limit(2).offset(2))
        .unwrap();

    assert_eq!(page1.items.len(), 2);
    assert_eq!(page2.items.len(), 1);

    // No overlap
    let p1: Vec<&str> = page1.items.iter().map(|s| s.username.as_str()).collect();
    let p2: Vec<&str> = page2.items.iter().map(|s| s.username.as_str()).collect();
    for name in &p2 {
        assert!(!p1.contains(name), "session {name} appears in both pages");
    }
}
