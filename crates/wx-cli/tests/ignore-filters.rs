use std::fs;
use std::path::Path;
use std::process::Command;

use rusqlite::{params, Connection};
use serde_json::Value;
use tempfile::TempDir;
use wx_db::encode_extra_buffer_for_test;

const TEST_KEY_HEX: &str = "abababababababababababababababababababababababababababababababab";
const ACCOUNT_SCOPE_A: &str = "wxid_scope_a_ab12";
const ACCOUNT_SCOPE_B: &str = "wxid_scope_b_cd34";
const ACCOUNT_TAGS: &str = "wxid_scope_tags_ef56";
const ACCOUNT_SENDERS: &str = "wxid_scope_senders_gh78";

const TALKER_ALICE: &str = "wxid_alice";
const TALKER_BOB: &str = "wxid_bob";
const TALKER_TAGGED: &str = "wxid_hidden_tagged";
const TALKER_GROUP: &str = "team@chatroom";
const TALKER_SPAM: &str = "wxid_spam";

const TABLE_ALICE: &str = "Msg_29a6db07e8bbdb53f5d54cc3c309f3f1";
const TABLE_BOB: &str = "Msg_8a7b11f2fd24e19a60664a5fe5d56342";
const TABLE_GROUP: &str = "Msg_adcd19623ae4b1f076f9731d6c37b266";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wx-cli")
}

