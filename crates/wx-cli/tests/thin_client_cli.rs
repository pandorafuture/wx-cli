use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wx-cli")
}

#[test]
fn sessions_can_use_remote_json_path() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => {
            assert!(request.starts_with("GET /api/v1/health"));
            http_response("200 OK", "{\"ready\":true}")
        }
        1 => {
            assert!(request.starts_with("GET /api/v1/sessions?"));
            assert!(request.contains("limit=20"));
            assert!(request.contains("offset=0"));
            assert!(request.contains("order=desc"));
            assert!(!request.contains("show_hidden="));
            assert!(request.contains("Authorization: Bearer secret-token\r\n"));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "sessions",
            "--server-only",
            "--server-url",
            &base_url,
            "--server-token",
            "secret-token",
            "--format",
            "json",
        ])
        .output()
        .expect("run sessions");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"items\": []"));
    handle.join().unwrap();
}

#[test]
fn contacts_can_use_remote_json_path() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/contacts?"));
            assert!(!request.contains("show_hidden="));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "contacts",
            "--server-only",
            "--server-url",
            &base_url,
            "--format",
            "json",
        ])
        .output()
        .expect("run contacts");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"items\": []"));
    handle.join().unwrap();
}

#[test]
fn contacts_show_hidden_is_forwarded_only_when_requested() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/contacts?"));
            assert!(request.contains("show_hidden=1"));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "contacts",
            "--server-only",
            "--server-url",
            &base_url,
            "--show-hidden",
            "--format",
            "json",
        ])
        .output()
        .expect("run contacts");

    assert!(output.status.success(), "{output:?}");
    handle.join().unwrap();
}

#[test]
fn contacts_all_uses_global_max_limit() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/contacts?"));
            assert!(request.contains("limit=20000"));
            assert!(request.contains("show_hidden=1"));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "contacts",
            "--server-only",
            "--server-url",
            &base_url,
            "--all",
            "--show-hidden",
            "--format",
            "json",
        ])
        .output()
        .expect("run contacts");

    assert!(output.status.success(), "{output:?}");
    handle.join().unwrap();
}

#[test]
fn query_can_use_remote_json_path() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/messages?"));
            assert!(request.contains("contact=%E5%BC%A0%E4%B8%89"));
            assert!(!request.contains("show_hidden="));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "query",
            "张三",
            "--server-only",
            "--server-url",
            &base_url,
            "--format",
            "json",
        ])
        .output()
        .expect("run query");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"items\": []"));
    handle.join().unwrap();
}

#[test]
fn query_show_hidden_is_forwarded_only_when_requested() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/messages?"));
            assert!(request.contains("contact=%E5%BC%A0%E4%B8%89"));
            assert!(request.contains("show_hidden=1"));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "query",
            "张三",
            "--server-only",
            "--server-url",
            &base_url,
            "--show-hidden",
            "--format",
            "json",
        ])
        .output()
        .expect("run query");

    assert!(output.status.success(), "{output:?}");
    handle.join().unwrap();
}

#[test]
fn search_can_use_remote_json_path() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/search?"));
            assert!(request.contains("q=%E5%91%A8%E6%9C%AB"));
            assert!(!request.contains("show_hidden="));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "search",
            "周末",
            "--server-only",
            "--server-url",
            &base_url,
            "--format",
            "json",
        ])
        .output()
        .expect("run search");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"items\": []"));
    handle.join().unwrap();
}

#[test]
fn sessions_show_hidden_is_forwarded_only_when_requested() {
    let (base_url, handle) = spawn_sequence_server(2, |request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        1 => {
            assert!(request.starts_with("GET /api/v1/sessions?"));
            assert!(request.contains("show_hidden=1"));
            empty_envelope_response()
        }
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args([
            "sessions",
            "--server-only",
            "--server-url",
            &base_url,
            "--show-hidden",
            "--format",
            "json",
        ])
        .output()
        .expect("run sessions");

    assert!(output.status.success(), "{output:?}");
    handle.join().unwrap();
}

#[test]
fn server_only_fails_when_remote_unavailable() {
    let output = Command::new(bin())
        .args([
            "sessions",
            "--server-only",
            "--server-url",
            "http://127.0.0.1:9",
        ])
        .output()
        .expect("run sessions server-only");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"));
    assert!(!stderr.contains("falling back to local"));
}

#[test]
fn unavailable_remote_falls_back_to_local() {
    let (mut command, _home) = command_without_local_account();
    let output = command
        .args(["sessions", "--server-url", "http://127.0.0.1:9"])
        .output()
        .expect("run sessions fallback");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("falling back to local sessions"));
    assert!(!stderr.contains("/api/v1/health"));
    assert!(!stderr.contains("127.0.0.1:9"));
    assert!(stderr.contains("no account found"));
}

#[test]
fn no_server_bypasses_remote_probe() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let addr = listener.local_addr().expect("listener addr");
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(750);
        loop {
            match listener.accept() {
                Ok((_stream, _)) => return true,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept error: {err}"),
            }
        }
    });

    let (mut command, _home) = command_without_local_account();
    let output = command
        .args([
            "sessions",
            "--no-server",
            "--server-url",
            &format!("http://{}", addr),
        ])
        .output()
        .expect("run sessions no-server");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("remote server unavailable"));
    assert!(
        !handle.join().unwrap(),
        "no-server should not contact the mock server"
    );
}

#[test]
fn healthy_server_business_transport_failure_does_not_fall_back() {
    let (base_url, handle) = spawn_sequence_server(2, |_request, index| match index {
        0 => http_response("200 OK", "{\"ready\":true}"),
        // Close the second connection without a response. This is classified as an
        // unavailable transport error, but health already proved the server was selected.
        1 => String::new(),
        _ => unreachable!(),
    });

    let output = Command::new(bin())
        .args(["sessions", "--server-url", &base_url])
        .output()
        .expect("run sessions with failed business request");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"));
    assert!(!stderr.contains("falling back to local"));
    handle.join().unwrap();
}

fn command_without_local_account() -> (Command, TempDir) {
    let home = TempDir::new().expect("create isolated home");
    let mut command = Command::new(bin());
    command
        .env("HOME", home.path())
        .env_remove("WECHAT_CLI_DATA_DIR")
        .env_remove("WECHAT_CLI_ACCOUNT")
        .env_remove("WECHAT_CLI_KEY");
    (command, home)
}

fn spawn_sequence_server(
    expected_requests: usize,
    responder: impl Fn(String, usize) -> String + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let handle = thread::spawn(move || {
        for index in 0..expected_requests {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 8192];
            let n = stream.read(&mut buf).expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let response = responder(request, index);
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    });
    (format!("http://{}", addr), handle)
}

fn empty_envelope_response() -> String {
    http_response(
        "200 OK",
        r#"{"items":[],"paging":{"limit":20,"offset":0,"returned":0,"has_more":false,"total":0},"stats":{"scanned":0,"skipped":0,"elapsed_ms":1,"shard_warnings":[]}}"#,
    )
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
