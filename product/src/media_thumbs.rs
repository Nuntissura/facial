//! Async thumbnail engine (WP-043).
//!
//! Off-thread decode workers feed the media browser's thumbnail grid:
//! - priority deque (visible band before prefetch band), generation counter so
//!   stale scroll positions cancel cheaply;
//! - sharded disk cache under `<workspace_root>/.facial/media/thumbs/` keyed by
//!   sha256(path|mtime|size|edge) at fixed edge sizes;
//! - Exif orientation applied at decode; corrupt files produce a memoized
//!   error state (never a retry storm);
//! - in-memory RGBA results are drained on the UI thread with a per-frame
//!   texture budget; texture LRU capped by count (renderer side, WP-044).
//!
//! No UI-thread decoding, no foreground windows, nothing written outside
//! `<workspace_root>/.facial`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Fixed thumbnail edge sizes (long-edge pixels) with on-disk cache.
pub const THUMB_EDGES: [u16; 2] = [256, 512];

/// Pick the cached edge size that best serves a requested display edge.
pub fn edge_for_display(display_edge: f32) -> u16 {
    for edge in THUMB_EDGES {
        if display_edge <= f32::from(edge) {
            return edge;
        }
    }
    THUMB_EDGES[THUMB_EDGES.len() - 1]
}

/// Default disk-cache budget (overridable via config `media_thumb_cache_mb`).
pub const DEFAULT_CACHE_CAP_MB: u64 = 2048;
/// Cache entries older than this are swept regardless of the size cap.
pub const CACHE_MAX_AGE_DAYS: u64 = 90;

/// Request priority band. Visible tiles always dequeue before prefetch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThumbPriority {
    Visible,
    Prefetch,
}

/// Decoded thumbnail pixels ready for texture upload on the UI thread.
pub struct ThumbPixels {
    pub key: ThumbKey,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Result of a completed job.
pub enum ThumbOutcome {
    Ready(ThumbPixels),
    Failed { key: ThumbKey, reason: String },
}

/// Cache/request key: absolute path + target edge.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ThumbKey {
    pub path: String,
    pub edge: u16,
}

struct Job {
    key: ThumbKey,
    generation: u64,
    priority: ThumbPriority,
}

#[derive(Default)]
struct Queues {
    visible: VecDeque<Job>,
    prefetch: VecDeque<Job>,
    /// Video extraction is isolated behind one dedicated worker. A run of
    /// visible videos can therefore never occupy the image decoder pool.
    video_visible: VecDeque<Job>,
    video_prefetch: VecDeque<Job>,
    /// Keys currently queued or being decoded (dedupe).
    in_flight: HashSet<ThumbKey>,
    shutdown: bool,
}

/// Counters surfaced through debug events (model observability).
#[derive(Default)]
pub struct ThumbStats {
    pub decodes: AtomicUsize,
    pub disk_hits: AtomicUsize,
    pub failures: AtomicUsize,
    pub stale_skips: AtomicUsize,
}

struct EngineShared {
    queues: Mutex<Queues>,
    wake: Condvar,
    generation: AtomicU64,
    cache_root: PathBuf,
    stats: ThumbStats,
    repaint: Box<dyn Fn() + Send + Sync>,
}

/// Async thumbnail engine. Owned by the UI; workers shut down on drop.
pub struct ThumbnailEngine {
    shared: Arc<EngineShared>,
    results_rx: mpsc::Receiver<ThumbOutcome>,
    workers: Vec<thread::JoinHandle<()>>,
    /// Memoized failures: never re-queued until `forget_failures`.
    failed: HashMap<ThumbKey, String>,
}