#[test]
fn ignore_rules_are_scoped_by_account_id() {
    let fixture = create_fixture();
    let scope_a_dir = fixture.account_dir(ACCOUNT_SCOPE_A);
    let scope_b_dir = fixture.account_dir(ACCOUNT_SCOPE_B);

    let scope_a = run_json(
        fixture.path(),
        &[
            "contacts",
            "--data-dir",
            scope_a_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    assert_contact_ids(&scope_a, &[TALKER_BOB, TALKER_TAGGED]);

    let scope_b = run_json(
        fixture.path(),
        &[
            "contacts",
            "--data-dir",
            scope_b_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    assert_contact_ids(&scope_b, &[TALKER_ALICE, TALKER_TAGGED]);
}

#[test]
fn ignore_tags_hide_matching_contact_at_both_talker_and_sender_level() {
    let fixture = create_fixture();
    let tags_dir = fixture.account_dir(ACCOUNT_TAGS);

    let contacts = run_json(
        fixture.path(),
        &[
            "contacts",
            "--data-dir",
            tags_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    assert_contact_ids(&contacts, &[TALKER_ALICE, TALKER_BOB]);

    // Group messages from tagged contact should be sender-level filtered
    let group_messages = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_GROUP,
            "--data-dir",
            tags_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = group_messages["items"]
        .as_array()
        .expect("query items array");
    // The only group message is from wxid_hidden_tagged; it should be filtered out
    assert_eq!(
        items.len(),
        0,
        "tagged contact's group messages should be sender-level filtered: {group_messages}"
    );

    // Session should show placeholder for group where last sender is tagged
    let sessions = run_json(
        fixture.path(),
        &[
            "sessions",
            "--data-dir",
            tags_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = sessions["items"].as_array().expect("sessions items array");
    let group_session = items
        .iter()
        .find(|item| item["username"].as_str() == Some(TALKER_GROUP));
    assert!(
        group_session.is_some(),
        "group session should stay visible: {sessions}"
    );
    let group_session = group_session.unwrap();
    assert_eq!(
        group_session["summary"].as_str(),
        Some("[消息已隐藏]"),
        "summary should be placeholder when last sender is tag-hidden: {group_session}"
    );
    assert!(
        group_session["last_msg_sender"].is_null(),
        "last_msg_sender should be null when tag-hidden: {group_session}"
    );
}

#[test]
fn hidden_talker_defaults_to_not_found_and_show_hidden_restores_query() {
    let fixture = create_fixture();
    let scope_a_dir = fixture.account_dir(ACCOUNT_SCOPE_A);

    let hidden = run_failure(
        fixture.path(),
        &[
            "query",
            TALKER_ALICE,
            "--data-dir",
            scope_a_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    assert!(hidden.contains("not found"), "{hidden}");
    assert!(!hidden.contains("hidden"), "{hidden}");

    let visible = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_ALICE,
            "--data-dir",
            scope_a_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--show-hidden",
            "--format",
            "json",
        ],
    );
    let items = visible["items"].as_array().expect("query items array");
    assert_eq!(items.len(), 1, "{visible}");
    assert_eq!(items[0]["talker"], TALKER_ALICE);
    assert_eq!(items[0]["server_id"], 2001);
}

#[test]
fn contacts_and_sessions_paging_metadata_reflect_visible_result_sets() {
    let fixture = create_fixture();
    let scope_a_dir = fixture.account_dir(ACCOUNT_SCOPE_A);

    let contacts = run_json(
        fixture.path(),
        &[
            "contacts",
            "--data-dir",
            scope_a_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--limit",
            "1",
            "--offset",
            "1",
            "--format",
            "json",
        ],
    );
    assert_eq!(contacts["paging"]["total"], 2);
    assert_eq!(contacts["paging"]["returned"], 1);
    assert_eq!(contacts["paging"]["has_more"], false);
    assert_eq!(contacts["stats"]["scanned"], 3);
    assert_contact_ids(&contacts, &[TALKER_TAGGED]);

    let sessions = run_json(
        fixture.path(),
        &[
            "sessions",
            "--data-dir",
            scope_a_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--limit",
            "1",
            "--offset",
            "1",
            "--format",
            "json",
        ],
    );
    assert_eq!(sessions["paging"]["total"], 2);
    assert_eq!(sessions["paging"]["returned"], 1);
    assert_eq!(sessions["paging"]["has_more"], false);
    assert_eq!(sessions["stats"]["scanned"], 3);
    let items = sessions["items"].as_array().expect("sessions items array");
    assert_eq!(items.len(), 1, "{sessions}");
    assert_eq!(items[0]["username"], TALKER_GROUP);
}

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn path(&self) -> &Path {
        self.root.path()
    }

    fn account_dir(&self, account_id: &str) -> String {
        self.root
            .path()
            .join(account_id)
            .to_str()
            .expect("fixture path utf8")
            .to_string()
    }
}

fn create_fixture() -> Fixture {
    let root = TempDir::new().expect("tempdir");
    for account_id in [ACCOUNT_SCOPE_A, ACCOUNT_SCOPE_B, ACCOUNT_TAGS] {
        create_account(root.path(), account_id);
    }
    create_sender_account(root.path());
    write_settings(root.path());
    Fixture { root }
}

fn write_settings(home: &Path) {
    let config_dir = home.join(".config").join("wechat-utils");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("settings.toml"),
        format!(
            r#"[accounts."{ACCOUNT_SCOPE_A}"]
ignore_contacts = ["{TALKER_ALICE}"]

[accounts."{ACCOUNT_SCOPE_B}"]
ignore_contacts = ["{TALKER_BOB}"]

[accounts."{ACCOUNT_TAGS}"]
ignore_tags = ["Sensitive"]

[accounts."{ACCOUNT_SENDERS}"]
ignore_contacts = ["{TALKER_SPAM}"]
"#
        ),
    )
    .expect("write settings");
}

fn create_account(root: &Path, account_id: &str) {
    let account_dir = root.join(account_id);
    let db_root = account_dir.join("db_storage");
    let contact_dir = db_root.join("contact");
    let session_dir = db_root.join("session");
    let message_dir = db_root.join("message");

    fs::create_dir_all(&contact_dir).expect("create contact dir");
    fs::create_dir_all(&session_dir).expect("create session dir");
    fs::create_dir_all(&message_dir).expect("create message dir");

    let raw_key = test_raw_key();
    create_encrypted_contact_db(&contact_dir.join("contact.db"), &raw_key);
    create_encrypted_session_db(&session_dir.join("session.db"), &raw_key);
    create_encrypted_message_db(&message_dir.join("message_0.db"), &raw_key);
}

fn create_encrypted_contact_db(path: &Path, raw_key: &[u8; 32]) {
    create_encrypted_db(
        path,
        raw_key,
        "CREATE TABLE contact (
            username TEXT PRIMARY KEY,
            alias TEXT DEFAULT '',
            remark TEXT DEFAULT '',
            nick_name TEXT DEFAULT '',
            description TEXT DEFAULT NULL,
            extra_buffer BLOB DEFAULT NULL
        );
        CREATE TABLE contact_label (
            label_id_ TEXT,
            label_name_ TEXT,
            sort_order_ INTEGER
        );",
        |conn| {
            conn.execute(
                "INSERT INTO contact_label VALUES (?1, ?2, ?3)",
                params!["1", "Sensitive", 0],
            )
            .expect("insert label");

            conn.execute(
                "INSERT INTO contact (username, nick_name) VALUES (?1, ?2)",
                params![TALKER_ALICE, "Alice"],
            )
            .expect("insert alice");
            conn.execute(
                "INSERT INTO contact (username, nick_name) VALUES (?1, ?2)",
                params![TALKER_BOB, "Bob"],
            )
            .expect("insert bob");

            let tagged_extra =
                encode_extra_buffer_for_test(None, None, None, None, None, None, None, Some("1"));
            conn.execute(
                "INSERT INTO contact (username, nick_name, extra_buffer) VALUES (?1, ?2, ?3)",
                params![TALKER_TAGGED, "Sensitive Person", tagged_extra],
            )
            .expect("insert tagged contact");
        },
    );
}

fn create_encrypted_session_db(path: &Path, raw_key: &[u8; 32]) {
    create_encrypted_db(
        path,
        raw_key,
        "CREATE TABLE SessionTable (
            username TEXT,
            sort_timestamp INTEGER,
            summary TEXT,
            last_msg_type INTEGER DEFAULT NULL,
            last_msg_sender TEXT DEFAULT NULL,
            last_sender_display_name TEXT DEFAULT NULL
        );",
        |conn| {
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    TALKER_ALICE,
                    1_700_000_300_i64,
                    "alice summary",
                    Some(1_i64),
                    Some(TALKER_ALICE),
                    Some("Alice"),
                ],
            )
            .expect("insert alice session");
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    TALKER_BOB,
                    1_700_000_200_i64,
                    "bob summary",
                    Some(1_i64),
                    Some(TALKER_BOB),
                    Some("Bob"),
                ],
            )
            .expect("insert bob session");
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    TALKER_GROUP,
                    1_700_000_100_i64,
                    "group summary",
                    Some(1_i64),
                    Some(TALKER_TAGGED),
                    Some("Sensitive Person"),
                ],
            )
            .expect("insert group session");
        },
    );
}

