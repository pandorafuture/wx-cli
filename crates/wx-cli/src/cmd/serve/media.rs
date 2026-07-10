use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use wx_db::{open_readonly_connection, Message, MessageContent, MessageQuery, SortOrder, WechatDb};

use super::error::ServeError;
use super::state::{AppState, CachedVoicePayload};
use crate::util::{format_month, sanitize_filename};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaFormat {
    Ogg,
    Mp3,
}

impl MediaFormat {
    pub fn parse(value: Option<&str>) -> Result<Self, ServeError> {
        match value.unwrap_or("ogg").to_ascii_lowercase().as_str() {
            "ogg" => Ok(Self::Ogg),
            "mp3" => Ok(Self::Mp3),
            other => Err(ServeError::InvalidParam(format!(
                "unsupported media format: {other}"
            ))),
        }
    }
}

pub struct MediaRequest {
    pub server_id: i64,
    pub talker: String,
    pub format: MediaFormat,
}

enum MediaPayload {
    InlineBytes {
        bytes: Vec<u8>,
        content_type: &'static str,
    },
    ServePath {
        path: PathBuf,
        content_type: &'static str,
        disposition: Option<String>,
    },
}

impl MediaPayload {
    async fn into_response(self, request: Request) -> Result<Response, ServeError> {
        match self {
            Self::InlineBytes {
                bytes,
                content_type,
            } => {
                let mut response = bytes.into_response();
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
                Ok(response)
            }
            Self::ServePath {
                path,
                content_type,
                disposition,
            } => {
                let mut response = ServeFile::new(&path)
                    .oneshot(request)
                    .await
                    .map_err(|never| match never {})?
                    .map(Body::new);
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
                if let Some(disposition) = disposition {
                    let value = HeaderValue::from_str(&disposition).map_err(|e| {
                        ServeError::Internal(format!(
                            "invalid content disposition for {}: {e}",
                            path.display()
                        ))
                    })?;
                    response.headers_mut().insert(CONTENT_DISPOSITION, value);
                }
                Ok(response)
            }
        }
    }
}

pub async fn serve_media(
    state: Arc<AppState>,
    media_request: MediaRequest,
    request: Request,
) -> Result<Response, ServeError> {
    let payload = resolve_media(state, media_request).await?;
    payload.into_response(request).await
}

async fn resolve_media(
    state: Arc<AppState>,
    media_request: MediaRequest,
) -> Result<MediaPayload, ServeError> {
    let db = Arc::clone(&state.db);
    let attach_dir = state.attach_dir.clone();
    let file_dir = state.file_dir.clone();
    let video_dir = state.video_dir.clone();
    let hardlink_db_path = state.hardlink_db_path.clone();
    let hardlink_db_conn = state.hardlink_db_conn.clone();
    let raw_key = state.raw_key;
    let dat_decrypt = state.dat_decrypt.clone();
    let voice_cache = Arc::clone(&state.voice_cache);
    let image_xor_cache = Arc::clone(&state.image_xor_cache);
    let visibility = Arc::clone(&state.visibility);
    let state_for_cache = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        let message = {
            let mut guard = db.lock().map_err(|e| ServeError::Internal(e.to_string()))?;
            lookup_message(&mut guard, &media_request.talker, media_request.server_id)?
        };
        let canonical_talker = message.talker.clone();

        if !visibility.allows_media_for_sender(&canonical_talker, &message.sender) {
            return Err(ServeError::NotFound(format!(
                "message not found for talker={}, server_id={}",
                media_request.talker, media_request.server_id
            )));
        }

        match message.content {
            MessageContent::Image { md5: Some(md5) } => resolve_image(
                &canonical_talker,
                &md5,
                &attach_dir,
                dat_decrypt,
                &image_xor_cache,
            ),
            MessageContent::Image { md5: None } => Err(ServeError::NotFound(format!(
                "image metadata missing for server_id={}",
                media_request.server_id
            ))),
            MessageContent::Voice => {
                let db_paths = {
                    let mut cache_guard = state_for_cache.media_db_paths.lock().map_err(
                        |e: std::sync::PoisonError<_>| ServeError::Internal(e.to_string()),
                    )?;
                    match cache_guard.as_ref() {
                        Some(paths) => paths.clone(),
                        None => {
                            let loaded = find_media_db_paths(&state_for_cache.media_db_dir)?;
                            *cache_guard = Some(loaded.clone());
                            loaded
                        }
                    }
                };
                resolve_voice(
                    media_request.server_id,
                    media_request.format,
                    &db_paths,
                    raw_key,
                    &voice_cache,
                )
            }
            MessageContent::Video { md5: Some(md5) } => resolve_video(
                &md5,
                message.create_time,
                &attach_dir,
                &video_dir,
                &hardlink_db_conn,
                &hardlink_db_path,
                raw_key,
            ),
            MessageContent::Video { md5: None } => Err(ServeError::NotFound(format!(
                "video metadata missing for server_id={}",
                media_request.server_id
            ))),
            MessageContent::File {
                md5: Some(md5),
                title,
                ..
            } => resolve_file(
                &md5,
                title.as_deref(),
                message.create_time,
                &file_dir,
                &hardlink_db_conn,
                &hardlink_db_path,
                raw_key,
            ),
            MessageContent::File { md5: None, .. } => Err(ServeError::NotFound(format!(
                "file metadata missing for server_id={}",
                media_request.server_id
            ))),
            _ => Err(ServeError::UnsupportedMedia(format!(
                "message type {} is not supported by /api/v1/media",
                message.msg_type
            ))),
        }
    })
    .await
    .map_err(|e| ServeError::Internal(e.to_string()))?
}

