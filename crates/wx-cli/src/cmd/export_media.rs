#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use crate::util::{format_month, sanitize_filename};
#[cfg(test)]
use wx_db::{Message, MessageContent};
#[cfg(test)]
use wx_media::DatDecryptOptions;

#[derive(Debug, Clone)]
pub struct MediaAsset {
    pub kind: MediaKind,
    pub filename: String,
}

#[derive(Debug, Default)]
pub struct MediaStats {
    pub skipped_videos: usize,
    pub skipped_files: usize,
    pub fallback_videos: usize,
    pub fallback_files: usize,
    pub thumbnail_images: usize,
    pub silk_voices: usize,
    pub wxgf_transcoded: usize,
    pub wxgf_fallback: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum MediaKind {
    Image,
    Voice,
    Video,
    File,
}

#[cfg(test)]
pub struct MediaBridge {
    attach_dir: PathBuf,
    media_dir: PathBuf,
    file_dir: PathBuf,
    video_dir: PathBuf,
    hardlink_db: PathBuf,
    output_media_dir: PathBuf,
    dat_opts: DatDecryptOptions,
    exported: HashSet<String>,
    xor_key_detected: bool,
    pub stats: MediaStats,
}

#[cfg(test)]
impl MediaBridge {
    pub fn new(
        attach_dir: PathBuf,
        media_dir: PathBuf,
        file_dir: PathBuf,
        video_dir: PathBuf,
        hardlink_db: PathBuf,
        output_media_dir: PathBuf,
        dat_opts: DatDecryptOptions,
    ) -> Self {
        Self {
            attach_dir,
            media_dir,
            file_dir,
            video_dir,
            hardlink_db,
            output_media_dir,
            dat_opts,
            exported: HashSet::new(),
            xor_key_detected: false,
            stats: MediaStats::default(),
        }
    }

    pub fn resolve(&mut self, msg: &Message, talker: &str) -> Vec<MediaAsset> {
        match &msg.content {
            MessageContent::Image { md5: Some(md5) } => self.resolve_image(md5, talker),
            MessageContent::Voice => self.resolve_voice(msg.server_id),
            MessageContent::Video { md5: Some(md5) } => self.resolve_video(md5, msg.create_time),
            MessageContent::File {
                md5: Some(md5),
                title,
                ..
            } => self.resolve_file(md5, msg.create_time, title.as_deref()),
            _ => vec![],
        }
    }

    fn resolve_image(&mut self, md5: &str, talker: &str) -> Vec<MediaAsset> {
        // Lazily detect XOR key from talker's attach subdirectory
        if !self.xor_key_detected {
            let username_hash = format!("{:x}", wx_media::md5_hash(talker.as_bytes()));
            let talker_attach = self.attach_dir.join(&username_hash);
            if let Some(key) = wx_media::detect_xor_key(&talker_attach) {
                self.dat_opts.xor_key = Some(key);
            }
            self.xor_key_detected = true;
        }

        let lookup = match wx_media::resolve_image_by_md5(talker, &self.attach_dir, md5) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("warning: image resolve failed for md5={md5}: {e}");
                return vec![];
            }
        };

        let dat_path = match lookup.recommended {
            Some(p) => p,
            None => {
                eprintln!("warning: no recommended .dat for md5={md5}");
                return vec![];
            }
        };

        // Detect if this is a thumbnail (_t.dat = ~9KB thumbnail, not the compressed _h or original)
        let is_thumbnail = dat_path
            .file_name()
            .map(|n| n.to_string_lossy().contains("_t."))
            .unwrap_or(false);