fn create_encrypted_message_db(path: &Path, raw_key: &[u8; 32]) {
    create_encrypted_db(
        path,
        raw_key,
        &format!(
            "CREATE TABLE Timestamp (timestamp INTEGER);
            CREATE TABLE Name2Id (
                rowid INTEGER PRIMARY KEY,
                user_name TEXT
            );
            CREATE TABLE [{alice}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER,
                WCDB_CT_message_content INTEGER
            );
            CREATE TABLE [{bob}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER,
                WCDB_CT_message_content INTEGER
            );
            CREATE TABLE [{group}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER,
                WCDB_CT_message_content INTEGER
            );",
            alice = TABLE_ALICE,
            bob = TABLE_BOB,
            group = TABLE_GROUP,
        ),
        |conn| {
            conn.execute(
                "INSERT INTO Timestamp VALUES (?1)",
                params![1_700_000_000_i64],
            )
            .expect("insert timestamp");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![1_i64, TALKER_ALICE],
            )
            .expect("insert alice mapping");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![2_i64, TALKER_BOB],
            )
            .expect("insert bob mapping");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![3_i64, TALKER_GROUP],
            )
            .expect("insert group mapping");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![4_i64, TALKER_TAGGED],
            )
            .expect("insert tagged mapping");

            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_ALICE
                ),
                params![
                    100_i64,
                    2001_i64,
                    1_i64,
                    1_i64,
                    1_700_000_301_i64,
                    b"alice says hi" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert alice message");

            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_BOB
                ),
                params![
                    110_i64,
                    2002_i64,
                    1_i64,
                    2_i64,
                    1_700_000_201_i64,
                    b"bob says hi" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert bob message");

            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_GROUP
                ),
                params![
                    120_i64,
                    2003_i64,
                    1_i64,
                    4_i64,
                    1_700_000_101_i64,
                    b"group message from tagged sender" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert group message");
        },
    );
}