fn lookup_message(db: &mut WechatDb, talker: &str, server_id: i64) -> Result<Message, ServeError> {
    let query = MessageQuery::for_talker(talker)
        .around_server_id(server_id)
        .context(0)
        .limit(1)
        .order(SortOrder::Desc);
    let result = db
        .query_messages_anchor(&query)
        .map_err(|e| ServeError::Db(e.to_string()))?;

    result
        .items
        .into_iter()
        .find(|message| message.server_id == server_id)
        .ok_or_else(|| {
            ServeError::NotFound(format!(
                "message not found for talker={talker}, server_id={server_id}"
            ))
        })
}

fn resolve_image(
    talker: &str,
    md5: &str,
    attach_dir: &Path,
    mut dat_decrypt: wx_media::DatDecryptOptions,
    image_xor_cache: &Arc<std::sync::Mutex<lru::LruCache<String, Option<u8>>>>,
) -> Result<MediaPayload, ServeError> {
    if dat_decrypt.xor_key.is_none() {
        dat_decrypt.xor_key = cached_xor_key(talker, attach_dir, image_xor_cache);
    }

    let lookup = wx_media::resolve_image_by_md5(talker, attach_dir, md5)
        .map_err(|e| map_media_lookup_err(md5, e))?;
    let dat_path = lookup.recommended.ok_or_else(|| {
        ServeError::NotFound(format!("no candidate image file found for md5={md5}"))
    })?;
    let data = std::fs::read(&dat_path)
        .map_err(|e| ServeError::Internal(format!("failed to read {}: {e}", dat_path.display())))?;
    let decoded = wx_media::decrypt_dat(&data, &dat_decrypt)
        .map_err(|e| ServeError::Internal(format!("decrypt_dat failed for {md5}: {e}")))?;

    if decoded.ext == "wxgf" {
        let transcoded =
            wx_media::transcode_wxgf(&decoded.data).map_err(|e| map_wxgf_err(md5, e))?;
        if !transcoded.transcoded {
            return Err(ServeError::UnsupportedMedia(format!(
                "wxgf image for md5={md5} contains HEVC content that cannot be served directly; {}",
                wx_media::MediaError::ffmpeg_install_hint()
            )));
        }
        return Ok(MediaPayload::InlineBytes {
            bytes: transcoded.data,
            content_type: image_content_type(transcoded.ext),
        });
    }

    Ok(MediaPayload::InlineBytes {
        bytes: decoded.data,
        content_type: image_content_type(&decoded.ext),
    })
}

