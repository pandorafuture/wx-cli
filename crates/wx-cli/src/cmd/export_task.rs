use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use rusqlite::Connection;

use crate::cmd::export_media::{export_image_bytes, MediaAsset, MediaKind, MediaStats};
use crate::schema::EnrichedMessage;
use crate::util::{format_month, sanitize_filename};
use wx_db::MessageContent;
use wx_media::DatDecryptOptions;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Pre-scanned typed task descriptor produced by the classify stage.
#[derive(Debug, Clone)]
pub enum MediaTask {
    Image {
        md5: String,
        msg_index: usize,
    },
    Voice {
        server_id: i64,
        msg_index: usize,
    },
    Video {
        md5: String,
        create_time: i64,
        msg_index: usize,
    },
    File {
        md5: String,
        create_time: i64,
        title: Option<String>,
        msg_index: usize,
    },
}

impl MediaTask {
    pub fn kind(&self) -> TaskKind {
        match self {
            MediaTask::Image { .. } => TaskKind::Image,
            MediaTask::Voice { .. } => TaskKind::Voice,
            MediaTask::Video { .. } => TaskKind::Video,
            MediaTask::File { .. } => TaskKind::File,
        }
    }

    pub fn msg_index(&self) -> usize {
        match self {
            MediaTask::Image { msg_index, .. } => *msg_index,
            MediaTask::Voice { msg_index, .. } => *msg_index,
            MediaTask::Video { msg_index, .. } => *msg_index,
            MediaTask::File { msg_index, .. } => *msg_index,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Image,
    Voice,
    Video,
    File,
}

/// Result of resolving a single task.
#[derive(Debug)]
pub struct ResolvedAsset {
    pub msg_index: usize,
    pub asset: Option<MediaAsset>,
    pub tags: Vec<TaskTag>,
    pub error: Option<ExportError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTag {
    ThumbnailImage,
    SilkVoice,
    WxgfTranscoded,
    WxgfFallback,
    FallbackVideo,
    FallbackFile,
    SkippedVideo,
    SkippedFile,
}

impl TaskTag {
    /// Whether this tag should be counted for duplicate messages.
    ///
    /// Matches old `MediaBridge` behavior:
    /// - Image tags (Thumbnail, WxgfTranscoded, WxgfFallback): NOT counted for duplicates
    ///   because old code returned early when `exported.insert()` failed (before counting).
    /// - All other tags: counted for duplicates because old code counted them
    ///   before or regardless of the `exported.insert()` check.
    pub fn counts_on_duplicate(self) -> bool {
        match self {
            TaskTag::ThumbnailImage => false,
            TaskTag::WxgfTranscoded => false,
            TaskTag::WxgfFallback => false,
            TaskTag::SilkVoice => true,
            TaskTag::FallbackVideo => true,
            TaskTag::FallbackFile => true,
            TaskTag::SkippedVideo => true,
            TaskTag::SkippedFile => true,
        }
    }
}

/// Structured error for a single failed task.
#[derive(Debug)]
pub struct ExportError {
    pub task_kind: &'static str,
    pub key: String,
    pub reason: String,
}

/// Aggregated error summary with grouped reporting.
#[derive(Debug, Default)]
pub struct ErrorSummary {
    pub errors: Vec<ExportError>,
}

impl ErrorSummary {
    pub fn print_report(&self) {
        if self.errors.is_empty() {
            return;
        }
        let mut groups: HashMap<&str, Vec<&ExportError>> = HashMap::new();
        for e in &self.errors {
            groups.entry(e.task_kind).or_default().push(e);
        }
        for (kind, errs) in groups {
            eprintln!("media errors [{kind}]: {} failure(s)", errs.len());
            for e in errs {
                eprintln!("  - {}: {}", e.key, e.reason);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Write gate — thread-safe output filename dedup
// ---------------------------------------------------------------------------

pub struct WriteGate {
    written: Mutex<HashSet<String>>,
}

impl WriteGate {
    pub fn new() -> Self {
        Self {
            written: Mutex::new(HashSet::new()),
        }
    }

    /// Try to claim a filename. Returns `true` if this thread should write.
    pub fn claim(&self, filename: &str) -> bool {
        self.written.lock().unwrap().insert(filename.to_string())
    }
}

// ---------------------------------------------------------------------------
// Thread-local connection pools
// ---------------------------------------------------------------------------

/// Per-thread connection pool for voice media_*.db files.
pub struct VoiceConnectionPool {
    db_paths: Vec<PathBuf>,
    path_key: u64,
}

impl VoiceConnectionPool {
    pub fn new(media_dir: &Path) -> Self {
        let db_paths = wx_media::find_media_dbs(media_dir).unwrap_or_default();
        let path_key = Self::compute_path_key(&db_paths);
        Self { db_paths, path_key }
    }

    fn compute_path_key(paths: &[PathBuf]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        let mut sorted: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        sorted.sort();
        for p in &sorted {
            p.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn open_all(&self) -> Vec<Connection> {
        let mut conns = Vec::new();
        for path in &self.db_paths {
            if let Ok(conn) =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            {
                conns.push(conn);
            }
        }
        conns
    }

    pub fn with_connections<R>(&self, f: impl FnOnce(&[Connection]) -> R) -> R {
        thread_local! {
            static CONNS: RefCell<Option<(u64, Vec<Connection>)>> = const { RefCell::new(None) };
        }
        CONNS.with(|cell| {
            let mut borrow = cell.borrow_mut();
            if let Some((key, conns)) = borrow.as_ref() {
                if *key == self.path_key {
                    return f(conns);
                }
            }
            let conns = self.open_all();
            *borrow = Some((self.path_key, conns));
            f(borrow.as_ref().unwrap().1.as_slice())
        })
    }
}

/// Per-thread connection pool for hardlink.db.
pub struct HardlinkConnectionPool {
    db_path: PathBuf,
    path_key: u64,
}

impl HardlinkConnectionPool {
    pub fn new(db_path: PathBuf) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        db_path.hash(&mut hasher);
        let path_key = hasher.finish();
        Self { db_path, path_key }
    }

    fn open(&self) -> Option<Connection> {
        Connection::open_with_flags(&self.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    }

    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> Option<R> {
        thread_local! {
            static CONN: RefCell<Option<(u64, Connection)>> = const { RefCell::new(None) };
        }
        CONN.with(|cell| {
            let mut borrow = cell.borrow_mut();
            if let Some((key, conn)) = borrow.as_ref() {
                if *key == self.path_key {
                    return Some(f(conn));
                }
            }
            let conn = self.open()?;
            *borrow = Some((self.path_key, conn));
            Some(f(&borrow.as_ref().unwrap().1))
        })
    }
}

// ---------------------------------------------------------------------------
// Shared context — immutable, Arc-shared across rayon threads
// ---------------------------------------------------------------------------

pub struct SharedContext {
    pub attach_dir: PathBuf,
    pub media_dir: PathBuf,
    pub file_dir: PathBuf,
    pub video_dir: PathBuf,
    pub output_media_dir: PathBuf,
    pub dat_opts: DatDecryptOptions,
    pub talker: String,
    pub voice_chat_name_id_hint: Arc<Mutex<Option<i64>>>,
    pub voice_pool: VoiceConnectionPool,
    pub hardlink_pool: HardlinkConnectionPool,
    pub write_gate: WriteGate,
}

// ---------------------------------------------------------------------------
// DupMap — dedup tracking
// ---------------------------------------------------------------------------

pub struct DupMap {
    /// (duplicate msg_index, canonical msg_index)
    pub duplicates: Vec<(usize, usize)>,
}

// ---------------------------------------------------------------------------
// Pipeline functions
// ---------------------------------------------------------------------------

/// Build shared context from account/session info (pre-compute stage).
#[allow(clippy::too_many_arguments)]
pub fn build_shared_context(
    attach_dir: PathBuf,
    media_dir: PathBuf,
    file_dir: PathBuf,
    video_dir: PathBuf,
    hardlink_db: PathBuf,
    output_media_dir: PathBuf,
    talker: &str,
    dat_opts: DatDecryptOptions,
) -> SharedContext {
    // Pre-detect XOR key
    let mut dat_opts = dat_opts;
    let username_hash = format!("{:x}", wx_media::md5_hash(talker.as_bytes()));
    let talker_attach = attach_dir.join(&username_hash);
    if let Some(key) = wx_media::detect_xor_key(&talker_attach) {
        dat_opts.xor_key = Some(key);
    }

    // Pre-cache ffmpeg availability (OnceLock, one-time check)
    let _ = wx_media::ffmpeg_available();

    SharedContext {
        voice_chat_name_id_hint: Arc::new(Mutex::new(None)),
        voice_pool: VoiceConnectionPool::new(&media_dir),
        hardlink_pool: HardlinkConnectionPool::new(hardlink_db),
        write_gate: WriteGate::new(),
        attach_dir,
        media_dir,
        file_dir,
        video_dir,
        output_media_dir,
        dat_opts,
        talker: talker.to_string(),
    }
}

fn update_voice_chat_name_id_hint(ctx: &SharedContext, blob: &wx_media::VoiceBlob) {
    if let Some(chat_name_id) = blob.chat_name_id {
        if let Ok(mut hint) = ctx.voice_chat_name_id_hint.lock() {
            *hint = Some(chat_name_id);
        }
    }
}

/// Stage 1: Classify messages into typed tasks.
pub fn classify(messages: &[EnrichedMessage]) -> Vec<MediaTask> {
    let mut tasks = Vec::new();
    for (idx, em) in messages.iter().enumerate() {
        match &em.message.content {
            MessageContent::Image { md5: Some(md5) } => {
                tasks.push(MediaTask::Image {
                    md5: md5.clone(),
                    msg_index: idx,
                });
            }
            MessageContent::Voice => {
                tasks.push(MediaTask::Voice {
                    server_id: em.message.server_id,
                    msg_index: idx,
                });
            }
            MessageContent::Video { md5: Some(md5) } => {
                tasks.push(MediaTask::Video {
                    md5: md5.clone(),
                    create_time: em.message.create_time,
                    msg_index: idx,
                });
            }
            MessageContent::File {
                md5: Some(md5),
                title,
                ..
            } => {
                tasks.push(MediaTask::File {
                    md5: md5.clone(),
                    create_time: em.message.create_time,
                    title: title.clone(),
                    msg_index: idx,
                });
            }
            _ => {}
        }
    }
    tasks
}

/// Stage 2: Deduplicate tasks by content key.
///
/// Image/voice are safely deduped (md5/server_id fully determines output).
/// Video/file are NOT deduped when they have different fallback parameters
/// (create_time/title), since different parameters may hit different source files.
pub fn dedup(tasks: Vec<MediaTask>) -> (Vec<MediaTask>, DupMap) {
    let mut canonical: HashMap<String, usize> = HashMap::new();
    let mut duplicates: Vec<(usize, usize)> = Vec::new();
    let mut unique: Vec<MediaTask> = Vec::new();

    for task in tasks {
        let msg_idx = task.msg_index();
        let (key, can_dedup) = match &task {
            MediaTask::Image { md5, .. } => (format!("img:{md5}"), true),
            MediaTask::Voice { server_id, .. } => (format!("voi:{server_id}"), true),
            MediaTask::Video {
                md5, create_time, ..
            } => (format!("vid:{md5}:{create_time}"), false),
            MediaTask::File {
                md5,
                create_time,
                title,
                ..
            } => {
                let t = title.as_deref().unwrap_or("");
                (format!("fil:{md5}:{create_time}:{t}"), false)
            }
        };

        if can_dedup {
            if let Some(&canonical_msg_idx) = canonical.get(&key) {
                duplicates.push((msg_idx, canonical_msg_idx));
                continue;
            }
        } else {
            // For video/file: only dedup if the key is identical
            // (same md5 AND same fallback params)
            if let Some(&canonical_msg_idx) = canonical.get(&key) {
                duplicates.push((msg_idx, canonical_msg_idx));
                continue;
            }
        }

        canonical.insert(key, msg_idx);
        unique.push(task);
    }

    (unique, DupMap { duplicates })
}

/// Default rayon thread pool size: min(num_cpus, 4).
fn rayon_default_threads() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cpus.min(4)
}

/// Stage 3-4: Parallel resolve.
///
/// Tasks are batched by type, each batch runs in parallel within a rayon thread pool.
/// Progress is reported per-type at ~10% intervals.
pub fn resolve_parallel(
    tasks: Vec<MediaTask>,
    ctx: Arc<SharedContext>,
    parallel: Option<usize>,
) -> (Vec<ResolvedAsset>, ErrorSummary) {
    let num_threads = parallel.unwrap_or_else(rayon_default_threads).max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap();

    // Group by kind
    let mut batches: HashMap<TaskKind, Vec<MediaTask>> = HashMap::new();
    for task in tasks {
        batches.entry(task.kind()).or_default().push(task);
    }

    let order = [
        TaskKind::Image,
        TaskKind::Voice,
        TaskKind::Video,
        TaskKind::File,
    ];
    let mut all_results = Vec::new();
    let mut all_errors = ErrorSummary::default();

    for kind in order {
        let batch = match batches.remove(&kind) {
            Some(b) => b,
            None => continue,
        };
        let total = batch.len();
        if total == 0 {
            continue;
        }

        let counter = AtomicUsize::new(0);
        let kind_label = match kind {
            TaskKind::Image => "image",
            TaskKind::Voice => "voice",
            TaskKind::Video => "video",
            TaskKind::File => "file",
        };

        let results: Vec<ResolvedAsset> = pool.install(|| {
            batch
                .par_iter()
                .map(|task| {
                    let result = resolve_one(task, &ctx);
                    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let prev_threshold = (done - 1) * 10 / total;
                    let cur_threshold = done * 10 / total;
                    if cur_threshold != prev_threshold || done == total {
                        eprintln!("media: {kind_label} {done}/{total}");
                    }
                    result
                })
                .collect()
        });

        for r in results {
            if let Some(e) = &r.error {
                all_errors.errors.push(ExportError {
                    task_kind: kind_label,
                    key: e.key.clone(),
                    reason: e.reason.clone(),
                });
            }
            all_results.push(ResolvedAsset {
                msg_index: r.msg_index,
                asset: r.asset,
                tags: r.tags,
                error: None, // errors collected separately
            });
        }
    }

    (all_results, all_errors)
}

/// Resolve a single task.
fn resolve_one(task: &MediaTask, ctx: &SharedContext) -> ResolvedAsset {
    match task {
        MediaTask::Image { md5, msg_index } => resolve_image(md5, *msg_index, ctx),
        MediaTask::Voice {
            server_id,
            msg_index,
        } => resolve_voice(*server_id, *msg_index, ctx),
        MediaTask::Video {
            md5,
            create_time,
            msg_index,
        } => resolve_video(md5, *create_time, *msg_index, ctx),
        MediaTask::File {
            md5,
            create_time,
            title,
            msg_index,
        } => resolve_file(md5, *create_time, title.as_deref(), *msg_index, ctx),
    }
}

fn resolve_image(md5: &str, msg_index: usize, ctx: &SharedContext) -> ResolvedAsset {
    let lookup = match wx_media::resolve_image_by_md5(&ctx.talker, &ctx.attach_dir, md5) {
        Ok(r) => r,
        Err(e) => {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "image",
                    key: md5.to_string(),
                    reason: format!("resolve failed: {e}"),
                }),
            };
        }
    };

    let dat_path = match lookup.recommended {
        Some(p) => p,
        None => {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "image",
                    key: md5.to_string(),
                    reason: "no recommended .dat".to_string(),
                }),
            };
        }
    };

    let is_thumbnail = dat_path
        .file_name()
        .map(|n| n.to_string_lossy().contains("_t."))
        .unwrap_or(false);

    let data = match std::fs::read(&dat_path) {
        Ok(d) => d,
        Err(e) => {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "image",
                    key: md5.to_string(),
                    reason: format!("read {}: {e}", dat_path.display()),
                }),
            };
        }
    };

    let decoded = match wx_media::decrypt_dat(&data, &ctx.dat_opts) {
        Ok(d) => d,
        Err(e) => {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "image",
                    key: md5.to_string(),
                    reason: format!("decrypt: {e}"),
                }),
            };
        }
    };

    let (image_data, image_ext, wxgf_transcoded, wxgf_fallback) =
        export_image_bytes(decoded.data, &decoded.ext);

    let filename = format!("{md5}.{image_ext}");
    if ctx.write_gate.claim(&filename) {
        let out_path = ctx.output_media_dir.join(&filename);
        if let Err(e) = std::fs::write(&out_path, &image_data) {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "image",
                    key: md5.to_string(),
                    reason: format!("write {}: {e}", out_path.display()),
                }),
            };
        }
    }

    let mut tags = vec![];
    if is_thumbnail {
        tags.push(TaskTag::ThumbnailImage);
    }
    if wxgf_transcoded {
        tags.push(TaskTag::WxgfTranscoded);
    }
    if wxgf_fallback {
        tags.push(TaskTag::WxgfFallback);
    }

    ResolvedAsset {
        msg_index,
        asset: Some(MediaAsset {
            kind: MediaKind::Image,
            filename,
        }),
        tags,
        error: None,
    }
}