fn create_encrypted_db(
    path: &Path,
    raw_key: &[u8; 32],
    schema_sql: &str,
    seed: impl FnOnce(&Connection),
) {
    let conn = Connection::open(path).expect("open sqlite");
    unsafe {
        let rc = rusqlite::ffi::sqlite3_key(
            conn.handle(),
            raw_key.as_ptr() as *const _,
            raw_key.len() as i32,
        );
        assert_eq!(rc, 0, "sqlite3_key rc={rc}");
    }
    conn.execute_batch(schema_sql).expect("apply schema");
    seed(&conn);
}

fn test_raw_key() -> [u8; 32] {
    let bytes = hex::decode(TEST_KEY_HEX).expect("decode test key");
    let mut raw_key = [0_u8; 32];
    raw_key.copy_from_slice(&bytes);
    raw_key
}

fn run_json(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run wx-cli");
    assert!(
        output.status.success(),
        "command failed: {:?}\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse json output")
}

fn run_failure(home: &Path, args: &[&str]) -> String {
    let output = Command::new(bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run failing wx-cli command");
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded: {:?}\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_contact_ids(envelope: &Value, expected: &[&str]) {
    let items = envelope["items"].as_array().expect("contacts items array");
    let actual = items
        .iter()
        .map(|item| item["user_name"].as_str().expect("contact user_name"))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{envelope}");
}

// ── Phase 2: sender-level hiding tests ──────────────────────────────

const TABLE_GROUP_SENDER: &str = "Msg_adcd19623ae4b1f076f9731d6c37b266";

fn create_sender_account(root: &Path) {
    let account_dir = root.join(ACCOUNT_SENDERS);
    let db_root = account_dir.join("db_storage");
    let contact_dir = db_root.join("contact");
    let session_dir = db_root.join("session");
    let message_dir = db_root.join("message");

    fs::create_dir_all(&contact_dir).expect("create contact dir");
    fs::create_dir_all(&session_dir).expect("create session dir");
    fs::create_dir_all(&message_dir).expect("create message dir");

    let raw_key = test_raw_key();

    // Contact DB: alice, spam, group
    create_encrypted_db(
        &contact_dir.join("contact.db"),
        &raw_key,
        "CREATE TABLE contact (
            username TEXT PRIMARY KEY,
            alias TEXT DEFAULT '',
            remark TEXT DEFAULT '',
            nick_name TEXT DEFAULT '',
            description TEXT DEFAULT NULL,
            extra_buffer BLOB DEFAULT NULL
        );
        CREATE TABLE contact_label (
            label_id_ TEXT,
            label_name_ TEXT,
            sort_order_ INTEGER
        );",
        |conn| {
            conn.execute(
                "INSERT INTO contact (username, nick_name) VALUES (?1, ?2)",
                params![TALKER_ALICE, "Alice"],
            )
            .expect("insert alice");
            conn.execute(
                "INSERT INTO contact (username, nick_name) VALUES (?1, ?2)",
                params![TALKER_SPAM, "Spammer"],
            )
            .expect("insert spam");
        },
    );

    // Session DB: alice (private), group (last_msg_sender = spam)
    create_encrypted_db(
        &session_dir.join("session.db"),
        &raw_key,
        "CREATE TABLE SessionTable (
            username TEXT,
            sort_timestamp INTEGER,
            summary TEXT,
            last_msg_type INTEGER DEFAULT NULL,
            last_msg_sender TEXT DEFAULT NULL,
            last_sender_display_name TEXT DEFAULT NULL
        );",
        |conn| {
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    TALKER_ALICE,
                    1_700_000_300_i64,
                    "private msg",
                    Some(1_i64),
                    Some(TALKER_ALICE),
                    Some("Alice"),
                ],
            )
            .expect("insert alice session");
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    TALKER_GROUP,
                    1_700_000_200_i64,
                    "spam message in group",
                    Some(1_i64),
                    Some(TALKER_SPAM),
                    Some("Spammer"),
                ],
            )
            .expect("insert group session");
        },
    );

    // Message DB: group messages from alice and spam, plus a quote referencing spam
    let quote_xml = format!(
        r#"<msg><appmsg appid="" sdkver=""><title>my reply to spam</title><type>57</type><refermsg><type>1</type><svrid>5001</svrid><fromusr>{TALKER_SPAM}</fromusr><displayname>Spammer</displayname><content>spam content</content></refermsg></appmsg></msg>"#
    );

    create_encrypted_db(
        &message_dir.join("message_0.db"),
        &raw_key,
        &format!(
            "CREATE TABLE Timestamp (timestamp INTEGER);
            CREATE TABLE Name2Id (
                rowid INTEGER PRIMARY KEY,
                user_name TEXT
            );
            CREATE TABLE [{alice}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER,
                WCDB_CT_message_content INTEGER
            );
            CREATE TABLE [{group}] (
                sort_seq INTEGER,
                server_id INTEGER,
                local_type INTEGER,
                real_sender_id INTEGER,
                create_time INTEGER,
                message_content BLOB,
                packed_info_data BLOB,
                status INTEGER,
                WCDB_CT_message_content INTEGER
            );",
            alice = TABLE_ALICE,
            group = TABLE_GROUP_SENDER,
        ),
        |conn| {
            conn.execute(
                "INSERT INTO Timestamp VALUES (?1)",
                params![1_700_000_000_i64],
            )
            .expect("insert timestamp");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![1_i64, TALKER_ALICE],
            )
            .expect("insert alice mapping");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![3_i64, TALKER_GROUP],
            )
            .expect("insert group mapping");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![5_i64, TALKER_SPAM],
            )
            .expect("insert spam mapping");

            // Private chat message (sender hiding should NOT apply)
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_ALICE
                ),
                params![
                    100_i64,
                    3001_i64,
                    1_i64,
                    1_i64,
                    1_700_000_301_i64,
                    b"private hello" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert alice private message");

            // Group: message from alice (visible sender)
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_GROUP_SENDER
                ),
                params![
                    200_i64,
                    4001_i64,
                    1_i64,
                    1_i64,
                    1_700_000_101_i64,
                    b"alice says hello in group" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert alice group message");

            // Group: message from spam (hidden sender)
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_GROUP_SENDER
                ),
                params![
                    210_i64,
                    4002_i64,
                    1_i64,
                    5_i64,
                    1_700_000_102_i64,
                    b"spam content" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert spam group message");

            // Group: quote from alice referencing spam
            // local_type = (sub_type << 32) | msg_type = (57 << 32) | 49
            let quote_local_type: i64 = (57_i64 << 32) | 49;
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = TABLE_GROUP_SENDER
                ),
                params![
                    220_i64,
                    4003_i64,
                    quote_local_type,
                    1_i64,
                    1_700_000_103_i64,
                    quote_xml.as_bytes(),
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert quote message");
        },
    );
}