impl ThumbnailEngine {
    /// Cache directory for a workspace.
    pub fn cache_root(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".facial").join("media").join("thumbs")
    }

    /// Spawn the engine with `max(2, cores/2)` decode workers.
    /// `repaint` is invoked after each completed job so tiles appear without
    /// user input (typically `move || ctx.request_repaint()`). A background
    /// startup sweep enforces the age + size caps without blocking launch.
    pub fn new(workspace_root: &Path, repaint: Box<dyn Fn() + Send + Sync>) -> Self {
        Self::new_with_cache_cap(workspace_root, DEFAULT_CACHE_CAP_MB, repaint)
    }

    pub fn new_with_cache_cap(
        workspace_root: &Path,
        cache_cap_mb: u64,
        repaint: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        let cache_root = Self::cache_root(workspace_root);
        let _ = std::fs::create_dir_all(&cache_root);
        {
            // Startup GC off-thread: launch cost stays flat on huge caches.
            let gc_root = cache_root.clone();
            let _ = thread::Builder::new()
                .name("thumb-gc".to_string())
                .spawn(move || {
                    let _ = Self::gc(&gc_root, cache_cap_mb, CACHE_MAX_AGE_DAYS);
                });
        }
        let worker_count = thread::available_parallelism()
            .map(|n| (n.get() / 2).max(2))
            .unwrap_or(2);
        let (results_tx, results_rx) = mpsc::channel::<ThumbOutcome>();
        let shared = Arc::new(EngineShared {
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
            cache_root,
            stats: ThumbStats::default(),
            repaint,
        });
        let mut workers = Vec::new();
        for index in 0..worker_count {
            let shared = Arc::clone(&shared);
            let tx = results_tx.clone();
            workers.push(
                thread::Builder::new()
                    .name(format!("thumb-worker-{index}"))
                    .spawn(move || worker_loop(shared, tx, false))
                    .expect("spawn thumbnail worker"),
            );
        }
        {
            let shared = Arc::clone(&shared);
            let tx = results_tx.clone();
            workers.push(
                thread::Builder::new()
                    .name("video-thumb-worker".to_string())
                    .spawn(move || worker_loop(shared, tx, true))
                    .expect("spawn video thumbnail worker"),
            );
        }
        Self {
            shared,
            results_rx,
            workers,
            failed: HashMap::new(),
        }
    }

    /// Bump the generation: queued-but-unstarted jobs from older generations
    /// are dropped by workers (cheap cancel on scroll).
    pub fn bump_generation(&self) -> u64 {
        self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Queue a thumbnail request. Deduped against in-flight work; memoized
    /// failures are not retried. Returns true when the job was queued.
    pub fn request(&mut self, path: &str, edge: u16, priority: ThumbPriority) -> bool {
        let key = ThumbKey {
            path: path.to_string(),
            edge,
        };
        if self.failed.contains_key(&key) {
            return false;
        }
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let mut queues = match self.shared.queues.lock() {
            Ok(q) => q,
            Err(_) => return false,
        };
        if queues.in_flight.contains(&key) {
            return false;
        }
        queues.in_flight.insert(key.clone());
        let job = Job {
            key,
            generation,
            priority,
        };
        match (is_video_source(path), priority) {
            // New viewport work goes to the front. A fast scroll must not
            // wait behind every tile that was visible in older frames.
            (false, ThumbPriority::Visible) => queues.visible.push_front(job),
            (false, ThumbPriority::Prefetch) => queues.prefetch.push_back(job),
            (true, ThumbPriority::Visible) => queues.video_visible.push_front(job),
            (true, ThumbPriority::Prefetch) => queues.video_prefetch.push_back(job),
        }
        drop(queues);
        // A shared condition variable serves two worker classes. Wake all so
        // a video request cannot wake only an image worker (or vice versa).
        self.shared.wake.notify_all();
        true
    }

    /// Drain up to `max` completed results on the UI thread. Failures are
    /// memoized; ready pixels are handed to `on_ready` (texture upload).
    pub fn drain_ready(&mut self, max: usize, mut on_ready: impl FnMut(ThumbPixels)) {
        for _ in 0..max {
            match self.results_rx.try_recv() {
                Ok(ThumbOutcome::Ready(pixels)) => on_ready(pixels),
                Ok(ThumbOutcome::Failed { key, reason }) => {
                    self.failed.insert(key, reason);
                }
                Err(_) => break,
            }
        }
    }

    /// Memoized failure reason for a key, if any.
    pub fn failure(&self, path: &str, edge: u16) -> Option<&str> {
        self.failed
            .get(&ThumbKey {
                path: path.to_string(),
                edge,
            })
            .map(String::as_str)
    }

    /// Clear failure memos (workspace switch / explicit refresh).
    pub fn forget_failures(&mut self) {
        self.failed.clear();
    }

    /// Snapshot of counters for debug events.
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        (
            self.shared.stats.decodes.load(Ordering::Relaxed),
            self.shared.stats.disk_hits.load(Ordering::Relaxed),
            self.shared.stats.failures.load(Ordering::Relaxed),
            self.shared.stats.stale_skips.load(Ordering::Relaxed),
        )
    }

    /// Delete cache entries older than `max_age_days` or beyond `cap_mb`
    /// (oldest first). Returns (files_removed, bytes_removed).
    pub fn gc(cache_root: &Path, cap_mb: u64, max_age_days: u64) -> (usize, u64) {
        let mut entries: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        collect_cache_files(cache_root, &mut entries);
        let now = std::time::SystemTime::now();
        let mut removed = 0usize;
        let mut removed_bytes = 0u64;
        // Age sweep.
        entries.retain(|(path, modified, size)| {
            let age_days = now
                .duration_since(*modified)
                .map(|d| d.as_secs() / 86_400)
                .unwrap_or(0);
            if age_days > max_age_days {
                if std::fs::remove_file(path).is_ok() {
                    removed += 1;
                    removed_bytes += *size;
                }
                false
            } else {
                true
            }
        });
        // Size cap, oldest first.
        let cap_bytes = cap_mb.saturating_mul(1024 * 1024);
        let mut total: u64 = entries.iter().map(|(_, _, s)| *s).sum();
        if total > cap_bytes {
            entries.sort_by_key(|(_, modified, _)| *modified);
            for (path, _, size) in entries {
                if total <= cap_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                    removed_bytes += size;
                    total = total.saturating_sub(size);
                }
            }
        }
        (removed, removed_bytes)
    }
}