fn resolve_voice(server_id: i64, msg_index: usize, ctx: &SharedContext) -> ResolvedAsset {
    let svr_id = server_id.to_string();
    let chat_name_id_hint = ctx
        .voice_chat_name_id_hint
        .lock()
        .ok()
        .and_then(|hint| *hint);

    let blob = ctx.voice_pool.with_connections(|conns| {
        for conn in conns {
            if let Ok(b) = wx_media::extract_voice_with_conn_hint(conn, &svr_id, chat_name_id_hint)
            {
                return Some(b);
            }
        }
        None
    });

    let blob = match blob {
        Some(b) => b,
        None => {
            // Fallback to opening fresh connections
            match wx_media::extract_voice(&ctx.media_dir, &svr_id) {
                Ok(b) => b,
                Err(e) => {
                    return ResolvedAsset {
                        msg_index,
                        asset: None,
                        tags: vec![],
                        error: Some(ExportError {
                            task_kind: "voice",
                            key: svr_id,
                            reason: format!("extract failed: {e}"),
                        }),
                    };
                }
            }
        }
    };

    update_voice_chat_name_id_hint(ctx, &blob);

    let (data, ext, is_silk) = match wx_media::transcode_silk_to_mp3(&blob.data) {
        Ok(result) => {
            let is_silk = !result.transcoded;
            (result.data, result.ext.to_string(), is_silk)
        }
        Err(e) => {
            eprintln!("warning: voice transcode failed for svr_id={svr_id}: {e}");
            (blob.data, "silk".to_string(), true)
        }
    };

    let filename = format!("{svr_id}.{ext}");
    if ctx.write_gate.claim(&filename) {
        let out_path = ctx.output_media_dir.join(&filename);
        if let Err(e) = std::fs::write(&out_path, &data) {
            return ResolvedAsset {
                msg_index,
                asset: None,
                tags: vec![],
                error: Some(ExportError {
                    task_kind: "voice",
                    key: svr_id,
                    reason: format!("write {}: {e}", out_path.display()),
                }),
            };
        }
    }

    let mut tags = vec![];
    if is_silk {
        tags.push(TaskTag::SilkVoice);
    }

    ResolvedAsset {
        msg_index,
        asset: Some(MediaAsset {
            kind: MediaKind::Voice,
            filename,
        }),
        tags,
        error: None,
    }
}