#[test]
fn sender_hiding_filters_hidden_sender_messages_in_group() {
    let fixture = create_fixture();
    let senders_dir = fixture.account_dir(ACCOUNT_SENDERS);

    let result = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_GROUP,
            "--data-dir",
            senders_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = result["items"].as_array().expect("query items array");

    // spam message (server_id=4002) should be filtered out
    let senders: Vec<&str> = items
        .iter()
        .map(|i| i["sender"].as_str().unwrap())
        .collect();
    assert!(
        !senders.contains(&TALKER_SPAM),
        "hidden sender message should be filtered: {result}"
    );
    assert!(
        senders.contains(&TALKER_ALICE),
        "visible sender should remain: {result}"
    );

    // paging.total should NOT change (DB-level count)
    // paging.returned should reflect filtered items
    assert_eq!(
        result["paging"]["returned"].as_u64().unwrap(),
        items.len() as u64
    );
}

#[test]
fn sender_hiding_does_not_affect_private_chat() {
    let fixture = create_fixture();
    let senders_dir = fixture.account_dir(ACCOUNT_SENDERS);

    // TALKER_ALICE is not in ignore_contacts, so her private chat is visible
    let result = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_ALICE,
            "--data-dir",
            senders_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = result["items"].as_array().expect("query items array");
    assert_eq!(
        items.len(),
        1,
        "private chat should not be filtered: {result}"
    );
}

