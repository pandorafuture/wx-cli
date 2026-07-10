use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::MediaError;

fn ffmpeg_bin() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string())
}

fn ffprobe_bin() -> String {
    std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".to_string())
}

static FFMPEG_CACHED: AtomicBool = AtomicBool::new(false);
static FFMPEG_VALUE: AtomicBool = AtomicBool::new(false);

static FFPROBE_CACHED: AtomicBool = AtomicBool::new(false);
static FFPROBE_VALUE: AtomicBool = AtomicBool::new(false);

/// Check whether ffmpeg is available on the system (result cached after first call).
pub fn ffmpeg_available() -> bool {
    if FFMPEG_CACHED.load(Ordering::Acquire) {
        return FFMPEG_VALUE.load(Ordering::Acquire);
    }
    let available = Command::new(ffmpeg_bin())
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    FFMPEG_VALUE.store(available, Ordering::Release);
    FFMPEG_CACHED.store(true, Ordering::Release);
    available
}

/// Check whether ffprobe is available on the system (result cached after first call).
pub fn ffprobe_available() -> bool {
    if FFPROBE_CACHED.load(Ordering::Acquire) {
        return FFPROBE_VALUE.load(Ordering::Acquire);
    }
    let available = Command::new(ffprobe_bin())
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    FFPROBE_VALUE.store(available, Ordering::Release);
    FFPROBE_CACHED.store(true, Ordering::Release);
    available
}

/// Reset ffmpeg/ffprobe availability caches (for testing).
pub fn reset_ffmpeg_cache() {
    FFMPEG_CACHED.store(false, Ordering::Release);
    FFPROBE_CACHED.store(false, Ordering::Release);
}

fn run_command_with_piped_input(
    bin: String,
    input: &[u8],
    args: &[&str],
) -> Result<Output, MediaError> {
    let mut child = Command::new(&bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| MediaError::FfmpegNotFound)?;

    std::thread::scope(|scope| {
        let writer = child.stdin.take().map(|mut stdin| {
            scope.spawn(move || {
                // Ignore broken pipe errors because some ffmpeg/ffprobe invocations
                // may stop reading stdin once they have enough input.
                let _ = stdin.write_all(input);
            })
        });

        let output = child
            .wait_with_output()
            .map_err(|e| MediaError::FfmpegFailed {
                status: -1,
                stderr: e.to_string(),
            })?;

        if let Some(writer) = writer {
            let _ = writer.join();
        }

        Ok(output)
    })
}

/// Run ffmpeg with the given input bytes piped to stdin, using the provided arguments.
/// Returns the stdout output on success.
pub fn run_ffmpeg(input: &[u8], args: &[&str]) -> Result<Vec<u8>, MediaError> {
    if !ffmpeg_available() {
        return Err(MediaError::FfmpegNotFound);
    }

    let output = run_command_with_piped_input(ffmpeg_bin(), input, args)?;

    if !output.status.success() {
        return Err(MediaError::FfmpegFailed {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    if output.stdout.is_empty() {
        return Err(MediaError::FfmpegFailed {
            status: 0,
            stderr: "ffmpeg produced no output".to_string(),
        });
    }

    Ok(output.stdout)
}

/// Run ffprobe with the given input bytes piped to stdin, using the provided arguments.
/// Returns the stdout output as a string on success.
pub fn run_ffprobe(input: &[u8], args: &[&str]) -> Result<String, MediaError> {
    if !ffprobe_available() {
        return Err(MediaError::FfmpegNotFound);
    }

    let output = run_command_with_piped_input(ffprobe_bin(), input, args)?;

    if !output.status.success() {
        return Err(MediaError::FfmpegFailed {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