fn resolve_voice(
    server_id: i64,
    format: MediaFormat,
    db_paths: &[PathBuf],
    raw_key: Option<[u8; 32]>,
    voice_cache: &Arc<std::sync::Mutex<lru::LruCache<String, CachedVoicePayload>>>,
) -> Result<MediaPayload, ServeError> {
    let cache_key = format!(
        "{}:{}",
        server_id,
        match format {
            MediaFormat::Ogg => "ogg",
            MediaFormat::Mp3 => "mp3",
        }
    );
    if let Ok(mut cache) = voice_cache.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(MediaPayload::InlineBytes {
                bytes: cached.bytes.clone(),
                content_type: cached.content_type,
            });
        }
    }

    let svr_id = server_id.to_string();
    let mut first_db_error: Option<String> = None;

    for db_path in db_paths {
        let conn = match open_readonly_connection(db_path, raw_key.as_ref()) {
            Ok(conn) => conn,
            Err(err) => {
                if first_db_error.is_none() {
                    first_db_error = Some(err.to_string());
                }
                continue;
            }
        };

        match wx_media::extract_voice_with_conn(&conn, &svr_id) {
            Ok(blob) => {
                let result = match format {
                    MediaFormat::Ogg => wx_media::transcode_silk_to_ogg_opus(&blob.data),
                    MediaFormat::Mp3 => wx_media::transcode_silk_to_mp3(&blob.data),
                }
                .map_err(|err| map_audio_err(server_id, format, err))?;

                if !result.transcoded {
                    return Err(voice_transcode_unavailable(server_id, format));
                }

                if let Ok(mut cache) = voice_cache.lock() {
                    cache.put(
                        cache_key.clone(),
                        CachedVoicePayload {
                            bytes: result.data.clone(),
                            content_type: result.mime,
                        },
                    );
                }

                return Ok(MediaPayload::InlineBytes {
                    bytes: result.data,
                    content_type: result.mime,
                });
            }
            Err(wx_media::MediaError::LookupMiss(_))
            | Err(wx_media::MediaError::SchemaMissing(_)) => continue,
            Err(err) => {
                if first_db_error.is_none() {
                    first_db_error = Some(err.to_string());
                }
            }
        }
    }

    if let Some(err) = first_db_error {
        return Err(ServeError::Db(err));
    }

    Err(ServeError::NotFound(format!(
        "voice asset not found for server_id={server_id}"
    )))
}

fn resolve_video(
    md5: &str,
    create_time: i64,
    attach_dir: &Path,
    video_dir: &Path,
    hardlink_db_conn: &Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    hardlink_db_path: &Path,
    raw_key: Option<[u8; 32]>,
) -> Result<MediaPayload, ServeError> {
    let mut candidates = Vec::new();
    match query_hardlink_entries(hardlink_db_conn, hardlink_db_path, raw_key, "video", md5) {
        Ok(entries) => {
            if let Some(entry) = entries.first() {
                candidates.push(
                    attach_dir
                        .join(&entry.dir1)
                        .join(&entry.dir2)
                        .join("Video")
                        .join(&entry.file_name),
                );
                candidates.push(
                    attach_dir
                        .join(&entry.dir1)
                        .join(&entry.dir2)
                        .join(&entry.file_name),
                );
                candidates.push(
                    attach_dir
                        .join(&entry.dir1)
                        .join("Video")
                        .join(&entry.file_name),
                );
                candidates.push(
                    video_dir
                        .join(&entry.dir1)
                        .join(&entry.dir2)
                        .join(&entry.file_name),
                );
                candidates.push(video_dir.join(&entry.dir1).join(&entry.file_name));
                candidates.push(video_dir.join(&entry.file_name));

                if let Some(path) = candidates.iter().find(|path| path.exists()) {
                    return Ok(MediaPayload::ServePath {
                        path: path.clone(),
                        content_type: video_content_type(path),
                        disposition: path.file_name().map(|name| {
                            format!(
                                "inline; filename=\"{}\"",
                                sanitize_header_filename(&name.to_string_lossy())
                            )
                        }),
                    });
                }
            }
        }
        Err(ServeError::NotFound(_)) => {}
        Err(err) => return Err(err),
    }

    let month = format_month(create_time);
    let fallback = wx_media::find_video_by_md5(video_dir, md5, &month).ok_or_else(|| {
        ServeError::NotFound(format!(
            "video asset not found for md5={md5}; the video may not be downloaded locally in WeChat yet"
        ))
    })?;
    Ok(MediaPayload::ServePath {
        path: fallback.clone(),
        content_type: video_content_type(&fallback),
        disposition: fallback.file_name().map(|name| {
            format!(
                "inline; filename=\"{}\"",
                sanitize_header_filename(&name.to_string_lossy())
            )
        }),
    })
}