#[test]
fn sender_hiding_redacts_quote_referring_hidden_sender() {
    let fixture = create_fixture();
    let senders_dir = fixture.account_dir(ACCOUNT_SENDERS);

    let result = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_GROUP,
            "--data-dir",
            senders_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = result["items"].as_array().expect("query items array");

    // Find the quote message (server_id=4003)
    let quote = items.iter().find(|i| i["server_id"].as_i64() == Some(4003));
    assert!(
        quote.is_some(),
        "quote message should be present (alice's reply): items={:?}",
        items
            .iter()
            .map(|i| i["server_id"].as_i64())
            .collect::<Vec<_>>()
    );
    let quote = quote.unwrap();

    // refer_sender and refer_content should be null (redacted)
    let q = &quote["content"]["Quote"];
    assert!(
        q["refer_sender"].is_null(),
        "refer_sender should be redacted: {quote}"
    );
    assert!(
        q["refer_content"].is_null(),
        "refer_content should be redacted: {quote}"
    );
    // reply_text should be preserved
    assert!(
        q["reply_text"].as_str().unwrap().contains("my reply"),
        "reply_text should be preserved: {quote}"
    );
    // raw_xml should be cleared
    assert_eq!(
        q["raw_xml"].as_str(),
        Some(""),
        "raw_xml should be empty: {quote}"
    );
}

#[test]
fn sender_hiding_show_hidden_restores_all_messages_and_quotes() {
    let fixture = create_fixture();
    let senders_dir = fixture.account_dir(ACCOUNT_SENDERS);

    let result = run_json(
        fixture.path(),
        &[
            "query",
            TALKER_GROUP,
            "--data-dir",
            senders_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--show-hidden",
            "--format",
            "json",
        ],
    );
    let items = result["items"].as_array().expect("query items array");
    assert_eq!(
        items.len(),
        3,
        "show_hidden should restore all 3 messages: {result}"
    );

    // Quote should have refer_sender intact
    let quote = items
        .iter()
        .find(|i| i["server_id"].as_i64() == Some(4003))
        .unwrap();
    assert!(
        !quote["content"]["Quote"]["refer_sender"].is_null(),
        "refer_sender should be intact with show_hidden: {quote}"
    );
}

#[test]
fn sender_hiding_session_placeholder_when_last_sender_hidden() {
    let fixture = create_fixture();
    let senders_dir = fixture.account_dir(ACCOUNT_SENDERS);

    let sessions = run_json(
        fixture.path(),
        &[
            "sessions",
            "--data-dir",
            senders_dir.as_str(),
            "--key",
            TEST_KEY_HEX,
            "--format",
            "json",
        ],
    );
    let items = sessions["items"].as_array().expect("sessions items array");

    let group_session = items
        .iter()
        .find(|i| i["username"].as_str() == Some(TALKER_GROUP));
    assert!(
        group_session.is_some(),
        "group session should be visible: {sessions}"
    );
    let group_session = group_session.unwrap();

    // Summary should be placeholder, sender fields should be null
    assert_eq!(
        group_session["summary"].as_str(),
        Some("[消息已隐藏]"),
        "summary should be placeholder: {group_session}"
    );
    assert!(
        group_session["last_msg_sender"].is_null(),
        "last_msg_sender should be null: {group_session}"
    );
    assert!(
        group_session["direction"].is_null(),
        "direction should be null: {group_session}"
    );
}