        let data = match std::fs::read(&dat_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("warning: cannot read {}: {e}", dat_path.display());
                return vec![];
            }
        };

        let decoded = match wx_media::decrypt_dat(&data, &self.dat_opts) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("warning: decrypt_dat failed for md5={md5}: {e}");
                return vec![];
            }
        };

        let (image_data, image_ext, wxgf_transcoded, wxgf_fallback) =
            export_image_bytes(decoded.data, &decoded.ext);

        let filename = format!("{}.{}", md5, image_ext);
        if !self.exported.insert(filename.clone()) {
            return vec![MediaAsset {
                kind: MediaKind::Image,
                filename,
            }];
        }

        let out_path = self.output_media_dir.join(&filename);
        if let Err(e) = std::fs::write(&out_path, &image_data) {
            eprintln!("warning: cannot write {}: {e}", out_path.display());
            return vec![];
        }

        if is_thumbnail {
            self.stats.thumbnail_images += 1;
        }
        if wxgf_transcoded {
            self.stats.wxgf_transcoded += 1;
        }
        if wxgf_fallback {
            self.stats.wxgf_fallback += 1;
        }

        vec![MediaAsset {
            kind: MediaKind::Image,
            filename,
        }]
    }

    fn resolve_voice(&mut self, server_id: i64) -> Vec<MediaAsset> {
        let svr_id = server_id.to_string();

        let blob = match wx_media::extract_voice(&self.media_dir, &svr_id) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("warning: voice extract failed for svr_id={svr_id}: {e}");
                return vec![];
            }
        };

        let (data, ext) = match wx_media::transcode_silk_to_mp3(&blob.data) {
            Ok(result) => {
                if !result.transcoded {
                    self.stats.silk_voices += 1;
                }
                (result.data, result.ext.to_string())
            }
            Err(e) => {
                eprintln!("warning: voice transcode failed for svr_id={svr_id}: {e}");
                (blob.data, "silk".to_string())
            }
        };

        let filename = format!("{svr_id}.{ext}");
        if !self.exported.insert(filename.clone()) {
            return vec![MediaAsset {
                kind: MediaKind::Voice,
                filename,
            }];
        }

        let out_path = self.output_media_dir.join(&filename);
        if let Err(e) = std::fs::write(&out_path, &data) {
            eprintln!("warning: cannot write {}: {e}", out_path.display());
            return vec![];
        }

        vec![MediaAsset {
            kind: MediaKind::Voice,
            filename,
        }]
    }

    fn resolve_video(&mut self, md5: &str, create_time: i64) -> Vec<MediaAsset> {
        let entries = match wx_media::query_hardlink(&self.hardlink_db, "video", md5) {
            Ok(e) => e,
            Err(e) => {
                if !matches!(&e, wx_media::MediaError::NotFound(_)) {
                    eprintln!("warning: video hardlink query failed for md5={md5}: {e}");
                }
                // Try directory scan fallback
                let month = format_month(create_time);
                return match wx_media::find_video_by_md5(&self.video_dir, md5, &month) {
                    Some(source) => self.copy_fallback_video(md5, &source),
                    None => {
                        self.stats.skipped_videos += 1;
                        vec![]
                    }
                };
            }
        };

        let entry = match entries.first() {
            Some(e) => e,
            None => return vec![],
        };

        // Try candidate paths to find the physical video file
        let candidates = [
            self.attach_dir
                .join(&entry.dir1)
                .join(&entry.dir2)
                .join("Video")
                .join(&entry.file_name),
            self.attach_dir
                .join(&entry.dir1)
                .join(&entry.dir2)
                .join(&entry.file_name),
            self.attach_dir
                .join(&entry.dir1)
                .join("Video")
                .join(&entry.file_name),
        ];

        let source = match candidates.iter().find(|p| p.exists()) {
            Some(p) => p.clone(),
            None => {
                // Hardlink entry exists but physical file missing; try directory scan
                let month = format_month(create_time);
                return match wx_media::find_video_by_md5(&self.video_dir, md5, &month) {
                    Some(source) => self.copy_fallback_video(md5, &source),
                    None => {
                        self.stats.skipped_videos += 1;
                        vec![]
                    }
                };
            }
        };

        let filename = entry.file_name.clone();
        if !self.exported.insert(filename.clone()) {
            return vec![MediaAsset {
                kind: MediaKind::Video,
                filename,
            }];
        }

        let out_path = self.output_media_dir.join(&filename);
        if let Err(e) = std::fs::copy(&source, &out_path) {
            eprintln!("warning: cannot copy video {}: {e}", source.display());
            return vec![];
        }

        vec![MediaAsset {
            kind: MediaKind::Video,
            filename,
        }]
    }

    fn copy_fallback_video(&mut self, md5: &str, source: &std::path::Path) -> Vec<MediaAsset> {
        let filename = format!("{md5}.mp4");
        if !self.exported.insert(filename.clone()) {
            self.stats.fallback_videos += 1;
            return vec![MediaAsset {
                kind: MediaKind::Video,
                filename,
            }];
        }

        let out_path = self.output_media_dir.join(&filename);
        if let Err(e) = std::fs::copy(source, &out_path) {
            eprintln!(
                "warning: cannot copy fallback video {}: {e}",
                source.display()
            );
            return vec![];
        }

        self.stats.fallback_videos += 1;
        vec![MediaAsset {
            kind: MediaKind::Video,
            filename,
        }]
    }

    fn resolve_file(
        &mut self,
        md5: &str,
        create_time: i64,
        title: Option<&str>,
    ) -> Vec<MediaAsset> {
        let entries = match wx_media::query_hardlink(&self.hardlink_db, "file", md5) {
            Ok(e) => e,
            Err(e) => {
                if !matches!(&e, wx_media::MediaError::NotFound(_)) {
                    eprintln!("warning: file hardlink query failed for md5={md5}: {e}");
                }
                return self.try_file_fallback(md5, create_time, title);
            }
        };

        let entry = match entries.first() {
            Some(e) => e,
            None => {
                return self.try_file_fallback(md5, create_time, title);
            }
        };

        // Try candidate paths to find the physical file
        let candidates = [
            self.file_dir
                .join(&entry.dir1)
                .join(&entry.dir2)
                .join(&entry.file_name),
            self.file_dir.join(&entry.dir1).join(&entry.file_name),
        ];

        let source = match candidates.iter().find(|p| p.exists()) {
            Some(p) => p.clone(),
            None => {
                // Hardlink entry exists but physical file missing; try directory scan
                return self.try_file_fallback(md5, create_time, title);
            }
        };

        let filename = format!("{}_{}", md5, entry.file_name);
        if !self.exported.insert(filename.clone()) {
            return vec![MediaAsset {
                kind: MediaKind::File,
                filename,
            }];
        }

        let out_path = self.output_media_dir.join(&filename);
        if let Err(e) = std::fs::copy(&source, &out_path) {
            eprintln!("warning: cannot copy file {}: {e}", source.display());
            return vec![];
        }

        vec![MediaAsset {
            kind: MediaKind::File,
            filename,
        }]
    }

    fn try_file_fallback(
        &mut self,
        md5: &str,
        create_time: i64,
        title: Option<&str>,
    ) -> Vec<MediaAsset> {
        if let Some(t) = title {
            let month = format_month(create_time);
            if let Some(source) = wx_media::find_file_by_name(&self.file_dir, t, &month) {
                let basename = std::path::Path::new(t)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| t.to_string());
                let safe_name = sanitize_filename(&basename);
                let filename = format!("{md5}_{safe_name}");
                if !self.exported.insert(filename.clone()) {
                    self.stats.fallback_files += 1;
                    return vec![MediaAsset {
                        kind: MediaKind::File,
                        filename,
                    }];
                }

                let out_path = self.output_media_dir.join(&filename);
                if let Err(e) = std::fs::copy(&source, &out_path) {
                    eprintln!(
                        "warning: cannot copy fallback file {}: {e}",
                        source.display()
                    );
                    return vec![];
                }

                self.stats.fallback_files += 1;
                return vec![MediaAsset {
                    kind: MediaKind::File,
                    filename,
                }];
            }
        }
        self.stats.skipped_files += 1;
        vec![]
    }
}