fn resolve_file(
    md5: &str,
    title: Option<&str>,
    create_time: i64,
    file_dir: &Path,
    hardlink_db_conn: &Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    hardlink_db_path: &Path,
    raw_key: Option<[u8; 32]>,
) -> Result<MediaPayload, ServeError> {
    match query_hardlink_entries(hardlink_db_conn, hardlink_db_path, raw_key, "file", md5) {
        Ok(entries) => {
            if let Some(entry) = entries.first() {
                let candidates = [
                    file_dir
                        .join(&entry.dir1)
                        .join(&entry.dir2)
                        .join(&entry.file_name),
                    file_dir.join(&entry.dir1).join(&entry.file_name),
                ];
                if let Some(path) = candidates.iter().find(|path| path.exists()) {
                    return Ok(MediaPayload::ServePath {
                        path: path.clone(),
                        content_type: file_content_type(path),
                        disposition: Some(format!(
                            "attachment; filename=\"{}\"",
                            sanitize_header_filename(&entry.file_name)
                        )),
                    });
                }
            }
        }
        Err(ServeError::NotFound(_)) => {}
        Err(err) => return Err(err),
    }

    let title = title.ok_or_else(|| {
        ServeError::NotFound(format!(
            "file asset not found for md5={md5} (missing title)"
        ))
    })?;
    let month = format_month(create_time);
    let fallback = wx_media::find_file_by_name(file_dir, title, &month)
        .ok_or_else(|| ServeError::NotFound(format!("file asset not found for md5={md5}")))?;
    let file_name = fallback
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| sanitize_filename(title));
    Ok(MediaPayload::ServePath {
        path: fallback,
        content_type: file_content_type(Path::new(&file_name)),
        disposition: Some(format!(
            "attachment; filename=\"{}\"",
            sanitize_header_filename(&file_name)
        )),
    })
}

fn query_hardlink_entries(
    hardlink_db_conn: &Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    hardlink_db_path: &Path,
    raw_key: Option<[u8; 32]>,
    media_type: &str,
    key: &str,
) -> Result<Vec<wx_media::HardlinkEntry>, ServeError> {
    let mut guard = hardlink_db_conn
        .lock()
        .map_err(|e| ServeError::Internal(format!("hardlink db lock failed: {e}")))?;

    // Lazily (re)open connection if absent (e.g. after refresh cleared it).
    if guard.is_none() && hardlink_db_path.exists() {
        match open_readonly_connection(hardlink_db_path, raw_key.as_ref()) {
            Ok(conn) => {
                eprintln!("server/hardlink: reopened pooled connection");
                *guard = Some(conn);
            }
            Err(e) => {
                eprintln!("warn: cannot reopen hardlink.db: {e}");
            }
        }
    }

    let result = if let Some(ref conn) = *guard {
        wx_media::query_hardlink_with_conn(conn, media_type, key)
            .map_err(|e| map_media_lookup_err(key, e))
    } else {
        // Fallback: open a new connection (file may not exist or open failed)
        let conn = open_readonly_connection(hardlink_db_path, raw_key.as_ref())
            .map_err(|e| ServeError::Db(e.to_string()))?;
        wx_media::query_hardlink_with_conn(&conn, media_type, key)
            .map_err(|e| map_media_lookup_err(key, e))
    };
    result
}