fn resolve_video(
    md5: &str,
    create_time: i64,
    msg_index: usize,
    ctx: &SharedContext,
) -> ResolvedAsset {
    // Try hardlink DB first
    let hardlink_result = ctx
        .hardlink_pool
        .with_connection(|conn| wx_media::query_hardlink_with_conn(conn, "video", md5));

    let entries = match hardlink_result {
        Some(Ok(e)) => Some(e),
        Some(Err(e)) => {
            if !matches!(&e, wx_media::MediaError::NotFound(_)) {
                eprintln!("warning: video hardlink query failed for md5={md5}: {e}");
            }
            None
        }
        None => None,
    };

    if let Some(entries) = entries {
        if let Some(entry) = entries.first() {
            let candidates = [
                ctx.attach_dir
                    .join(&entry.dir1)
                    .join(&entry.dir2)
                    .join("Video")
                    .join(&entry.file_name),
                ctx.attach_dir
                    .join(&entry.dir1)
                    .join(&entry.dir2)
                    .join(&entry.file_name),
                ctx.attach_dir
                    .join(&entry.dir1)
                    .join("Video")
                    .join(&entry.file_name),
            ];

            if let Some(source) = candidates.iter().find(|p| p.exists()) {
                let filename = entry.file_name.clone();
                if ctx.write_gate.claim(&filename) {
                    let out_path = ctx.output_media_dir.join(&filename);
                    if let Err(e) = std::fs::copy(source, &out_path) {
                        return ResolvedAsset {
                            msg_index,
                            asset: None,
                            tags: vec![],
                            error: Some(ExportError {
                                task_kind: "video",
                                key: md5.to_string(),
                                reason: format!("copy {}: {e}", source.display()),
                            }),
                        };
                    }
                }
                return ResolvedAsset {
                    msg_index,
                    asset: Some(MediaAsset {
                        kind: MediaKind::Video,
                        filename,
                    }),
                    tags: vec![],
                    error: None,
                };
            }
        }
    }

    // Fallback: directory scan
    let month = format_month(create_time);
    match wx_media::find_video_by_md5(&ctx.video_dir, md5, &month) {
        Some(source) => {
            let filename = format!("{md5}.mp4");
            if ctx.write_gate.claim(&filename) {
                let out_path = ctx.output_media_dir.join(&filename);
                if let Err(e) = std::fs::copy(&source, &out_path) {
                    return ResolvedAsset {
                        msg_index,
                        asset: None,
                        tags: vec![],
                        error: Some(ExportError {
                            task_kind: "video",
                            key: md5.to_string(),
                            reason: format!("copy fallback {}: {e}", source.display()),
                        }),
                    };
                }
            }
            ResolvedAsset {
                msg_index,
                asset: Some(MediaAsset {
                    kind: MediaKind::Video,
                    filename,
                }),
                tags: vec![TaskTag::FallbackVideo],
                error: None,
            }
        }
        None => ResolvedAsset {
            msg_index,
            asset: None,
            tags: vec![TaskTag::SkippedVideo],
            error: None,
        },
    }
}