impl Drop for ThumbnailEngine {
    /// Joins workers after their CURRENT decode finishes — a workspace
    /// switch mid-decode of a huge source can stall for that one decode.
    /// Accepted trade-off: detached workers writing to a dying cache dir
    /// would be worse; decodes are bounded by the thumbnail resize cost.
    fn drop(&mut self) {
        if let Ok(mut queues) = self.shared.queues.lock() {
            queues.shutdown = true;
            queues.visible.clear();
            queues.prefetch.clear();
            queues.video_visible.clear();
            queues.video_prefetch.clear();
        }
        self.shared.wake.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(shared: Arc<EngineShared>, tx: mpsc::Sender<ThumbOutcome>, video_only: bool) {
    loop {
        let job = {
            let mut queues = match shared.queues.lock() {
                Ok(q) => q,
                Err(_) => return,
            };
            loop {
                if queues.shutdown {
                    return;
                }
                let job = if video_only {
                    queues
                        .video_visible
                        .pop_front()
                        .or_else(|| queues.video_prefetch.pop_front())
                } else {
                    queues
                        .visible
                        .pop_front()
                        .or_else(|| queues.prefetch.pop_front())
                };
                if let Some(job) = job {
                    break job;
                }
                queues = match shared.wake.wait(queues) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
        };

        // Any queued job from an older viewport generation is stale. A tile
        // that was visible before a fast scroll must not delay the tiles the
        // operator is looking at now. A decode already in progress completes;
        // queued work is cheap to discard and can be requested again later.
        let current = shared.generation.load(Ordering::SeqCst);
        if job.generation < current {
            shared.stats.stale_skips.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut queues) = shared.queues.lock() {
                queues.in_flight.remove(&job.key);
            }
            continue;
        }

        let outcome = produce_thumbnail(&shared, &job.key);
        if let Ok(mut queues) = shared.queues.lock() {
            queues.in_flight.remove(&job.key);
        }
        match &outcome {
            ThumbOutcome::Ready(_) => {}
            ThumbOutcome::Failed { .. } => {
                shared.stats.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        if tx.send(outcome).is_err() {
            return; // engine dropped
        }
        (shared.repaint)();
    }
}

/// Decode-or-load one thumbnail: disk cache first, else full decode + resize +
/// cache write. All failures become `Failed` outcomes (memoized by the UI).
fn produce_thumbnail(shared: &EngineShared, key: &ThumbKey) -> ThumbOutcome {
    let meta = match std::fs::metadata(&key.path) {
        Ok(m) => m,
        Err(err) => {
            return ThumbOutcome::Failed {
                key: key.clone(),
                reason: format!("stat: {err}"),
            }
        }
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache_path = cache_path_for(&shared.cache_root, &key.path, mtime, meta.len(), key.edge);

    // Disk hit: cached thumbs are pre-rotated and pre-sized.
    if let Ok(bytes) = std::fs::read(&cache_path) {
        if let Ok(img) = image::load_from_memory(&bytes) {
            shared.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
            let rgba = img.to_rgba8();
            return ThumbOutcome::Ready(ThumbPixels {
                key: key.clone(),
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                rgba: rgba.into_raw(),
            });
        }
        let _ = std::fs::remove_file(&cache_path); // corrupt cache entry
    }

    shared.stats.decodes.fetch_add(1, Ordering::Relaxed);
    if is_video_source(&key.path) {
        return produce_video_thumbnail(shared, key, &cache_path);
    }
    let bytes = match std::fs::read(&key.path) {
        Ok(b) => b,
        Err(err) => {
            return ThumbOutcome::Failed {
                key: key.clone(),
                reason: format!("read: {err}"),
            }
        }
    };
    let img = match image::load_from_memory(&bytes) {
        Ok(i) => i,
        Err(err) => {
            return ThumbOutcome::Failed {
                key: key.clone(),
                reason: format!("decode: {err}"),
            }
        }
    };
    let oriented = apply_exif_orientation(img, exif_orientation(&bytes));
    let thumb = oriented.thumbnail(key.edge as u32, key.edge as u32);

    // Best-effort cache write via temp+rename (atomic on same volume).
    // Tmp name carries pid + a process-wide counter: same-key work is already
    // deduped by `in_flight`, but distinct keys can share a shard directory,
    // and a second process may target the same cache.
    if let Some(parent) = cache_path.parent() {
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let _ = std::fs::create_dir_all(parent);
        let tmp = cache_path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut buf = Vec::new();
        let write_ok = {
            let mut cursor = std::io::Cursor::new(&mut buf);
            thumb
                .to_rgb8()
                .write_to(&mut cursor, image::ImageFormat::Jpeg)
                .is_ok()
        };
        if write_ok && std::fs::write(&tmp, &buf).is_ok() {
            let _ = std::fs::rename(&tmp, &cache_path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    let rgba = thumb.to_rgba8();
    ThumbOutcome::Ready(ThumbPixels {
        key: key.clone(),
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

/// Video extensions understood by the media explorer. Kept local to avoid a
/// UI-module dependency from the thumbnail engine.
pub fn is_video_source(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv" | "mpeg" | "mpg")
    )
}

/// Resolve FFmpeg without a machine-specific hardcoded path. Operators can
/// pin a build with `FACIAL_FFMPEG`; otherwise normal PATH discovery applies.
pub fn resolve_ffmpeg() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FACIAL_FFMPEG").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let executable = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(executable))
        .find(|candidate| candidate.is_file())
}

fn produce_video_thumbnail(
    _shared: &EngineShared,
    key: &ThumbKey,
    cache_path: &Path,
) -> ThumbOutcome {
    let Some(ffmpeg) = resolve_ffmpeg() else {
        return ThumbOutcome::Failed {
            key: key.clone(),
            reason: "video thumbnail unavailable: set FACIAL_FFMPEG or add ffmpeg to PATH"
                .to_string(),
        };
    };
    let Some(parent) = cache_path.parent() else {
        return ThumbOutcome::Failed {
            key: key.clone(),
            reason: "video thumbnail cache path has no parent".to_string(),
        };
    };
    let _ = std::fs::create_dir_all(parent);
    static VIDEO_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let frame_path = parent.join(format!(
        ".video-frame-{}-{}.png",
        std::process::id(),
        VIDEO_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // Seek near the beginning for a useful frame. If the clip is shorter than
    // one second, retry at zero. Both attempts are bounded and run only on the
    // single video worker.
    let mut last_error = String::new();
    for seek in ["1.0", "0"] {
        let _ = std::fs::remove_file(&frame_path);
        match run_ffmpeg_frame(&ffmpeg, Path::new(&key.path), &frame_path, key.edge, seek) {
            Ok(()) if frame_path.is_file() => match image::open(&frame_path) {
                Ok(frame) => {
                    let thumb = frame.thumbnail(key.edge as u32, key.edge as u32);
                    write_cached_jpeg(&thumb, cache_path);
                    let _ = std::fs::remove_file(&frame_path);
                    let rgba = thumb.to_rgba8();
                    return ThumbOutcome::Ready(ThumbPixels {
                        key: key.clone(),
                        width: rgba.width() as usize,
                        height: rgba.height() as usize,
                        rgba: rgba.into_raw(),
                    });
                }
                Err(error) => last_error = format!("decode extracted frame: {error}"),
            },
            Ok(()) => last_error = "ffmpeg produced no frame".to_string(),
            Err(error) => last_error = error,
        }
    }
    let _ = std::fs::remove_file(&frame_path);
    ThumbOutcome::Failed {
        key: key.clone(),
        reason: last_error,
    }
}

fn run_ffmpeg_frame(
    ffmpeg: &Path,
    source: &Path,
    output: &Path,
    edge: u16,
    seek: &str,
) -> Result<(), String> {
    let filter =
        format!("scale={edge}:{edge}:force_original_aspect_ratio=decrease:flags=fast_bilinear");
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-ss",
            seek,
            "-i",
        ])
        .arg(source)
        .args(["-frames:v", "1", "-vf", &filter, "-y"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("start ffmpeg: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("ffmpeg exited with {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("ffmpeg thumbnail extraction timed out after 5 seconds".to_string());
            }
            Err(error) => return Err(format!("wait for ffmpeg: {error}")),
        }
    }
}

fn write_cached_jpeg(img: &image::DynamicImage, cache_path: &Path) {
    let Some(parent) = cache_path.parent() else {
        return;
    };
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let _ = std::fs::create_dir_all(parent);
    let tmp = cache_path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut buf = Vec::new();
    let write_ok = {
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.to_rgb8()
            .write_to(&mut cursor, image::ImageFormat::Jpeg)
            .is_ok()
    };
    if write_ok && std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, cache_path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Cache file path: sharded by the first two hex chars of
/// sha256(`path|mtime|size|edge`); filenames never embed source paths.
pub fn cache_path_for(
    cache_root: &Path,
    source_path: &str,
    mtime: u64,
    size: u64,
    edge: u16,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(source_path.as_bytes());
    hasher.update(b"|");
    hasher.update(mtime.to_le_bytes());
    hasher.update(b"|");
    hasher.update(size.to_le_bytes());
    hasher.update(b"|");
    hasher.update(edge.to_le_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    cache_root.join(&hex[..2]).join(format!("{hex}.jpg"))
}

/// Read the Exif orientation tag (1..=8) from raw file bytes; 1 when absent.
fn exif_orientation(bytes: &[u8]) -> u32 {
    let mut cursor = std::io::Cursor::new(bytes);
    let Ok(reader) = exif::Reader::new().read_from_container(&mut cursor) else {
        return 1;
    };
    reader
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .filter(|v| (1..=8).contains(v))
        .unwrap_or(1)
}

/// Apply an Exif orientation (1..=8) to a decoded image.
fn apply_exif_orientation(img: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn collect_cache_files(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cache_files(&path, out);
        } else if let Ok(meta) = entry.metadata() {
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            out.push((path, modified, meta.len()));
        }
    }
}

// ---------------------------------------------------------------------------
// Texture-side LRU (used by the renderer; plain data structure, no egui dep
// here so it unit-tests cheaply).
// ---------------------------------------------------------------------------

/// Count-capped LRU map for uploaded textures (or any per-key resource).
pub struct TextureLru<V> {
    cap: usize,
    order: VecDeque<ThumbKey>,
    map: HashMap<ThumbKey, V>,
}

impl<V> TextureLru<V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert, evicting least-recently-used entries beyond capacity.
    /// Returns evicted values (caller frees textures).
    pub fn insert(&mut self, key: ThumbKey, value: V) -> Vec<V> {
        if self.map.contains_key(&key) {
            self.touch(&key);
            self.map.insert(key, value);
            return Vec::new();
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
        let mut evicted = Vec::new();
        while self.map.len() > self.cap {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&old) {
                evicted.push(v);
            }
        }
        evicted
    }

    /// Get and mark as recently used.
    pub fn get(&mut self, key: &ThumbKey) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
        }
        self.map.get(key)
    }

    pub fn contains(&self, key: &ThumbKey) -> bool {
        self.map.contains_key(key)
    }

    pub fn clear(&mut self) -> Vec<V> {
        self.order.clear();
        self.map.drain().map(|(_, v)| v).collect()
    }

    fn touch(&mut self, key: &ThumbKey) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos).expect("position valid");
            self.order.push_back(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("facial-thumbs-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp dir");
        root
    }

    fn write_test_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        img.save(path).expect("write test png");
    }

    #[test]
    fn video_extension_detection_is_case_insensitive() {
        assert!(is_video_source("D:/clips/example.MP4"));
        assert!(is_video_source("example.mkv"));
        assert!(!is_video_source("example.jpg"));
        assert!(!is_video_source("example"));
    }

    #[test]
    fn video_worker_extracts_and_caches_a_real_frame_when_ffmpeg_is_available() {
        let Some(ffmpeg) = resolve_ffmpeg() else {
            return;
        };
        let ws = temp_dir("video-engine");
        let src = ws.join("clip.mp4");
        let mut make = Command::new(&ffmpeg);
        make.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=c=0xE4552F:s=640x360:d=2",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&src)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            make.creation_flags(0x0800_0000);
        }
        let status = make.status().expect("generate synthetic video fixture");
        assert!(status.success(), "ffmpeg generated the fixture");

        let src_str = src.to_string_lossy().to_string();
        let mut engine = ThumbnailEngine::new(&ws, Box::new(|| {}));
        assert!(engine.request(&src_str, 256, ThumbPriority::Visible));
        let mut got: Option<ThumbPixels> = None;
        for _ in 0..1000 {
            engine.drain_ready(8, |pixels| got = Some(pixels));
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let pixels = got.expect("video thumbnail produced");
        assert_eq!((pixels.width, pixels.height), (256, 144));
        let (_, _, failures, _) = engine.stats();
        assert_eq!(failures, 0);
        drop(engine);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cache_key_is_stable_and_input_sensitive() {
        let root = PathBuf::from("cache");
        let a = cache_path_for(&root, "D:/x/a.jpg", 100, 2000, 256);
        let b = cache_path_for(&root, "D:/x/a.jpg", 100, 2000, 256);
        assert_eq!(a, b, "same inputs -> same key");
        for other in [
            cache_path_for(&root, "D:/x/b.jpg", 100, 2000, 256),
            cache_path_for(&root, "D:/x/a.jpg", 101, 2000, 256),
            cache_path_for(&root, "D:/x/a.jpg", 100, 2001, 256),
            cache_path_for(&root, "D:/x/a.jpg", 100, 2000, 512),
        ] {
            assert_ne!(a, other, "any input change -> different key");
        }
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".jpg") && name.len() == 64 + 4, "{name}");
    }

    #[test]
    fn lru_evicts_least_recently_used_first() {
        let mut lru: TextureLru<u32> = TextureLru::new(2);
        let k = |n: u16| ThumbKey {
            path: format!("p{n}"),
            edge: n,
        };
        assert!(lru.insert(k(1), 1).is_empty());
        assert!(lru.insert(k(2), 2).is_empty());
        // Touch k1 so k2 becomes LRU.
        assert_eq!(lru.get(&k(1)), Some(&1));
        let evicted = lru.insert(k(3), 3);
        assert_eq!(evicted, vec![2], "k2 was least recently used");
        assert!(lru.contains(&k(1)) && lru.contains(&k(3)));
        assert_eq!(lru.len(), 2);
    }

    #[test]
    fn orientation_transforms_rotate_dimensions() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::new(40, 20));
        for (orientation, expect_w, expect_h) in
            [(1u32, 40, 20), (3, 40, 20), (6, 20, 40), (8, 20, 40)]
        {
            let out = apply_exif_orientation(img.clone(), orientation);
            assert_eq!(
                (out.width(), out.height()),
                (expect_w, expect_h),
                "orientation {orientation}"
            );
        }
    }

    #[test]
    fn engine_produces_thumbnail_and_hits_disk_cache_on_second_run() {
        let ws = temp_dir("engine");
        let src = ws.join("img.png");
        write_test_png(&src, 800, 600);
        let src_str = src.to_string_lossy().to_string();

        let mut engine = ThumbnailEngine::new(&ws, Box::new(|| {}));
        assert!(engine.request(&src_str, 256, ThumbPriority::Visible));
        let mut got: Option<ThumbPixels> = None;
        // Generous budget: the parallel test suite can starve worker threads.
        for _ in 0..1000 {
            engine.drain_ready(8, |pixels| got = Some(pixels));
            if got.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pixels = got.expect("thumbnail produced");
        assert!(pixels.width <= 256 && pixels.height <= 256);
        assert!(pixels.width == 256 || pixels.height == 256);
        let (decodes_1, disk_hits_1, _, _) = engine.stats();
        assert_eq!(decodes_1, 1);
        assert_eq!(disk_hits_1, 0);
        drop(engine);

        // Fresh engine (fresh process simulation): disk cache must hit.
        let mut engine2 = ThumbnailEngine::new(&ws, Box::new(|| {}));
        assert!(engine2.request(&src_str, 256, ThumbPriority::Visible));
        let mut got2 = false;
        for _ in 0..1000 {
            engine2.drain_ready(8, |_| got2 = true);
            if got2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(got2, "cached thumbnail produced");
        let (decodes_2, disk_hits_2, _, _) = engine2.stats();
        assert_eq!(decodes_2, 0, "no re-decode on disk hit");
        assert_eq!(disk_hits_2, 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn corrupt_file_fails_once_and_is_memoized() {
        let ws = temp_dir("corrupt");
        let src = ws.join("bad.jpg");
        std::fs::write(&src, b"this is not an image").unwrap();
        let src_str = src.to_string_lossy().to_string();

        let mut engine = ThumbnailEngine::new(&ws, Box::new(|| {}));
        assert!(engine.request(&src_str, 256, ThumbPriority::Visible));
        for _ in 0..1000 {
            engine.drain_ready(8, |_| {});
            if engine.failure(&src_str, 256).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(engine.failure(&src_str, 256).is_some(), "failure memoized");
        // Re-request must be refused (no retry storm).
        assert!(!engine.request(&src_str, 256, ThumbPriority::Visible));
        let (_, _, failures, _) = engine.stats();
        assert_eq!(failures, 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn gc_removes_beyond_cap_oldest_first() {
        let root = temp_dir("gc");
        let old = root.join("aa").join("old.jpg");
        let new = root.join("bb").join("new.jpg");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&old, vec![0u8; 600 * 1024]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&new, vec![0u8; 600 * 1024]).unwrap();
        // Cap 1MB: total is ~1.2MB, oldest must go.
        let (removed, _) = ThumbnailEngine::gc(&root, 1, 3650);
        assert_eq!(removed, 1);
        assert!(!old.exists(), "oldest entry removed");
        assert!(new.exists(), "newest entry kept");
        let _ = std::fs::remove_dir_all(&root);
    }
}
