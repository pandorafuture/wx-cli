//! WeChat media decryption, transcoding, and resource resolution.
//!
//! # Decrypt a `.dat` image file
//!
//! ```no_run
//! use wx_media::{decrypt_dat, DatDecryptOptions};
//!
//! let data = std::fs::read("image.dat").unwrap();
//! // XOR / V1 files need no extra key:
//! let result = decrypt_dat(&data, &DatDecryptOptions::default()).unwrap();
//! std::fs::write(format!("output.{}", result.ext), &result.data).unwrap();
//!
//! // V2 files require a 16-byte AES key:
//! let opts = DatDecryptOptions {
//!     v2_aes_key: Some(*b"abcdefghijklmnop"),
//!     xor_key: None,
//! };
//! let result = decrypt_dat(&data, &opts).unwrap();
//! ```
//!
//! # Transcode a WXGF image to PNG/GIF
//!
//! ```no_run
//! use wx_media::transcode_wxgf;
//!
//! let wxgf_data = std::fs::read("image.wxgf").unwrap();
//! let result = transcode_wxgf(&wxgf_data).unwrap();
//! std::fs::write(format!("output.{}", result.ext), &result.data).unwrap();
//! // result.transcoded == true if ffmpeg converted to PNG/GIF
//! ```
//!
//! # Decrypt a WeChat Channels video
//!
//! ```no_run
//! use wx_media::decrypt_video;
//!
//! let encrypted = std::fs::read("video.enc").unwrap();
//! let result = decrypt_video(&encrypted, 12345); // seed from decode_key
//! std::fs::write("output.mp4", &result.data).unwrap();
//! // result.is_valid_mp4 == true if "ftyp" signature found
//! ```
//!
//! # Resolve an image by `local_id`
//!
//! ```no_run
//! use std::path::Path;
//! use wx_media::resolve_image;
//!
//! let lookup = resolve_image(
//!     Path::new("db_storage/message/message_resource.db"),
//!     12345,                   // local_id from Message table
//!     "wxid_alice",            // chat partner username
//!     Path::new("msg/attach"), // attach base directory
//! ).unwrap();
//! // lookup.recommended is the best-match .dat file path
//! ```
//!
//! # Extract a voice BLOB
//!
//! ```no_run
//! use std::path::Path;
//! use wx_media::extract_voice;
//!
//! let blob = extract_voice(Path::new("db_storage/media"), "123456789").unwrap();
//! std::fs::write("voice.silk", &blob.data).unwrap();
//! ```

pub mod audio_transcode;
mod dat;
mod error;
mod fallback;
pub mod ffmpeg;
mod hardlink;
mod image_resolver;
pub mod image_transcode;
pub mod isaac64;
pub mod key;
mod resource;
mod types;
pub mod video_decrypt;
mod voice;
pub mod wxgf;

pub use audio_transcode::{transcode_silk_to_mp3, transcode_silk_to_ogg_opus};
pub use dat::{decrypt_dat, detect_dat_format, detect_image_type, detect_xor_key};
pub use error::MediaError;
pub use fallback::{find_file_by_name, find_video_by_md5};
pub use ffmpeg::{
    ffmpeg_available, ffprobe_available, reset_ffmpeg_cache, run_ffmpeg, run_ffprobe,
};
pub use hardlink::{query_hardlink, query_hardlink_with_conn};
pub use image_resolver::{resolve_image, resolve_image_by_md5};
pub use image_transcode::transcode_wxgf;
pub use isaac64::Isaac64;
pub use key::{derive_v2_aes_key, derive_v2_key_from_dir, extract_wxid, read_uin};
pub use resource::extract_md5_from_packed_info;
pub use types::{
    DatDecryptOptions, DatFormat, DecodedImage, DecryptVideoResult, HardlinkEntry, ImageType,
    MediaLookupResult, TranscodeAudioResult, TranscodeImageResult, VoiceBlob,
};
pub use video_decrypt::{decrypt_video, decrypt_video_with_keystream};
pub use voice::{
    extract_voice, extract_voice_with_conn, extract_voice_with_conn_hint, find_media_dbs,
};
pub use wxgf::{parse_wxgf, WxgfContent};

/// Compute MD5 hash of bytes, returning the `md5::Digest` (displays as hex).
pub fn md5_hash(data: &[u8]) -> md5::Digest {
    md5::compute(data)
}