fn resolve_file(
    md5: &str,
    create_time: i64,
    title: Option<&str>,
    msg_index: usize,
    ctx: &SharedContext,
) -> ResolvedAsset {
    // Try hardlink DB first
    let hardlink_result = ctx
        .hardlink_pool
        .with_connection(|conn| wx_media::query_hardlink_with_conn(conn, "file", md5));

    let entries = match hardlink_result {
        Some(Ok(e)) => Some(e),
        Some(Err(e)) => {
            if !matches!(&e, wx_media::MediaError::NotFound(_)) {
                eprintln!("warning: file hardlink query failed for md5={md5}: {e}");
            }
            None
        }
        None => None,
    };

    if let Some(entries) = entries {
        if let Some(entry) = entries.first() {
            let candidates = [
                ctx.file_dir
                    .join(&entry.dir1)
                    .join(&entry.dir2)
                    .join(&entry.file_name),
                ctx.file_dir.join(&entry.dir1).join(&entry.file_name),
            ];

            if let Some(source) = candidates.iter().find(|p| p.exists()) {
                let filename = format!("{}_{}", md5, entry.file_name);
                if ctx.write_gate.claim(&filename) {
                    let out_path = ctx.output_media_dir.join(&filename);
                    if let Err(e) = std::fs::copy(source, &out_path) {
                        return ResolvedAsset {
                            msg_index,
                            asset: None,
                            tags: vec![],
                            error: Some(ExportError {
                                task_kind: "file",
                                key: md5.to_string(),
                                reason: format!("copy {}: {e}", source.display()),
                            }),
                        };
                    }
                }
                return ResolvedAsset {
                    msg_index,
                    asset: Some(MediaAsset {
                        kind: MediaKind::File,
                        filename,
                    }),
                    tags: vec![],
                    error: None,
                };
            }
        }
    }

    // Fallback: directory scan by title
    if let Some(t) = title {
        let month = format_month(create_time);
        if let Some(source) = wx_media::find_file_by_name(&ctx.file_dir, t, &month) {
            let basename = std::path::Path::new(t)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| t.to_string());
            let safe_name = sanitize_filename(&basename);
            let filename = format!("{md5}_{safe_name}");
            if ctx.write_gate.claim(&filename) {
                let out_path = ctx.output_media_dir.join(&filename);
                if let Err(e) = std::fs::copy(&source, &out_path) {
                    return ResolvedAsset {
                        msg_index,
                        asset: None,
                        tags: vec![],
                        error: Some(ExportError {
                            task_kind: "file",
                            key: md5.to_string(),
                            reason: format!("copy fallback {}: {e}", source.display()),
                        }),
                    };
                }
            }
            return ResolvedAsset {
                msg_index,
                asset: Some(MediaAsset {
                    kind: MediaKind::File,
                    filename,
                }),
                tags: vec![TaskTag::FallbackFile],
                error: None,
            };
        }
    }

    ResolvedAsset {
        msg_index,
        asset: None,
        tags: vec![TaskTag::SkippedFile],
        error: None,
    }
}

