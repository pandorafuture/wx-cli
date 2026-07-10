use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use tempfile::TempDir;
use wx_db::encode_packed_info_for_test;

const TEST_KEY_HEX: &str = "abababababababababababababababababababababababababababababababab";
const TEST_ACCOUNT_ID: &str = "wxid_test_account";
const TALKER: &str = "wxid_alice";
const MSG_TABLE: &str = "Msg_29a6db07e8bbdb53f5d54cc3c309f3f1";
const GROUP_TALKER: &str = "test@chatroom";
const GROUP_MSG_TABLE: &str = "Msg_1d282e28b02b5c9f9522f855de32f9a8";
const HIDDEN_SENDER: &str = "wxid_spam";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wx-cli")
}

struct TestServer {
    _fixture: TempDir,
    child: Child,
    base_url: String,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_media_missing_server_id_returns_400() {
    let server = spawn_test_server();
    let response = http_get(&server.base_url, "/api/v1/media?talker=wxid_alice");
    assert_eq!(response.status_code, 400, "{response:#?}");
}

#[test]
fn serve_media_missing_talker_returns_400() {
    let server = spawn_test_server();
    let response = http_get(&server.base_url, "/api/v1/media?server_id=2001");
    assert_eq!(response.status_code, 400, "{response:#?}");
}

#[test]
fn serve_media_invalid_format_returns_400() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=2001&talker=wxid_alice&format=wav",
    );
    assert_eq!(response.status_code, 400, "{response:#?}");
}

#[test]
fn serve_media_missing_asset_returns_404() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=2001&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 404, "{response:#?}");
}

#[test]
fn serve_media_hidden_sender_in_group_returns_404() {
    let server = spawn_test_server_with_hidden_contacts(&[HIDDEN_SENDER]);
    let response = http_get(
        &server.base_url,
        &format!("/api/v1/media?server_id=7001&talker={GROUP_TALKER}"),
    );
    assert_eq!(response.status_code, 404, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("message not found"), "{body}");
    assert!(!body.contains("hidden"), "{body}");
}

#[test]
fn serve_media_visible_sender_in_group_with_hidden_persons_not_visibility_blocked() {
    let server = spawn_test_server_with_hidden_contacts(&[HIDDEN_SENDER]);
    // server_id=7002 is from visible sender (wxid_alice), same group
    let response = http_get(
        &server.base_url,
        &format!("/api/v1/media?server_id=7002&talker={GROUP_TALKER}"),
    );
    // May be 404 (image asset not physically present for this talker path) but the
    // error should NOT be visibility-related — it should be about the missing asset,
    // not "message not found" (which is the visibility error message).
    if response.status_code == 404 {
        let body = String::from_utf8_lossy(&response.body);
        // The media endpoint was reached (not blocked by visibility) but the asset
        // isn't on disk. Acceptable.
        assert!(
            !body.contains("message not found"),
            "visible sender should NOT get visibility 404: {body}"
        );
    }
    // If 200, even better — means asset resolution succeeded
}

#[test]
fn serve_media_hidden_talker_returns_404_without_leaking_hidden_state() {
    let server = spawn_test_server_with_hidden_contacts(&[TALKER]);
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3001&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 404, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("message not found"), "{body}");
    assert!(!body.contains("hidden"), "{body}");
}

#[test]
fn serve_media_missing_video_mentions_not_downloaded() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=2003&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 404, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("downloaded locally"), "{body}");
}

#[test]
fn serve_media_unsupported_message_returns_415() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=2002&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 415, "{response:#?}");
}

