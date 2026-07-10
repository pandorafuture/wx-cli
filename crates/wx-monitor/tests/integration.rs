use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection};
use wx_decrypt::{KeyMaterial, MACOS_4_1_7_31};
use wx_monitor::{MonitorConfig, WechatMonitor};

// ---- crypto helpers (standalone, matching wx-decrypt internals) ----

fn derive_enc_key(raw_key: &[u8; 32], salt: &[u8; 16]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(raw_key, salt, MACOS_4_1_7_31.kdf_iter, &mut key);
    key
}

fn derive_mac_key(enc_key: &[u8; 32], salt: &[u8; 16]) -> [u8; 32] {
    let mut mac_salt = [0u8; 16];
    for (i, b) in salt.iter().enumerate() {
        mac_salt[i] = b ^ 0x3a;
    }
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(enc_key, &mac_salt, 2, &mut key);
    key
}

/// Encrypt a single page from a plaintext SQLite file.
///
/// - `page_data`: raw 4096 bytes from the plaintext SQLite file
/// - `page_num`: 0-indexed page number
/// - `salt`: 16-byte salt (required for page 0, ignored for others)
fn encrypt_page(
    page_data: &[u8],
    enc_key: &[u8; 32],
    mac_key: &[u8; 32],
    page_num: u32,
    salt: &[u8; 16],
) -> Vec<u8> {
    use aes::cipher::{BlockModeEncrypt, KeyIvInit};
    use hmac::{Hmac, Mac};
    use sha2::Sha512;

    let params = &MACOS_4_1_7_31;
    let iv: [u8; 16] = [0x42; 16];
    let offset = if page_num == 0 { 16 } else { 0 };
    let data_size = params.page_size - params.reserve - offset; // 4000 for page 0, 4016 for others

    // Extract plaintext from the original page (skip SQLite header for page 0, skip reserved area)
    let plaintext = &page_data[offset..offset + data_size];

    // Encrypt with AES-256-CBC
    type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
    let mut ciphertext = plaintext.to_vec();
    let encryptor = Aes256CbcEnc::new(enc_key.into(), (&iv).into());
    encryptor
        .encrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut ciphertext, data_size)
        .unwrap();

    // Assemble encrypted page
    let mut page = Vec::with_capacity(params.page_size);
    if page_num == 0 {
        page.extend_from_slice(salt);
    }
    page.extend_from_slice(&ciphertext);
    // Reserve area: IV + HMAC
    page.extend_from_slice(&iv);
    page.resize(params.page_size, 0); // zero-fill HMAC placeholder

    // Compute HMAC
    let hmac_data_end = params.page_size - params.reserve + params.iv_size;
    let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(mac_key).unwrap();
    mac.update(&page[offset..hmac_data_end]);
    mac.update(&(page_num + 1).to_le_bytes()); // 1-indexed
    let hmac_result = mac.finalize().into_bytes();

    let hmac_start = params.page_size - params.reserve + params.iv_size;
    page[hmac_start..hmac_start + params.hmac_size]
        .copy_from_slice(&hmac_result[..params.hmac_size]);

    page
}

/// Create a valid SQLite session.db with reserved_page_size=80, then encrypt it.
///
/// Returns the path to the encrypted file.
fn create_encrypted_session_db(
    dir: &Path,
    raw_key: &[u8; 32],
    sessions: &[(&str, i64, &str)],
) -> PathBuf {
    let salt: [u8; 16] = [0x01; 16];

    // 1. Create a valid SQLite DB with reserved_page_size=80
    let plain_path = dir.join("session_plain.db");
    {
        let conn = Connection::open(&plain_path).unwrap();
        conn.execute_batch("PRAGMA page_size = 4096;").unwrap();

        // Set reserved bytes via sqlite3_file_control
        unsafe {
            let mut reserve: i32 = 80;
            let rc = rusqlite::ffi::sqlite3_file_control(
                conn.handle(),
                c"main".as_ptr(),
                38, // SQLITE_FCNTL_RESERVE_BYTES
                &mut reserve as *mut _ as *mut std::ffi::c_void,
            );
            assert_eq!(rc, 0, "sqlite3_file_control failed");
        }

        conn.execute_batch(
            "CREATE TABLE SessionTable (
                username TEXT,
                sort_timestamp INTEGER,
                summary TEXT,
                last_msg_type INTEGER,
                last_msg_sender TEXT,
                last_sender_display_name TEXT
            );",
        )
        .unwrap();

        for (username, ts, summary) in sessions {
            conn.execute(
                "INSERT INTO SessionTable VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
                params![username, ts, summary],
            )
            .unwrap();
        }
    }

    // 2. Read raw bytes and verify reserved=80
    let plain_data = std::fs::read(&plain_path).unwrap();
    assert_eq!(
        plain_data[20], 80,
        "reserved_page_size should be 80, got {}",
        plain_data[20]
    );
    let page_count = plain_data.len() / 4096;
    assert!(page_count >= 1, "expected at least 1 page");

    // 3. Derive keys
    let enc_key = derive_enc_key(raw_key, &salt);
    let mac_key = derive_mac_key(&enc_key, &salt);

    // 4. Encrypt each page
    let enc_path = dir.join("session.db");
    let mut enc_data = Vec::with_capacity(plain_data.len());
    for i in 0..page_count {
        let start = i * 4096;
        let page = &plain_data[start..start + 4096];
        let encrypted = encrypt_page(page, &enc_key, &mac_key, i as u32, &salt);
        enc_data.extend_from_slice(&encrypted);
    }
    std::fs::write(&enc_path, &enc_data).unwrap();

    // Clean up plain file
    let _ = std::fs::remove_file(&plain_path);

    enc_path
}