/// Stage 5: Collect resolved assets back into a media_map indexed by message position.
///
/// Also populates MediaStats from TaskTags.
pub fn collect(
    results: Vec<ResolvedAsset>,
    dup_map: &DupMap,
    total_messages: usize,
) -> (Vec<Vec<MediaAsset>>, MediaStats, ErrorSummary) {
    let mut media_map: Vec<Vec<MediaAsset>> = vec![vec![]; total_messages];
    let mut stats = MediaStats::default();
    let mut errors = ErrorSummary::default();

    // Build index from results by msg_index
    let mut by_index: HashMap<usize, (Option<MediaAsset>, Vec<TaskTag>)> = HashMap::new();
    for r in results {
        if let Some(e) = r.error {
            errors.errors.push(e);
        }
        by_index.insert(r.msg_index, (r.asset, r.tags));
    }

    // Place canonical results — count tags always, copy asset only when present.
    // Matches old MediaBridge: SkippedVideo/SkippedFile stats counted unconditionally;
    // image stats (ThumbnailImage, WxgfTranscoded, WxgfFallback) also counted
    // because canonical always does the full resolve.
    for (msg_idx, (asset, tags)) in &by_index {
        apply_tags(&mut stats, tags);
        if let Some(a) = asset {
            media_map[*msg_idx].push(a.clone());
        }
    }

    // Resolve duplicates — copy the canonical task's asset to duplicate msg positions.
    // Step 1: Count tags that should be counted per-message (SilkVoice, FallbackVideo,
    // FallbackFile, SkippedVideo, SkippedFile) — always, even when asset is None.
    // Step 2: Copy the asset to duplicate msg positions (only when asset exists).
    // This two-step approach matches old MediaBridge behavior where skipped/fallback
    // stats were counted regardless of dedup, but image stats only counted once.
    for (dup_msg_idx, canonical_msg_idx) in &dup_map.duplicates {
        if let Some((asset, tags)) = by_index.get(canonical_msg_idx) {
            let dup_tags: Vec<TaskTag> = tags
                .iter()
                .copied()
                .filter(|t| t.counts_on_duplicate())
                .collect();
            apply_tags(&mut stats, &dup_tags);
            if let Some(a) = asset {
                media_map[*dup_msg_idx].push(a.clone());
            }
        }
    }

    (media_map, stats, errors)
}