fn find_media_db_paths(media_db_dir: &Path) -> Result<Vec<PathBuf>, ServeError> {
    let entries = std::fs::read_dir(media_db_dir).map_err(|_| {
        ServeError::NotFound(format!(
            "media database directory not found: {}",
            media_db_dir.display()
        ))
    })?;

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    (name == "media.db" || name.starts_with("media_")) && name.ends_with(".db")
                })
        })
        .collect();
    paths.sort();

    if paths.is_empty() {
        return Err(ServeError::NotFound(format!(
            "no media databases found under {}",
            media_db_dir.display()
        )));
    }

    Ok(paths)
}

fn image_content_type(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
}

fn video_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

fn file_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "txt" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn map_media_lookup_err(key: &str, err: wx_media::MediaError) -> ServeError {
    match err {
        wx_media::MediaError::NotFound(_)
        | wx_media::MediaError::LookupMiss(_)
        | wx_media::MediaError::NoDatFiles { .. }
        | wx_media::MediaError::NoMediaDbs(_) => {
            ServeError::NotFound(format!("media asset not found for key={key}"))
        }
        other => ServeError::Internal(other.to_string()),
    }
}

fn map_audio_err(server_id: i64, format: MediaFormat, err: wx_media::MediaError) -> ServeError {
    match err {
        wx_media::MediaError::FfmpegNotFound => voice_transcode_unavailable(server_id, format),
        wx_media::MediaError::AudioFeatureDisabled => ServeError::Internal(format!(
            "voice transcoding for server_id={server_id} is unavailable because wx-cli was built without the 'audio' feature"
        )),
        wx_media::MediaError::FfmpegFailed { .. }
        | wx_media::MediaError::SilkDecodeFailed { .. } => ServeError::Upstream(format!(
            "voice transcode failed for server_id={server_id}: {err}"
        )),
        other => ServeError::Internal(other.to_string()),
    }
}

fn map_wxgf_err(md5: &str, err: wx_media::MediaError) -> ServeError {
    match err {
        wx_media::MediaError::InvalidWxgf => ServeError::UnsupportedMedia(format!(
            "wxgf image for md5={md5} is invalid or unreadable"
        )),
        wx_media::MediaError::FfmpegNotFound => ServeError::UnsupportedMedia(format!(
            "wxgf image for md5={md5} requires ffmpeg for HEVC decode; {}",
            wx_media::MediaError::ffmpeg_install_hint()
        )),
        wx_media::MediaError::FfmpegFailed { .. } => {
            ServeError::Upstream(format!("wxgf transcode failed for {md5}: {err}"))
        }
        other => ServeError::Internal(format!("wxgf transcode failed for {md5}: {other}")),
    }
}

fn voice_transcode_unavailable(server_id: i64, format: MediaFormat) -> ServeError {
    let format_name = match format {
        MediaFormat::Ogg => "ogg",
        MediaFormat::Mp3 => "mp3",
    };
    ServeError::UnsupportedMedia(format!(
        "voice media for server_id={server_id} requires ffmpeg to produce {format_name}; {}",
        wx_media::MediaError::ffmpeg_install_hint()
    ))
}

fn sanitize_header_filename(name: &str) -> String {
    sanitize_filename(name).replace('"', "_")
}

fn cached_xor_key(
    talker: &str,
    attach_dir: &Path,
    image_xor_cache: &Arc<std::sync::Mutex<lru::LruCache<String, Option<u8>>>>,
) -> Option<u8> {
    if let Ok(mut cache) = image_xor_cache.lock() {
        if let Some(value) = cache.get(talker) {
            return *value;
        }
    }

    let username_hash = format!("{:x}", wx_media::md5_hash(talker.as_bytes()));
    let talker_attach = attach_dir.join(username_hash);
    let detected = wx_media::detect_xor_key(&talker_attach);

    if let Ok(mut cache) = image_xor_cache.lock() {
        cache.put(talker.to_string(), detected);
    }

    detected
}