#[test]
fn serve_media_dispatch_image_returns_png_bytes() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3001&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("image/png"));
    assert_eq!(&response.body[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn serve_media_wxgf_embedded_png_returns_png_without_ffmpeg() {
    let server = spawn_test_server_with_env(&[("FFMPEG_PATH", "/definitely-missing-ffmpeg")]);
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3006&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("image/png"));
    assert_eq!(&response.body[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn serve_media_wxgf_hevc_without_ffmpeg_returns_actionable_415() {
    let server = spawn_test_server_with_env(&[("FFMPEG_PATH", "/definitely-missing-ffmpeg")]);
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3007&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 415, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("install ffmpeg"), "{body}");
    assert!(body.contains("HEVC"), "{body}");
}

#[test]
fn serve_media_wxgf_hevc_returns_png_when_ffmpeg_is_available() {
    if !wx_media::ffmpeg_available() {
        return;
    }

    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3008&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("image/png"));
    assert_eq!(&response.body[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn serve_media_dispatch_voice_returns_ogg_by_default() {
    if !wx_media::ffmpeg_available() {
        return;
    }

    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3002&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("audio/ogg"));
    assert!(!response.body.is_empty());
}

#[test]
fn serve_media_dispatch_voice_mp3_returns_audio_mpeg() {
    if !wx_media::ffmpeg_available() {
        return;
    }

    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3002&talker=wxid_alice&format=mp3",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("audio/mpeg"));
    assert!(!response.body.is_empty());
}

#[test]
fn serve_media_voice_ogg_without_ffmpeg_returns_actionable_415() {
    let server = spawn_test_server_with_env(&[("FFMPEG_PATH", "/definitely-missing-ffmpeg")]);
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3002&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 415, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("install ffmpeg"), "{body}");
    assert!(body.contains("server_id=3002"), "{body}");
}

#[test]
fn serve_media_voice_mp3_without_ffmpeg_returns_actionable_415() {
    let server = spawn_test_server_with_env(&[("FFMPEG_PATH", "/definitely-missing-ffmpeg")]);
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3002&talker=wxid_alice&format=mp3",
    );
    assert_eq!(response.status_code, 415, "{response:#?}");
    let body = String::from_utf8_lossy(&response.body);
    assert!(body.contains("install ffmpeg"), "{body}");
    assert!(body.contains("mp3"), "{body}");
}

#[test]
fn serve_media_dispatch_video_returns_inline_file() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3003&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("video/mp4"));
    assert_eq!(
        response.header("content-disposition"),
        Some("inline; filename=\"vid001.mp4\"")
    );
    assert!(response.body.starts_with(b"video payload"));
}

#[test]
fn serve_media_dispatch_video_from_msg_video_path() {
    let server = spawn_test_server();
    let response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3005&talker=wxid_alice",
    );
    assert_eq!(response.status_code, 200, "{response:#?}");
    assert_eq!(response.header("content-type"), Some("video/mp4"));
    assert_eq!(
        response.header("content-disposition"),
        Some("inline; filename=\"custom-name.mp4\"")
    );
    assert_eq!(response.body, b"video via msg/video path".to_vec());
}

#[test]
fn serve_media_headers_file_attachment_and_range() {
    let server = spawn_test_server();
    let file_response = http_get(
        &server.base_url,
        "/api/v1/media?server_id=3004&talker=wxid_alice",
    );
    assert_eq!(file_response.status_code, 200, "{file_response:#?}");
    assert_eq!(
        file_response.header("content-disposition"),
        Some("attachment; filename=\"report.txt\"")
    );

    let range_response = http_get_with_headers(
        &server.base_url,
        "/api/v1/media?server_id=3003&talker=wxid_alice",
        &[("Range", "bytes=0-4")],
    );
    assert_eq!(range_response.status_code, 206, "{range_response:#?}");
    assert_eq!(range_response.body, b"video".to_vec());
    assert_eq!(range_response.header("content-range"), Some("bytes 0-4/23"));
}

fn spawn_test_server() -> TestServer {
    spawn_test_server_with_env(&[])
}

fn spawn_test_server_with_hidden_contacts(hidden_contacts: &[&str]) -> TestServer {
    spawn_test_server_with_setup(&[], hidden_contacts)
}

fn spawn_test_server_with_env(envs: &[(&str, &str)]) -> TestServer {
    spawn_test_server_with_setup(envs, &[])
}

fn spawn_test_server_with_setup(envs: &[(&str, &str)], hidden_contacts: &[&str]) -> TestServer {
    let fixture = create_fixture();
    if !hidden_contacts.is_empty() {
        write_settings(fixture.path(), hidden_contacts);
    }
    let account_dir = fixture.path().join(TEST_ACCOUNT_ID);
    let runtime_root = fixture.path().join("runtime");
    let port = find_open_port();
    let mut command = Command::new(bin());
    command
        .args([
            "server",
            "_worker",
            "--data-dir",
            account_dir.to_str().expect("fixture path utf8"),
            "--key",
            TEST_KEY_HEX,
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--poll",
            "--poll-ms",
            "1000",
            "--runtime-root",
            runtime_root.to_str().expect("runtime root utf8"),
        ])
        .env("HOME", fixture.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn wx-cli server worker");

    wait_for_server(port, &mut child);

    TestServer {
        _fixture: fixture,
        child,
        base_url: format!("http://127.0.0.1:{port}"),
    }
}

fn wait_for_server(port: u16, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = String::new();

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll child") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("server worker exited early with {status}: {stderr}");
        }

        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                let _ = stream.write_all(
                    b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                );
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                if buf.starts_with("HTTP/1.1") || buf.starts_with("HTTP/1.0") {
                    return;
                }
                last_error = format!("unexpected health response: {buf}");
            }
            Err(err) => {
                last_error = err.to_string();
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    panic!("server worker did not start on port {port}: {last_error}");
}