// ---- integration test ----

// Slow by design: exercises real polling + PBKDF2(256k) decrypt flow and
// routinely takes tens of seconds in debug builds. Keep it opt-in unless
// explicitly validating monitor/decrypt integration.
#[tokio::test]
#[ignore = "slow integration test; runs real PBKDF2/decrypt path"]
async fn monitor_detects_session_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let session_dir = dir.path().to_path_buf();

    let raw_key: [u8; 32] = [0xAB; 32];

    // Create initial encrypted session.db with one session
    create_encrypted_session_db(
        &session_dir,
        &raw_key,
        &[("wxid_alice", 1000, "hello from alice")],
    );

    // Start monitor with polling (200ms interval)
    let mut monitor = WechatMonitor::start(MonitorConfig {
        encrypted_session_dir: session_dir.clone(),
        key_material: KeyMaterial::RawKey(raw_key),
        params: &MACOS_4_1_7_31,
        watch_mode: wx_monitor::WatchMode::Poll,
        poll_interval: Duration::from_millis(200),
        channel_capacity: 100,
        raw_key: None,
        encrypted_root: None,
    })
    .expect("monitor should start");

    // Wait for initial setup to stabilize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Overwrite with updated data (new session added)
    // Need mtime to change, so sleep briefly
    std::thread::sleep(Duration::from_millis(1100));
    create_encrypted_session_db(
        &session_dir,
        &raw_key,
        &[
            ("wxid_alice", 1000, "hello from alice"),
            ("wxid_bob", 2000, "hello from bob"),
        ],
    );

    // Wait for event (up to 10 seconds, accounting for PBKDF2 overhead)
    // Plan specifies 5s; PBKDF2 256k iterations takes ~1s release / ~9s debug per call.
    // update() triggers a second PBKDF2, so debug needs ~20s total.
    let event = tokio::time::timeout(Duration::from_secs(25), monitor.recv())
        .await
        .expect("should receive event within timeout")
        .expect("event should not be None");

    // Should be an Updated event for wxid_bob (the new session)
    assert_eq!(event.username, "wxid_bob");
    assert!(matches!(event.kind, wx_monitor::SessionEventKind::Updated));

    // Stop monitor and assert clean exit
    monitor.stop();

    // Drain any remaining events, then recv must return None (task exited, channel closed).
    // The monitor loop checks shutdown every 500ms, so 5s is generous.
    loop {
        let result = tokio::time::timeout(Duration::from_secs(1), monitor.recv())
            .await
            .expect("monitor task should exit within timeout after stop()");
        match result {
            Some(_) => continue, // drain buffered events
            None => break,       // channel closed — task exited cleanly
        }
    }
}

// Slow by design: re-encrypts the same DB and waits long enough to prove the
// monitor does not flood reset events after a full decrypt. Skip by default
// because the PBKDF2/debug path makes this a tens-of-seconds test.
#[tokio::test]
#[ignore = "slow integration test; runs real PBKDF2/decrypt path"]
async fn full_decrypt_does_not_flood_resets() {
    let dir = tempfile::TempDir::new().unwrap();
    let session_dir = dir.path().to_path_buf();

    let raw_key: [u8; 32] = [0xAB; 32];

    // Create initial encrypted session.db with 5 sessions
    let sessions: Vec<(&str, i64, &str)> = vec![
        ("wxid_alice", 1000, "hello alice"),
        ("wxid_bob", 2000, "hello bob"),
        ("wxid_charlie", 3000, "hello charlie"),
        ("wxid_dave", 4000, "hello dave"),
        ("wxid_eve", 5000, "hello eve"),
    ];
    create_encrypted_session_db(&session_dir, &raw_key, &sessions);

    // Start monitor with polling (200ms interval)
    let mut monitor = WechatMonitor::start(MonitorConfig {
        encrypted_session_dir: session_dir.clone(),
        key_material: KeyMaterial::RawKey(raw_key),
        params: &MACOS_4_1_7_31,
        watch_mode: wx_monitor::WatchMode::Poll,
        poll_interval: Duration::from_millis(200),
        channel_capacity: 100,
        raw_key: None,
        encrypted_root: None,
    })
    .expect("monitor should start");

    // Wait for initial setup to fully stabilize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Drain any buffered events from initialization
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), monitor.recv()).await {
    }

    // Re-encrypt the same session.db with identical data (simulates WAL checkpoint)
    std::thread::sleep(Duration::from_millis(1100));
    create_encrypted_session_db(&session_dir, &raw_key, &sessions);

    // Wait long enough for FullDecrypt to complete (PBKDF2 ~20s in debug mode)
    // then verify no events arrived. With the old reset() code, 5 events would
    // arrive after PBKDF2 completes. With diff(), zero events are produced.
    let result = tokio::time::timeout(Duration::from_secs(30), monitor.recv()).await;
    assert!(
        result.is_err(),
        "expected no events after re-encrypting identical data, but got one — FullDecrypt is still flooding"
    );

    monitor.stop();
}