pub fn export_image_bytes(
    decoded_data: Vec<u8>,
    decoded_ext: &str,
) -> (Vec<u8>, String, bool, bool) {
    if decoded_ext != "wxgf" {
        return (decoded_data, decoded_ext.to_string(), false, false);
    }

    match wx_media::transcode_wxgf(&decoded_data) {
        Ok(transcoded) if transcoded.transcoded => {
            (transcoded.data, transcoded.ext.to_string(), true, false)
        }
        Ok(_) => (decoded_data, "wxgf".to_string(), false, true),
        Err(e) => {
            eprintln!("warning: wxgf image export kept as .wxgf due to transcode error: {e}");
            (decoded_data, "wxgf".to_string(), false, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static FFMPEG_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_resolve_video_fallback_when_no_hardlink_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // Setup video_dir with a video file in 2024-03
        let video_dir = root.join("video");
        let month_dir = video_dir.join("2024-03");
        std::fs::create_dir_all(&month_dir).unwrap();
        std::fs::write(month_dir.join("deadbeef.mp4"), b"fake-video").unwrap();

        // Setup output media dir
        let output_media = root.join("output");
        std::fs::create_dir_all(&output_media).unwrap();

        // Use a nonexistent hardlink DB so query_hardlink will fail → triggers fallback
        let mut bridge = MediaBridge::new(
            root.join("attach"),         // attach_dir (unused for this test)
            root.join("media"),          // media_dir (unused)
            root.join("file"),           // file_dir (unused)
            video_dir,                   // video_dir
            root.join("nonexistent.db"), // hardlink_db (will fail)
            output_media.clone(),
            wx_media::DatDecryptOptions::default(),
        );

        // create_time = 2024-03-15T12:00:00Z = 1710504000
        let msg = Message {
            sort_seq: 0,
            server_id: 1,
            msg_type: 43,
            sub_type: 0,
            sender: "wxid_test".into(),
            talker: "wxid_other".into(),
            create_time: 1710504000,
            content: MessageContent::Video {
                md5: Some("deadbeef".into()),
            },
            status: 0,
        };

        let assets = bridge.resolve(&msg, "wxid_other");
        assert_eq!(assets.len(), 1);
        assert!(matches!(assets[0].kind, MediaKind::Video));
        assert_eq!(assets[0].filename, "deadbeef.mp4");
        assert_eq!(bridge.stats.fallback_videos, 1);
        assert!(output_media.join("deadbeef.mp4").exists());
    }

    #[test]
    fn test_resolve_image_transcodes_embedded_wxgf_to_standard_image() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let talker = "wxid_other";
        let md5 = "4865625c4e99e4d3b0959a0fe84f41cd";
        let xor_key = 0xa5;
        let wxgf = sample_embedded_png_wxgf();

        write_xor_dat(root, talker, md5, &wxgf, xor_key);

        let output_media = root.join("output");
        std::fs::create_dir_all(&output_media).unwrap();

        let mut bridge = MediaBridge::new(
            root.join("attach"),
            root.join("media"),
            root.join("file"),
            root.join("video"),
            root.join("hardlink.db"),
            output_media.clone(),
            wx_media::DatDecryptOptions {
                v2_aes_key: None,
                xor_key: Some(xor_key),
            },
        );

        let assets = bridge.resolve_image(md5, talker);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].filename, format!("{md5}.png"));
        assert_eq!(bridge.stats.wxgf_transcoded, 1);
        assert_eq!(bridge.stats.wxgf_fallback, 0);
        assert_eq!(
            std::fs::read(output_media.join(format!("{md5}.png"))).unwrap(),
            sample_png()
        );
    }

    #[test]
    fn test_resolve_image_keeps_wxgf_when_hevc_cannot_be_transcoded() {
        let _guard = FFMPEG_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("FFMPEG_PATH", "/definitely-missing-ffmpeg");
        }
        wx_media::reset_ffmpeg_cache();

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let talker = "wxid_other";
        let md5 = "cdb2f853d5e1cdebbdc66bb8c80e1714";
        let xor_key = 0xa5;
        let wxgf = sample_hevc_wxgf();

        write_xor_dat(root, talker, md5, &wxgf, xor_key);

        let output_media = root.join("output");
        std::fs::create_dir_all(&output_media).unwrap();

        let mut bridge = MediaBridge::new(
            root.join("attach"),
            root.join("media"),
            root.join("file"),
            root.join("video"),
            root.join("hardlink.db"),
            output_media.clone(),
            wx_media::DatDecryptOptions {
                v2_aes_key: None,
                xor_key: Some(xor_key),
            },
        );

        let assets = bridge.resolve_image(md5, talker);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].filename, format!("{md5}.wxgf"));
        assert_eq!(bridge.stats.wxgf_transcoded, 0);
        assert_eq!(bridge.stats.wxgf_fallback, 1);
        assert_eq!(
            std::fs::read(output_media.join(format!("{md5}.wxgf"))).unwrap(),
            wxgf
        );

        unsafe {
            std::env::remove_var("FFMPEG_PATH");
        }
        wx_media::reset_ffmpeg_cache();
    }

    #[test]
    fn test_resolve_image_keeps_wxgf_when_transcode_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let talker = "wxid_other";
        let md5 = "0badf00d0badf00d0badf00d0badf00d";
        let xor_key = 0xa5;
        let wxgf = b"wxgf".to_vec();

        write_xor_dat(root, talker, md5, &wxgf, xor_key);

        let output_media = root.join("output");
        std::fs::create_dir_all(&output_media).unwrap();

        let mut bridge = MediaBridge::new(
            root.join("attach"),
            root.join("media"),
            root.join("file"),
            root.join("video"),
            root.join("hardlink.db"),
            output_media.clone(),
            wx_media::DatDecryptOptions {
                v2_aes_key: None,
                xor_key: Some(xor_key),
            },
        );

        let assets = bridge.resolve_image(md5, talker);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].filename, format!("{md5}.wxgf"));
        assert_eq!(bridge.stats.wxgf_transcoded, 0);
        assert_eq!(bridge.stats.wxgf_fallback, 1);
        assert_eq!(
            std::fs::read(output_media.join(format!("{md5}.wxgf"))).unwrap(),
            wxgf
        );
    }

    fn write_xor_dat(
        root: &std::path::Path,
        talker: &str,
        md5: &str,
        plaintext: &[u8],
        xor_key: u8,
    ) {
        let username_hash = format!("{:x}", wx_media::md5_hash(talker.as_bytes()));
        let img_dir = root
            .join("attach")
            .join(username_hash)
            .join("2026-03")
            .join("Img");
        std::fs::create_dir_all(&img_dir).unwrap();
        let encrypted: Vec<u8> = plaintext.iter().map(|b| b ^ xor_key).collect();
        std::fs::write(img_dir.join(format!("{md5}.dat")), encrypted).unwrap();
    }

    fn sample_embedded_png_wxgf() -> Vec<u8> {
        let mut wxgf = b"wxgfmetadata".to_vec();
        wxgf.extend_from_slice(&sample_png());
        wxgf
    }

    fn sample_hevc_wxgf() -> Vec<u8> {
        let mut wxgf = b"wxgfmetadata".to_vec();
        wxgf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x02, 0x03, 0x04]);
        wxgf
    }

    fn sample_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99,
            0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }
}