fn apply_tags(stats: &mut MediaStats, tags: &[TaskTag]) {
    for tag in tags {
        match tag {
            TaskTag::ThumbnailImage => stats.thumbnail_images += 1,
            TaskTag::SilkVoice => stats.silk_voices += 1,
            TaskTag::WxgfTranscoded => stats.wxgf_transcoded += 1,
            TaskTag::WxgfFallback => stats.wxgf_fallback += 1,
            TaskTag::FallbackVideo => stats.fallback_videos += 1,
            TaskTag::FallbackFile => stats.fallback_files += 1,
            TaskTag::SkippedVideo => stats.skipped_videos += 1,
            TaskTag::SkippedFile => stats.skipped_files += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_classify_empty_messages() {
        let tasks = classify(&[]);
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_dedup_no_duplicates() {
        let tasks = vec![
            MediaTask::Image {
                md5: "aaa".to_string(),
                msg_index: 0,
            },
            MediaTask::Image {
                md5: "bbb".to_string(),
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 2);
        assert!(dup_map.duplicates.is_empty());
    }

    #[test]
    fn test_dedup_duplicate_md5_image() {
        let tasks = vec![
            MediaTask::Image {
                md5: "same".to_string(),
                msg_index: 0,
            },
            MediaTask::Image {
                md5: "same".to_string(),
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);
        assert_eq!(dup_map.duplicates.len(), 1);
        assert_eq!(dup_map.duplicates[0].0, 1); // duplicate msg_index
    }

    #[test]
    fn test_dedup_video_different_create_time_not_deduped() {
        let tasks = vec![
            MediaTask::Video {
                md5: "same".to_string(),
                create_time: 1000,
                msg_index: 0,
            },
            MediaTask::Video {
                md5: "same".to_string(),
                create_time: 2000,
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 2);
        assert!(dup_map.duplicates.is_empty());
    }

    #[test]
    fn test_dedup_file_different_title_not_deduped() {
        let tasks = vec![
            MediaTask::File {
                md5: "same".to_string(),
                create_time: 1000,
                title: Some("file_a.pdf".to_string()),
                msg_index: 0,
            },
            MediaTask::File {
                md5: "same".to_string(),
                create_time: 1000,
                title: Some("file_b.pdf".to_string()),
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 2);
        assert!(dup_map.duplicates.is_empty());
    }

    #[test]
    fn test_dedup_voice_same_server_id() {
        let tasks = vec![
            MediaTask::Voice {
                server_id: 123,
                msg_index: 0,
            },
            MediaTask::Voice {
                server_id: 123,
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);
        assert_eq!(dup_map.duplicates.len(), 1);
    }

    #[test]
    fn test_write_gate_prevents_duplicate_writes() {
        let gate = WriteGate::new();
        assert!(gate.claim("file1.jpg"));
        assert!(!gate.claim("file1.jpg")); // second claim returns false
        assert!(gate.claim("file2.jpg"));
    }

    // --- Parity / integration tests ---

    /// Verify that duplicate messages get the same asset but tags are NOT double-counted.
    /// This matches the old MediaBridge behavior where `exported.insert()` returned early
    /// for duplicates, skipping stat increments.
    #[test]
    fn test_collect_duplicate_image_no_double_tag_count() {
        // Two image messages with same md5 → dedup removes one, collect copies asset
        let tasks = vec![
            MediaTask::Image {
                md5: "abc123".to_string(),
                msg_index: 0,
            },
            MediaTask::Image {
                md5: "abc123".to_string(),
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);
        assert_eq!(dup_map.duplicates.len(), 1);

        // Simulate resolve producing a thumbnail + wxgf_transcoded asset
        let results = vec![ResolvedAsset {
            msg_index: 0,
            asset: Some(MediaAsset {
                kind: MediaKind::Image,
                filename: "abc123.png".to_string(),
            }),
            tags: vec![TaskTag::ThumbnailImage, TaskTag::WxgfTranscoded],
            error: None,
        }];

        let (media_map, stats, _errors) = collect(results, &dup_map, 2);

        // Both messages should have the asset
        assert_eq!(media_map[0].len(), 1);
        assert_eq!(media_map[1].len(), 1);
        assert_eq!(media_map[0][0].filename, "abc123.png");
        assert_eq!(media_map[1][0].filename, "abc123.png");

        // Tags should only be counted once (for the canonical task), not for the duplicate
        assert_eq!(stats.thumbnail_images, 1);
        assert_eq!(stats.wxgf_transcoded, 1);
    }

    /// Verify that duplicate voice messages DO count silk_voices for each duplicate.
    /// Matches old MediaBridge where silk was counted BEFORE the export check.
    #[test]
    fn test_collect_duplicate_voice_counts_silk() {
        let tasks = vec![
            MediaTask::Voice {
                server_id: 42,
                msg_index: 0,
            },
            MediaTask::Voice {
                server_id: 42,
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);

        let results = vec![ResolvedAsset {
            msg_index: 0,
            asset: Some(MediaAsset {
                kind: MediaKind::Voice,
                filename: "42.mp3".to_string(),
            }),
            tags: vec![TaskTag::SilkVoice],
            error: None,
        }];

        let (media_map, stats, _errors) = collect(results, &dup_map, 2);

        assert_eq!(media_map[0].len(), 1);
        assert_eq!(media_map[1].len(), 1);
        // silk_voices counted for each message (old behavior: counted before export check)
        assert_eq!(stats.silk_voices, 2);
    }

    /// Verify that duplicate video fallback counts fallback_videos for each duplicate.
    #[test]
    fn test_collect_duplicate_video_fallback_counts() {
        let tasks = vec![
            MediaTask::Video {
                md5: "v1".to_string(),
                create_time: 1000,
                msg_index: 0,
            },
            MediaTask::Video {
                md5: "v1".to_string(),
                create_time: 1000,
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);

        let results = vec![ResolvedAsset {
            msg_index: 0,
            asset: Some(MediaAsset {
                kind: MediaKind::Video,
                filename: "v1.mp4".to_string(),
            }),
            tags: vec![TaskTag::FallbackVideo],
            error: None,
        }];

        let (_, stats, _) = collect(results, &dup_map, 2);

        // fallback_videos counted for each message (old behavior)
        assert_eq!(stats.fallback_videos, 2);
    }

    /// Verify that duplicate skipped videos DO count skipped_videos for each duplicate.
    /// Matches old MediaBridge where skipped_videos was counted unconditionally.
    #[test]
    fn test_collect_duplicate_skipped_video_counts() {
        let tasks = vec![
            MediaTask::Video {
                md5: "missing".to_string(),
                create_time: 1000,
                msg_index: 0,
            },
            MediaTask::Video {
                md5: "missing".to_string(),
                create_time: 1000,
                msg_index: 1,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 1);

        let results = vec![ResolvedAsset {
            msg_index: 0,
            asset: None,
            tags: vec![TaskTag::SkippedVideo],
            error: None,
        }];

        let (_, stats, _) = collect(results, &dup_map, 2);

        // skipped_videos counted for each message (old behavior: unconditional count)
        assert_eq!(stats.skipped_videos, 2);
    }

    /// Verify dedup produces consistent output: two identical image messages
    /// result in the same asset being placed at both msg positions.
    #[test]
    fn test_dedup_produces_consistent_output() {
        let tasks = vec![
            MediaTask::Image {
                md5: "img1".to_string(),
                msg_index: 0,
            },
            MediaTask::Image {
                md5: "img2".to_string(),
                msg_index: 1,
            },
            MediaTask::Image {
                md5: "img1".to_string(), // duplicate of msg 0
                msg_index: 2,
            },
        ];
        let (unique, dup_map) = dedup(tasks);
        assert_eq!(unique.len(), 2);
        assert_eq!(dup_map.duplicates.len(), 1);
        assert_eq!(dup_map.duplicates[0], (2, 0)); // msg 2 is dup of canonical msg 0

        // Simulate resolve for the 2 unique tasks
        let results = vec![
            ResolvedAsset {
                msg_index: 0,
                asset: Some(MediaAsset {
                    kind: MediaKind::Image,
                    filename: "img1.jpg".to_string(),
                }),
                tags: vec![],
                error: None,
            },
            ResolvedAsset {
                msg_index: 1,
                asset: Some(MediaAsset {
                    kind: MediaKind::Image,
                    filename: "img2.jpg".to_string(),
                }),
                tags: vec![],
                error: None,
            },
        ];

        let (media_map, _stats, _errors) = collect(results, &dup_map, 3);

        // All 3 messages should have their assets
        assert_eq!(media_map[0].len(), 1);
        assert_eq!(media_map[0][0].filename, "img1.jpg");
        assert_eq!(media_map[1].len(), 1);
        assert_eq!(media_map[1][0].filename, "img2.jpg");
        assert_eq!(media_map[2].len(), 1);
        assert_eq!(media_map[2][0].filename, "img1.jpg"); // duplicate gets canonical's asset
    }

    /// Verify classify correctly maps message types to tasks.
    #[test]
    fn test_classify_mixed_messages() {
        use crate::schema::EnrichedMessage;
        use wx_context::Direction;
        use wx_db::Message;

        let msgs = vec![
            EnrichedMessage {
                message: Message {
                    sort_seq: 0,
                    server_id: 1,
                    msg_type: 3,
                    sub_type: 0,
                    sender: "a".into(),
                    talker: "b".into(),
                    create_time: 1000,
                    content: MessageContent::Image {
                        md5: Some("md5_a".into()),
                    },
                    status: 0,
                },
                sender_display_name: "A".into(),
                direction: Direction::Incoming,
                snippet: String::new(),
            },
            EnrichedMessage {
                message: Message {
                    sort_seq: 1,
                    server_id: 2,
                    msg_type: 34,
                    sub_type: 0,
                    sender: "a".into(),
                    talker: "b".into(),
                    create_time: 1001,
                    content: MessageContent::Voice,
                    status: 0,
                },
                sender_display_name: "A".into(),
                direction: Direction::Incoming,
                snippet: String::new(),
            },
            EnrichedMessage {
                message: Message {
                    sort_seq: 2,
                    server_id: 3,
                    msg_type: 43,
                    sub_type: 0,
                    sender: "a".into(),
                    talker: "b".into(),
                    create_time: 1002,
                    content: MessageContent::Text("hello".into()),
                    status: 0,
                },
                sender_display_name: "A".into(),
                direction: Direction::Incoming,
                snippet: "hello".into(),
            },
        ];

        let tasks = classify(&msgs);
        assert_eq!(tasks.len(), 2); // Text message skipped

        // First task: Image
        assert!(matches!(
            &tasks[0],
            MediaTask::Image { md5, msg_index: 0 } if md5 == "md5_a"
        ));

        // Second task: Voice
        assert!(matches!(
            &tasks[1],
            MediaTask::Voice {
                server_id: 2,
                msg_index: 1
            }
        ));
    }

    /// End-to-end parity test: resolve an image through the new parallel pipeline
    /// and compare with the old MediaBridge serial oracle.
    /// Uses real .dat file I/O with mock XOR-encrypted data.
    #[test]
    fn test_parallel_equals_serial_image_resolve() {
        use crate::cmd::export_media::MediaBridge;

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let talker = "wxid_testuser";
        let md5 = "4865625c4e99e4d3b0959a0fe84f41cd";
        let xor_key = 0xa5u8;

        // Create a sample .dat file: XOR-encrypted embedded PNG WXGF
        let png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99,
            0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let mut wxgf = b"wxgfmetadata".to_vec();
        wxgf.extend_from_slice(&png);
        let encrypted: Vec<u8> = wxgf.iter().map(|b| b ^ xor_key).collect();

        let username_hash = format!("{:x}", wx_media::md5_hash(talker.as_bytes()));
        let img_dir = root
            .join("attach")
            .join(&username_hash)
            .join("2026-03")
            .join("Img");
        std::fs::create_dir_all(&img_dir).unwrap();
        std::fs::write(img_dir.join(format!("{md5}.dat")), &encrypted).unwrap();

        let output_media = root.join("output");
        std::fs::create_dir_all(&output_media).unwrap();

        let dat_opts = wx_media::DatDecryptOptions {
            v2_aes_key: None,
            xor_key: Some(xor_key),
        };

        // --- Serial oracle (old MediaBridge) ---
        let mut bridge = MediaBridge::new(
            root.join("attach"),
            root.join("media"),
            root.join("file"),
            root.join("video"),
            root.join("hardlink.db"),
            output_media.clone(),
            dat_opts.clone(),
        );
        let serial_assets = bridge.resolve(
            &wx_db::Message {
                sort_seq: 0,
                server_id: 1,
                msg_type: 3,
                sub_type: 0,
                sender: "sender".into(),
                talker: talker.into(),
                create_time: 1_700_000_000,
                content: wx_db::MessageContent::Image {
                    md5: Some(md5.to_string()),
                },
                status: 0,
            },
            talker,
        );

        // --- New parallel pipeline ---
        let ctx = build_shared_context(
            root.join("attach"),
            root.join("media"),
            root.join("file"),
            root.join("video"),
            root.join("hardlink.db"),
            output_media.clone(),
            talker,
            dat_opts,
        );

        // Clean output dir so the new pipeline can write
        let _ = std::fs::remove_dir_all(&output_media);
        std::fs::create_dir_all(&output_media).unwrap();

        let tasks = vec![MediaTask::Image {
            md5: md5.to_string(),
            msg_index: 0,
        }];
        let (results, _) = resolve_parallel(tasks, std::sync::Arc::new(ctx), Some(1));
        let (media_map, stats, _) = collect(results, &DupMap { duplicates: vec![] }, 1);

        // Compare: both should produce the same filename
        assert_eq!(serial_assets.len(), 1);
        assert_eq!(media_map[0].len(), 1);
        assert_eq!(serial_assets[0].filename, media_map[0][0].filename);

        // Both should have written the same file
        let serial_bytes = std::fs::read(output_media.join(&serial_assets[0].filename)).unwrap();
        let parallel_bytes = std::fs::read(output_media.join(&media_map[0][0].filename)).unwrap();
        assert_eq!(serial_bytes, parallel_bytes);

        // Stats should match (wxgf_transcoded = 1 in both)
        assert_eq!(bridge.stats.wxgf_transcoded, stats.wxgf_transcoded);
    }

    #[cfg(feature = "audio")]
    fn sample_silk() -> Vec<u8> {
        silk_rs::encode_silk(vec![0_u8; 24_000 / 1_000 * 40 * 2], 24_000, 24_000, true).unwrap()
    }

    #[cfg(feature = "audio")]
    fn create_voice_media_db(path: &Path, rows: &[(i64, i64, i64, i64, &[u8])]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE VoiceInfo (
                chat_name_id INTEGER,
                create_time INTEGER,
                local_id INTEGER,
                svr_id INTEGER,
                voice_data BLOB,
                data_index TEXT DEFAULT '0'
            );
            CREATE INDEX VoiceInfo_INDEX ON VoiceInfo(chat_name_id, svr_id);",
        )
        .unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO VoiceInfo (chat_name_id, create_time, local_id, svr_id, voice_data)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .unwrap();
        for &(chat_name_id, create_time, local_id, svr_id, data) in rows {
            stmt.execute(rusqlite::params![
                chat_name_id,
                create_time,
                local_id,
                svr_id,
                data
            ])
            .unwrap();
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn test_resolve_voice_caches_chat_name_id_hint_after_first_lookup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let media_dir = tmp.path().join("media");
        let output_media = tmp.path().join("output");
        std::fs::create_dir_all(&media_dir).unwrap();
        std::fs::create_dir_all(&output_media).unwrap();

        let silk = sample_silk();
        create_voice_media_db(
            &media_dir.join("media_0.db"),
            &[(55, 1000, 1, 101, &silk), (55, 1001, 2, 102, &silk)],
        );

        let ctx = Arc::new(build_shared_context(
            tmp.path().join("attach"),
            media_dir,
            tmp.path().join("file"),
            tmp.path().join("video"),
            tmp.path().join("hardlink.db"),
            output_media,
            "he593121260",
            wx_media::DatDecryptOptions::default(),
        ));

        assert_eq!(*ctx.voice_chat_name_id_hint.lock().unwrap(), None);

        let first = resolve_voice(101, 0, &ctx);
        assert!(first.error.is_none());
        assert_eq!(*ctx.voice_chat_name_id_hint.lock().unwrap(), Some(55));

        let second = resolve_voice(102, 1, &ctx);
        assert!(second.error.is_none());
        assert_eq!(*ctx.voice_chat_name_id_hint.lock().unwrap(), Some(55));
    }
}