#[derive(Debug)]
struct HttpResponse {
    status_code: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn http_get(base_url: &str, path_and_query: &str) -> HttpResponse {
    http_get_with_headers(base_url, path_and_query, &[])
}

fn http_get_with_headers(
    base_url: &str,
    path_and_query: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let url = format!("{base_url}{path_and_query}");
    let mut request = ureq::get(&url);
    for (name, value) in headers {
        request = request.set(name, value);
    }

    match request.call() {
        Ok(response) => build_http_response(response.status(), response),
        Err(ureq::Error::Status(status_code, response)) => {
            build_http_response(status_code, response)
        }
        Err(err) => panic!("request failed for {url}: {err}"),
    }
}

fn build_http_response(status_code: u16, response: ureq::Response) -> HttpResponse {
    let headers = response
        .headers_names()
        .into_iter()
        .filter_map(|name| {
            response
                .header(&name)
                .map(|value| (name.to_ascii_lowercase(), value.to_string()))
        })
        .collect::<HashMap<_, _>>();
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .expect("read response body");
    HttpResponse {
        status_code,
        headers,
        body,
    }
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

fn write_settings(home: &Path, hidden_contacts: &[&str]) {
    let config_dir = home.join(".config").join("wechat-utils");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let contacts = hidden_contacts
        .iter()
        .map(|contact| format!("\"{contact}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut toml = format!("[accounts.\"{TEST_ACCOUNT_ID}\"]\n");
    if !hidden_contacts.is_empty() {
        toml.push_str(&format!("ignore_contacts = [{contacts}]\n"));
    }
    fs::write(config_dir.join("settings.toml"), toml).expect("write settings");
}

fn create_fixture() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let account_dir = dir.path().join(TEST_ACCOUNT_ID);
    let db_root = account_dir.join("db_storage");
    let contact_dir = db_root.join("contact");
    let session_dir = db_root.join("session");
    let message_dir = db_root.join("message");
    let attach_dir = account_dir.join("msg").join("attach");
    let file_dir = account_dir.join("msg").join("file");
    let video_dir = account_dir.join("msg").join("video");
    let hardlink_dir = db_root.join("hardlink");

    fs::create_dir_all(&contact_dir).expect("create contact dir");
    fs::create_dir_all(&session_dir).expect("create session dir");
    fs::create_dir_all(&message_dir).expect("create message dir");
    fs::create_dir_all(&hardlink_dir).expect("create hardlink dir");
    fs::create_dir_all(&file_dir).expect("create file dir");
    fs::create_dir_all(&video_dir).expect("create video dir");
    fs::create_dir_all(&attach_dir).expect("create attach dir");

    let raw_key = test_raw_key();
    create_encrypted_contact_db(&contact_dir.join("contact.db"), &raw_key);
    create_encrypted_session_db(&session_dir.join("session.db"), &raw_key);
    create_encrypted_message_db(&message_dir.join("message_0.db"), &raw_key);
    create_encrypted_voice_db(
        &message_dir.join("media_0.db"),
        &message_dir.join("media_1.db"),
        &raw_key,
    );
    create_encrypted_hardlink_db(&hardlink_dir.join("hardlink.db"), &raw_key);
    create_image_fixture(&attach_dir);
    create_video_fixture(&attach_dir);
    create_video_fixture_under_video_dir(&video_dir);
    create_file_fixture(&file_dir);

    dir
}

fn test_raw_key() -> [u8; 32] {
    let bytes = hex::decode(TEST_KEY_HEX).expect("decode test key");
    let mut raw_key = [0_u8; 32];
    raw_key.copy_from_slice(&bytes);
    raw_key
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
                "INSERT INTO contact (username, alias, remark, nick_name) VALUES (?1, ?2, ?3, ?4)",
                params![TALKER, "", "", "Alice"],
            )
            .expect("insert contact");
            conn.execute(
                "INSERT INTO contact (username, alias, remark, nick_name) VALUES (?1, ?2, ?3, ?4)",
                params![HIDDEN_SENDER, "", "", "Spammer"],
            )
            .expect("insert spam contact");
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
            summary TEXT
        );",
        |conn| {
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3)",
                params![TALKER, 1_700_000_000_i64, "fixture summary"],
            )
            .expect("insert session");
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3)",
                params![GROUP_TALKER, 1_700_000_001_i64, "group summary"],
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
            CREATE TABLE [{table}] (
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
            CREATE TABLE [{group_table}] (
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
            table = MSG_TABLE,
            group_table = GROUP_MSG_TABLE,
        ),
        |conn| {
            conn.execute(
                "INSERT INTO Timestamp VALUES (?1)",
                params![1_700_000_000_i64],
            )
            .expect("insert timestamp");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![1_i64, TALKER],
            )
            .expect("insert name2id");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![2_i64, GROUP_TALKER],
            )
            .expect("insert group name2id");
            conn.execute(
                "INSERT INTO Name2Id VALUES (?1, ?2)",
                params![3_i64, HIDDEN_SENDER],
            )
            .expect("insert spam name2id");

            let image_info = encode_packed_info_for_test(Some("md5_image_missing_asset"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    100_i64,
                    2001_i64,
                    3_i64,
                    1_i64,
                    1_700_000_100_i64,
                    Vec::<u8>::new(),
                    image_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert image message");

            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    200_i64,
                    2002_i64,
                    1_i64,
                    1_i64,
                    1_700_000_200_i64,
                    b"plain text" as &[u8],
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert text message");

            let missing_video_info = encode_packed_info_for_test(None, Some("vid_missing"));
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    250_i64,
                    2003_i64,
                    43_i64,
                    1_i64,
                    1_700_000_250_i64,
                    Vec::<u8>::new(),
                    missing_video_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert missing video message");

            let image_ok_info = encode_packed_info_for_test(Some("md5_image_ok"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    300_i64,
                    3001_i64,
                    3_i64,
                    1_i64,
                    1_709_251_200_i64,
                    Vec::<u8>::new(),
                    image_ok_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert image ok message");

            let image_wxgf_png_info = encode_packed_info_for_test(Some("md5_image_wxgf_png"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    350_i64,
                    3006_i64,
                    3_i64,
                    1_i64,
                    1_709_251_205_i64,
                    Vec::<u8>::new(),
                    image_wxgf_png_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert wxgf png image message");

            let image_wxgf_hevc_info =
                encode_packed_info_for_test(Some("md5_image_wxgf_hevc"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    360_i64,
                    3007_i64,
                    3_i64,
                    1_i64,
                    1_709_251_206_i64,
                    Vec::<u8>::new(),
                    image_wxgf_hevc_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert wxgf hevc image message");

            let image_wxgf_hevc_valid_info =
                encode_packed_info_for_test(Some("md5_image_wxgf_hevc_valid"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    370_i64,
                    3008_i64,
                    3_i64,
                    1_i64,
                    1_709_251_207_i64,
                    Vec::<u8>::new(),
                    image_wxgf_hevc_valid_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert valid wxgf hevc image message");

            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    400_i64,
                    3002_i64,
                    34_i64,
                    1_i64,
                    1_709_251_201_i64,
                    Vec::<u8>::new(),
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert voice message");

            let video_info = encode_packed_info_for_test(None, Some("vid001"));
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    500_i64,
                    3003_i64,
                    43_i64,
                    1_i64,
                    1_709_251_202_i64,
                    Vec::<u8>::new(),
                    video_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert video message");

            let video_info_msg_video = encode_packed_info_for_test(None, Some("vid002"));
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    550_i64,
                    3005_i64,
                    43_i64,
                    1_i64,
                    1_709_251_204_i64,
                    Vec::<u8>::new(),
                    video_info_msg_video,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert msg/video video message");

            let file_xml = r#"<msg><appmsg><title>report.txt</title><fileext>txt</fileext><totallen>11</totallen><md5>doc123</md5></appmsg></msg>"#;
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = MSG_TABLE
                ),
                params![
                    600_i64,
                    3004_i64,
                    (6_i64 << 32) | 49_i64,
                    1_i64,
                    1_709_251_203_i64,
                    file_xml.as_bytes(),
                    None::<Vec<u8>>,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert file message");

            // Group chatroom: image message from hidden sender (server_id=7001)
            let group_image_info = encode_packed_info_for_test(Some("md5_group_spam_image"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = GROUP_MSG_TABLE
                ),
                params![
                    900_i64,
                    7001_i64,
                    3_i64, // msg_type=3 (image)
                    3_i64, // real_sender_id=3 → HIDDEN_SENDER
                    1_700_000_900_i64,
                    Vec::<u8>::new(),
                    group_image_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert group spam image");

            // Group chatroom: image message from visible sender (server_id=7002)
            let group_visible_info = encode_packed_info_for_test(Some("md5_image_png_ok"), None);
            conn.execute(
                &format!(
                    "INSERT INTO [{table}] VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    table = GROUP_MSG_TABLE
                ),
                params![
                    910_i64,
                    7002_i64,
                    3_i64, // msg_type=3 (image)
                    1_i64, // real_sender_id=1 → TALKER (visible)
                    1_700_000_910_i64,
                    Vec::<u8>::new(),
                    group_visible_info,
                    0_i32,
                    None::<i32>,
                ],
            )
            .expect("insert group visible image");
        },
    );
}

fn create_encrypted_voice_db(schema_only_path: &Path, voice_path: &Path, raw_key: &[u8; 32]) {
    create_encrypted_db(
        schema_only_path,
        raw_key,
        "CREATE TABLE Metadata (value TEXT);",
        |conn| {
            conn.execute("INSERT INTO Metadata (value) VALUES ('schema-only')", [])
                .expect("insert metadata row");
        },
    );

    create_encrypted_db(
        voice_path,
        raw_key,
        "CREATE TABLE VoiceInfo (
            svr_id TEXT,
            voice_data BLOB
        );",
        |conn| {
            conn.execute(
                "INSERT INTO VoiceInfo (svr_id, voice_data) VALUES (?1, ?2)",
                params!["3002", sample_silk()],
            )
            .expect("insert voice blob");
        },
    );
}

fn create_encrypted_hardlink_db(path: &Path, raw_key: &[u8; 32]) {
    create_encrypted_db(
        path,
        raw_key,
        "CREATE TABLE dir2id (rowid INTEGER PRIMARY KEY, username TEXT);
         CREATE TABLE image_hardlink_info_v3 (
             md5 TEXT, file_name TEXT, file_size INTEGER, modify_time INTEGER,
             dir1 INTEGER, dir2 INTEGER
         );
         CREATE TABLE video_hardlink_info_v3 (
             md5 TEXT, file_name TEXT, file_size INTEGER, modify_time INTEGER,
             dir1 INTEGER, dir2 INTEGER
         );
         CREATE TABLE file_hardlink_info_v3 (
             md5 TEXT, file_name TEXT, file_size INTEGER, modify_time INTEGER,
             dir1 INTEGER, dir2 INTEGER
         );",
        |conn| {
            conn.execute(
                "INSERT INTO dir2id (rowid, username) VALUES (?1, ?2)",
                params![1_i64, TALKER],
            )
            .expect("insert hardlink dir1");
            conn.execute(
                "INSERT INTO dir2id (rowid, username) VALUES (?1, ?2)",
                params![2_i64, "2026-03"],
            )
            .expect("insert hardlink dir2");
            conn.execute(
                "INSERT INTO video_hardlink_info_v3 (md5, file_name, file_size, modify_time, dir1, dir2) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["vid001", "vid001.mp4", 23_i64, 1_709_251_202_i64, 1_i64, 2_i64],
            )
            .expect("insert video hardlink");
            conn.execute(
                "INSERT INTO video_hardlink_info_v3 (md5, file_name, file_size, modify_time, dir1, dir2) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["vid002", "custom-name.mp4", 22_i64, 1_709_251_204_i64, 1_i64, 2_i64],
            )
            .expect("insert msg/video hardlink");
            conn.execute(
                "INSERT INTO file_hardlink_info_v3 (md5, file_name, file_size, modify_time, dir1, dir2) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["doc123", "report.txt", 11_i64, 1_709_251_203_i64, 1_i64, 2_i64],
            )
            .expect("insert file hardlink");
        },
    );
}

fn create_image_fixture(attach_dir: &Path) {
    let month_dir = attach_dir
        .join(format!("{:x}", wx_media::md5_hash(TALKER.as_bytes())))
        .join("2026-03")
        .join("Img");
    fs::create_dir_all(&month_dir).expect("create image month dir");

    let xor_key = 0x5A_u8;
    let png = sample_png();
    let encrypted = xor_bytes(&png, xor_key);
    fs::write(month_dir.join("md5_image_ok_t.dat"), &encrypted).expect("write thumb dat");
    fs::write(month_dir.join("md5_image_ok.dat"), &encrypted).expect("write image dat");

    let wxgf_png = sample_wxgf_with_embedded_png();
    let wxgf_png_encrypted = xor_bytes(&wxgf_png, xor_key);
    fs::write(
        month_dir.join("md5_image_wxgf_png.dat"),
        &wxgf_png_encrypted,
    )
    .expect("write wxgf embedded png dat");

    let wxgf_hevc = sample_wxgf_with_hevc();
    let wxgf_hevc_encrypted = xor_bytes(&wxgf_hevc, xor_key);
    fs::write(
        month_dir.join("md5_image_wxgf_hevc.dat"),
        &wxgf_hevc_encrypted,
    )
    .expect("write wxgf hevc dat");

    let wxgf_hevc_valid = sample_wxgf_with_valid_hevc();
    let wxgf_hevc_valid_encrypted = xor_bytes(&wxgf_hevc_valid, xor_key);
    fs::write(
        month_dir.join("md5_image_wxgf_hevc_valid.dat"),
        &wxgf_hevc_valid_encrypted,
    )
    .expect("write valid wxgf hevc dat");
}

fn create_video_fixture(attach_dir: &Path) {
    let video_dir = attach_dir.join(TALKER).join("2026-03").join("Video");
    fs::create_dir_all(&video_dir).expect("create video dir");
    fs::write(video_dir.join("vid001.mp4"), b"video payload bytes 123").expect("write video");
}

fn create_file_fixture(file_dir: &Path) {
    let month_dir = file_dir.join(TALKER).join("2026-03");
    fs::create_dir_all(&month_dir).expect("create file month dir");
    fs::write(month_dir.join("report.txt"), b"hello file!").expect("write file");
}

fn create_video_fixture_under_video_dir(video_dir: &Path) {
    let month_dir = video_dir.join(TALKER).join("2026-03");
    fs::create_dir_all(&month_dir).expect("create msg/video dir");
    fs::write(
        month_dir.join("custom-name.mp4"),
        b"video via msg/video path",
    )
    .expect("write msg/video file");
}

fn sample_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

fn sample_wxgf_with_embedded_png() -> Vec<u8> {
    let mut data = b"wxgfmetadata".to_vec();
    data.extend_from_slice(&sample_png());
    data
}

fn sample_wxgf_with_hevc() -> Vec<u8> {
    let mut data = b"wxgfmetadata".to_vec();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x02, 0x03, 0x04]);
    data
}

fn sample_wxgf_with_valid_hevc() -> Vec<u8> {
    let mut data = b"wxgfmetadata".to_vec();
    data.extend_from_slice(&sample_valid_hevc());
    data
}

fn sample_valid_hevc() -> Vec<u8> {
    if !wx_media::ffmpeg_available() {
        return vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x02, 0x03, 0x04];
    }

    let temp = TempDir::new().expect("tempdir for hevc sample");
    let output = temp.path().join("frame.hevc");
    let ffmpeg = std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64:d=0.04:r=1",
            "-frames:v",
            "1",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error",
            "-f",
            "hevc",
            output.to_str().expect("hevc output path utf8"),
        ])
        .status()
        .expect("run ffmpeg for hevc sample");
    assert!(status.success(), "failed to create HEVC sample");
    fs::read(output).expect("read hevc sample")
}

fn xor_bytes(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|byte| byte ^ key).collect()
}

fn sample_silk() -> Vec<u8> {
    let pcm = vec![0_u8; 24_000 / 1_000 * 40 * 2];
    silk_rs::encode_silk(pcm, 24_000, 24_000, true).expect("encode silk")
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
            raw_key.as_ptr() as *const std::ffi::c_void,
            32,
        );
        assert_eq!(rc, 0, "sqlite3_key failed for {}", path.display());
    }
    conn.execute_batch(schema_sql).expect("apply schema");
    seed(&conn);
}

fn find_open_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("listener addr").port()
}
