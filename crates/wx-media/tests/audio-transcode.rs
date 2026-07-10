use wx_media::MediaError;

#[cfg(feature = "audio")]
fn silent_pcm_frame() -> Vec<u8> {
    vec![0_u8; 24_000 / 1_000 * 40 * 2]
}

#[cfg(feature = "audio")]
fn sample_silk() -> Vec<u8> {
    silk_rs::encode_silk(silent_pcm_frame(), 24_000, 24_000, true).unwrap()
}

#[cfg(feature = "audio")]
fn long_sample_silk() -> Vec<u8> {
    silk_rs::encode_silk(
        vec![0_u8; silent_pcm_frame().len() * 250],
        24_000,
        24_000,
        true,
    )
    .unwrap()
}

#[cfg(feature = "audio")]
#[test]
fn audio_transcode_ogg_returns_ogg_when_ffmpeg_is_available() {
    if !wx_media::ffmpeg_available() {
        return;
    }

    let result = wx_media::transcode_silk_to_ogg_opus(&sample_silk()).unwrap();
    assert_eq!(result.ext, "ogg");
    assert_eq!(result.mime, "audio/ogg");
    assert!(result.transcoded);
    assert!(!result.data.is_empty());
}

#[cfg(feature = "audio")]
#[test]
fn audio_transcode_ogg_errors_when_ffmpeg_is_missing() {
    if wx_media::ffmpeg_available() {
        return;
    }

    let result = wx_media::transcode_silk_to_ogg_opus(&sample_silk());
    assert!(matches!(result, Err(MediaError::FfmpegNotFound)));
}

#[cfg(feature = "audio")]
#[test]
fn audio_transcode_mp3_keeps_existing_compatibility() {
    let result = wx_media::transcode_silk_to_mp3(&sample_silk()).unwrap();
    if wx_media::ffmpeg_available() {
        assert_eq!(result.ext, "mp3");
        assert_eq!(result.mime, "audio/mpeg");
        assert!(result.transcoded);
        assert!(!result.data.is_empty());
    } else {
        assert_eq!(result.ext, "silk");
        assert_eq!(result.mime, "audio/x-silk");
        assert!(!result.transcoded);
    }
}

#[cfg(feature = "audio")]
#[test]
fn audio_transcode_mp3_handles_long_audio_without_hanging() {
    if !wx_media::ffmpeg_available() {
        return;
    }

    let output = run_long_audio_child(std::time::Duration::from_secs(5));
    assert!(
        output.status.success(),
        "child failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("long-audio-ok"));
}

#[cfg(feature = "audio")]
#[test]
fn audio_transcode_long_audio_child_mode() {
    if std::env::var_os("WECHAT_MEDIA_LONG_AUDIO_CHILD").is_none() {
        return;
    }

    let result = wx_media::transcode_silk_to_mp3(&long_sample_silk()).unwrap();
    assert_eq!(result.ext, "mp3");
    assert_eq!(result.mime, "audio/mpeg");
    assert!(result.transcoded);
    assert!(!result.data.is_empty());
    println!("long-audio-ok {}", result.data.len());
}

#[cfg(feature = "audio")]
fn run_long_audio_child(timeout: std::time::Duration) -> std::process::Output {
    let current_exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(current_exe)
        .arg("--exact")
        .arg("audio_transcode_long_audio_child_mode")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("WECHAT_MEDIA_LONG_AUDIO_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "child timed out after {:?}: stdout={}\nstderr={}",
                timeout,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(not(feature = "audio"))]
#[test]
fn audio_transcode_feature_disabled_returns_error() {
    let result = wx_media::transcode_silk_to_mp3(b"ignored");
    assert!(matches!(result, Err(MediaError::AudioFeatureDisabled)));

    let result = wx_media::transcode_silk_to_ogg_opus(b"ignored");
    assert!(matches!(result, Err(MediaError::AudioFeatureDisabled)));
}
