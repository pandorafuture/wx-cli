use std::process::Command;
use std::time::Duration;

use regex::Regex;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as AsyncCommand;
use tokio::time::timeout;

use crate::error::KeychainError;
use crate::process::AccountDirInfo;
use crate::script::CAPTURE_KEY_SCRIPT;
use wx_decrypt::params::MACOS_4_1_7_31;
use wx_decrypt::validate_key;

/// Result of a successful key capture.
#[derive(Debug)]
pub struct CaptureResult {
    pub raw_key: [u8; 32],
    pub call_count: u32,
    /// Which account directory the captured key belongs to.
    pub matched_account: AccountDirInfo,
}

/// Run the full LLDB key capture flow against all known account directories.
///
/// 1. Read salts from ALL account `message_0.db` files.
/// 2. Kill WeChat.
/// 3. Launch LLDB with `-w -n WeChat` (waits for WeChat to start).
/// 4. Open WeChat; user logs in.
/// 5. Stream LLDB output, parsing PBKDF2 calls.
/// 6. For each call with rounds=256000, check its salt against ALL known salts.
/// 7. On match, validate the full key via HMAC. Return key + matched account.
///
/// This approach never pre-picks a target account, so it works regardless of
/// which account WeChat decides to auto-login as.
pub async fn capture_key(
    accounts: &[AccountDirInfo],
    capture_timeout: Duration,
) -> Result<CaptureResult, KeychainError> {
    if accounts.is_empty() {
        return Err(KeychainError::Other(
            "no account directories provided".into(),
        ));
    }

    // Pre-read salts from all accounts. Skip unreadable DBs.
    let account_salts: Vec<([u8; 16], &AccountDirInfo)> = accounts
        .iter()
        .filter_map(|a| {
            wx_decrypt::read_db_salt(&a.message_db_path)
                .ok()
                .map(|salt| (salt, a))
        })
        .collect();

    if account_salts.is_empty() {
        return Err(KeychainError::Other(
            "could not read salt from any account database".into(),
        ));
    }

    // Kill WeChat.
    let _ = Command::new("killall").arg("WeChat").output();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Write capture script to temp file.
    let script_path = wx_paths::AppPaths::lldb_script_file();
    if let Some(parent) = script_path.parent() {
        wx_paths::AppPaths::ensure_dir(parent)?;
    }
    std::fs::write(&script_path, CAPTURE_KEY_SCRIPT)?;

    // Prepare LLDB output file.
    let output_path = wx_paths::AppPaths::lldb_output_file();

    // Launch LLDB in wait mode.
    let mut lldb = AsyncCommand::new("lldb")
        .args([
            "-w",
            "-n",
            "WeChat",
            "-o",
            &format!("command script import {}", script_path.display()),
            "-o",
            "capture_keys",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| KeychainError::Other(format!("failed to start lldb: {e}")))?;

    // Brief pause then open WeChat.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = Command::new("open").arg("-a").arg("WeChat").output();

    eprintln!("Waiting for WeChat to start and trigger PBKDF2 calls...");
    eprintln!("Please log in to WeChat when prompted.");

    // Read LLDB stdout line by line, looking for PBKDF2 calls.
    let stdout = lldb
        .stdout
        .take()
        .ok_or_else(|| KeychainError::Other("no lldb stdout".into()))?;
    let mut reader = tokio::io::BufReader::new(stdout).lines();

    let re_header = Regex::new(r"^\[PBKDF2 #(\d+)\].*rounds=(\d+)").unwrap();
    let re_password = Regex::new(r"^\s*Password:\s*([0-9a-f]+)").unwrap();
    let re_salt = Regex::new(r"^\s*Salt:\s*([0-9a-f]+)").unwrap();

    let mut current_call: Option<(u32, u32)> = None; // (call_count, rounds)
    let mut current_password: Option<String> = None;
    let mut call_count = 0u32;
    let mut output_lines = Vec::new();

    let result = timeout(capture_timeout, async {
        loop {
            let line = match reader.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break Err(KeychainError::NoPbkdfCalls),
                Err(e) => break Err(KeychainError::Other(format!("read error: {e}"))),
            };

            output_lines.push(line.clone());

            if let Some(caps) = re_header.captures(&line) {
                let count: u32 = caps[1].parse().unwrap_or(0);
                let rounds: u32 = caps[2].parse().unwrap_or(0);
                current_call = Some((count, rounds));
                current_password = None;
                call_count = count;
                continue;
            }

            if let Some(caps) = re_password.captures(&line) {
                current_password = Some(caps[1].to_string());
                continue;
            }

            if let Some(caps) = re_salt.captures(&line) {
                let salt_hex = caps[1].to_string();

                if let Some((_, rounds)) = current_call {
                    if rounds == 256000 {
                        if let Some(ref pwd_hex) = current_password {
                            if let Ok(salt_bytes) = hex::decode(&salt_hex) {
                                if salt_bytes.len() == 16 {
                                    let mut pbkdf_salt = [0u8; 16];
                                    pbkdf_salt.copy_from_slice(&salt_bytes);

                                    // Match against ALL known account salts.
                                    let mut matched: Option<CaptureResult> = None;
                                    'salt_match: for (known_salt, account) in &account_salts {
                                        if pbkdf_salt != *known_salt {
                                            continue;
                                        }
                                        // Salt matched — validate the key.
                                        if let Ok(key_bytes) = hex::decode(pwd_hex) {
                                            if key_bytes.len() == 32 {
                                                let mut raw_key = [0u8; 32];
                                                raw_key.copy_from_slice(&key_bytes);

                                                use std::io::Read;
                                                let mut first_page = vec![0u8; 4096];
                                                if let Ok(mut f) =
                                                    std::fs::File::open(&account.message_db_path)
                                                {
                                                    if f.read_exact(&mut first_page).is_ok()
                                                        && validate_key(
                                                            &first_page,
                                                            &raw_key,
                                                            &MACOS_4_1_7_31,
                                                        )
                                                        .is_some()
                                                    {
                                                        matched = Some(CaptureResult {
                                                            raw_key,
                                                            call_count,
                                                            matched_account: (*account).clone(),
                                                        });
                                                        break 'salt_match;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if let Some(result) = matched {
                                        break Ok(result);
                                    }
                                }
                            }
                        }
                    }
                }
                current_call = None;
                current_password = None;
            }
        }
    })
    .await;

    // Save output for debugging.
    let _ = std::fs::write(&output_path, output_lines.join("\n"));

    // Kill LLDB.
    let _ = lldb.kill().await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(KeychainError::CaptureTimeout {
            seconds: capture_timeout.as_secs(),
        }),
    }
}
