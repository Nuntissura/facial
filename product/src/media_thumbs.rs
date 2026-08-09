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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::media_io::{
    IoRequest, MediaIoCoordinator, PermitOutcome, RootIdentity, RootKind, WorkClass,
};

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
    id: u64,
    key: ThumbKey,
    generation: u64,
    priority: ThumbPriority,
    queued_at: Instant,
}

struct PendingIo {
    generation: u64,
    request: IoRequest,
}

#[derive(Default)]
struct Queues {
    visible: VecDeque<Job>,
    prefetch: VecDeque<Job>,
    /// Video extraction is isolated behind one dedicated worker. A run of
    /// visible videos can therefore never occupy the image decoder pool.
    video_visible: VecDeque<Job>,
    video_prefetch: VecDeque<Job>,
    /// Explicit lifecycle per key. Dedupe must distinguish work that can still
    /// be promoted from a decode that is already touching the source.
    jobs: HashMap<ThumbKey, JobState>,
    shutdown: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobPhase {
    Queued,
    PermitWaiting,
    Decoding,
}

#[derive(Clone, Copy, Debug)]
struct JobState {
    id: u64,
    generation: u64,
    priority: ThumbPriority,
    video: bool,
    phase: JobPhase,
}

/// Counters surfaced through debug events (model observability).
#[derive(Default)]
pub struct ThumbStats {
    pub decodes: AtomicUsize,
    pub disk_hits: AtomicUsize,
    pub failures: AtomicUsize,
    pub stale_skips: AtomicUsize,
    pub queue_wait_total_us: AtomicU64,
    pub queue_wait_max_us: AtomicU64,
    pub filesystem_total_us: AtomicU64,
    pub filesystem_max_us: AtomicU64,
    pub video_extract_total_us: AtomicU64,
    pub video_extract_max_us: AtomicU64,
    pub prefetch_promotions: AtomicUsize,
    pub active_visible_reuses: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ThumbDiagnostics {
    pub decodes: usize,
    pub disk_hits: usize,
    pub failures: usize,
    pub stale_skips: usize,
    pub queue_wait_total_us: u64,
    pub queue_wait_max_us: u64,
    pub filesystem_total_us: u64,
    pub filesystem_max_us: u64,
    pub video_extract_total_us: u64,
    pub video_extract_max_us: u64,
    pub queued_visible: usize,
    pub queued_prefetch: usize,
    pub in_flight: usize,
    pub permit_waiting: usize,
    pub active_decodes: usize,
    pub coordinator_queued: usize,
    pub coordinator_active: usize,
    pub coordinator_cache_hits: u64,
    pub coordinator_filesystem_operations: u64,
    pub prefetch_promotions: usize,
    pub active_visible_reuses: usize,
}

#[derive(Clone, Copy)]
enum WorkerKind {
    ImageVisible,
    ImageGeneral,
    VideoVisible,
    VideoPrefetch,
}

struct EngineShared {
    queues: Mutex<Queues>,
    wake: Condvar,
    generation: AtomicU64,
    next_job_id: AtomicU64,
    pending_io: Mutex<HashMap<u64, PendingIo>>,
    cache_root: PathBuf,
    source_root: PathBuf,
    coordinator: Arc<MediaIoCoordinator>,
    root_identity: RootIdentity,
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
        let source_root = workspace_root.to_path_buf();
        let root_identity = RootIdentity::new(
            normalize_cache_path(workspace_root.to_string_lossy().as_ref()),
            0,
            RootKind::Unknown,
        );
        Self::new_with_cache_cap_and_io(
            workspace_root,
            cache_cap_mb,
            Arc::new(MediaIoCoordinator::new()),
            root_identity,
            &source_root,
            repaint,
        )
    }

    /// Spawn an engine sharing the application's root-aware I/O coordinator.
    /// `root_identity` must be resolved once by the caller for the configured
    /// media-root generation. `source_root` is its display spelling and is
    /// used only to derive a relative cache identity; no mapped-drive lookup
    /// occurs per thumbnail.
    pub fn new_with_io(
        workspace_root: &Path,
        coordinator: Arc<MediaIoCoordinator>,
        root_identity: RootIdentity,
        source_root: &Path,
        repaint: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::new_with_cache_cap_and_io(
            workspace_root,
            DEFAULT_CACHE_CAP_MB,
            coordinator,
            root_identity,
            source_root,
            repaint,
        )
    }

    /// Cache-cap variant of [`Self::new_with_io`].
    pub fn new_with_cache_cap_and_io(
        workspace_root: &Path,
        cache_cap_mb: u64,
        coordinator: Arc<MediaIoCoordinator>,
        root_identity: RootIdentity,
        source_root: &Path,
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
            next_job_id: AtomicU64::new(0),
            pending_io: Mutex::new(HashMap::new()),
            cache_root,
            source_root: source_root.to_path_buf(),
            coordinator,
            root_identity,
            stats: ThumbStats::default(),
            repaint,
        });
        let mut workers = Vec::new();
        for index in 0..worker_count {
            let shared = Arc::clone(&shared);
            let tx = results_tx.clone();
            // Keep one image worker exclusively available for visible work.
            // Old prefetch jobs may block in the OS and cannot be preempted.
            let kind = if index == 0 {
                WorkerKind::ImageVisible
            } else {
                WorkerKind::ImageGeneral
            };
            workers.push(
                thread::Builder::new()
                    .name(format!("thumb-worker-{index}"))
                    .spawn(move || worker_loop(shared, tx, kind))
                    .expect("spawn thumbnail worker"),
            );
        }
        {
            let shared = Arc::clone(&shared);
            let tx = results_tx.clone();
            workers.push(
                thread::Builder::new()
                    .name("video-thumb-visible-worker".to_string())
                    .spawn(move || worker_loop(shared, tx, WorkerKind::VideoVisible))
                    .expect("spawn video thumbnail worker"),
            );
        }
        {
            let shared = Arc::clone(&shared);
            let tx = results_tx.clone();
            workers.push(
                thread::Builder::new()
                    .name("video-thumb-prefetch-worker".to_string())
                    .spawn(move || worker_loop(shared, tx, WorkerKind::VideoPrefetch))
                    .expect("spawn video thumbnail prefetch worker"),
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
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        cancel_pending_io(&self.shared, |pending| pending.generation < generation);
        generation
    }

    /// Queue a thumbnail request. A visible request promotes the same key from
    /// queued/pending prefetch work instead of being rejected by dedupe. Once
    /// decoding has begun, ownership stays bounded to that one operation and
    /// this returns false; no duplicate source read/FFmpeg process is started.
    /// Memoized failures are not retried. Returns true for a new or promoted
    /// job and false for an unchanged duplicate/active reuse.
    pub fn request(&mut self, path: &str, edge: u16, priority: ThumbPriority) -> bool {
        let key = ThumbKey {
            path: path.to_string(),
            edge,
        };
        if self.failed.contains_key(&key) {
            return false;
        }
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let video = is_video_source(path);
        let mut queues = match self.shared.queues.lock() {
            Ok(q) => q,
            Err(_) => return false,
        };
        if let Some(existing) = queues.jobs.get(&key).copied() {
            match existing.phase {
                JobPhase::Queued => {
                    let should_promote = priority == ThumbPriority::Visible
                        && existing.priority == ThumbPriority::Prefetch;
                    let should_refresh = existing.generation < generation;
                    if should_promote || should_refresh {
                        if let Some(mut job) = remove_queued_job(&mut queues, &key, existing) {
                            job.generation = generation;
                            job.queued_at = Instant::now();
                            if should_promote {
                                job.priority = ThumbPriority::Visible;
                                self.shared
                                    .stats
                                    .prefetch_promotions
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            queues.jobs.insert(
                                key.clone(),
                                JobState {
                                    id: job.id,
                                    generation: job.generation,
                                    priority: job.priority,
                                    video: existing.video,
                                    phase: JobPhase::Queued,
                                },
                            );
                            push_queued_job(&mut queues, job, existing.video);
                            drop(queues);
                            self.shared.wake.notify_all();
                            return true;
                        }
                        // Defensive recovery from an impossible state/queue
                        // mismatch: purge any orphan before enqueueing once.
                        queues.jobs.remove(&key);
                        purge_queued_job(&mut queues, &key, existing.id);
                    } else {
                        return false;
                    }
                }
                JobPhase::PermitWaiting => {
                    if priority == ThumbPriority::Visible
                        && (existing.priority == ThumbPriority::Prefetch
                            || existing.generation < generation)
                    {
                        let promoted = existing.priority == ThumbPriority::Prefetch;
                        queues.jobs.insert(
                            key.clone(),
                            JobState {
                                priority: ThumbPriority::Visible,
                                generation,
                                ..existing
                            },
                        );
                        if promoted {
                            self.shared
                                .stats
                                .prefetch_promotions
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        drop(queues);
                        // If the prefetch request is already published, wake
                        // its worker by cancelling it. If publication races us,
                        // the worker's post-publish state check cancels it.
                        cancel_pending_job(&self.shared, existing.id);
                        self.shared.wake.notify_all();
                        return true;
                    }
                    return false;
                }
                JobPhase::Decoding => {
                    if priority == ThumbPriority::Visible {
                        self.shared
                            .stats
                            .active_visible_reuses
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    return false;
                }
            }
        }
        let id = self.shared.next_job_id.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            key: key.clone(),
            generation,
            priority,
            queued_at: Instant::now(),
        };
        queues.jobs.insert(
            key,
            JobState {
                id,
                generation,
                priority,
                video,
                phase: JobPhase::Queued,
            },
        );
        push_queued_job(&mut queues, job, video);
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

    pub fn diagnostics(&self) -> ThumbDiagnostics {
        let (queued_visible, queued_prefetch, in_flight, permit_waiting, active_decodes) = self
            .shared
            .queues
            .lock()
            .map(|queues| {
                (
                    queues.visible.len() + queues.video_visible.len(),
                    queues.prefetch.len() + queues.video_prefetch.len(),
                    queues.jobs.len(),
                    queues
                        .jobs
                        .values()
                        .filter(|state| state.phase == JobPhase::PermitWaiting)
                        .count(),
                    queues
                        .jobs
                        .values()
                        .filter(|state| state.phase == JobPhase::Decoding)
                        .count(),
                )
            })
            .unwrap_or_default();
        let coordinator = self.shared.coordinator.diagnostics();
        let mut coordinator_queued = 0usize;
        let mut coordinator_active = 0usize;
        let mut coordinator_cache_hits = 0u64;
        let mut coordinator_filesystem_operations = 0u64;
        if let Some(root) = coordinator
            .roots
            .iter()
            .find(|candidate| candidate.root == self.shared.root_identity)
        {
            for class in &root.classes {
                if matches!(class.class, WorkClass::Visible | WorkClass::Prefetch) {
                    coordinator_queued = coordinator_queued.saturating_add(class.queued);
                    coordinator_active = coordinator_active.saturating_add(class.active);
                    coordinator_cache_hits =
                        coordinator_cache_hits.saturating_add(class.cache_hits);
                    coordinator_filesystem_operations = coordinator_filesystem_operations
                        .saturating_add(class.filesystem_operations);
                }
            }
        }
        ThumbDiagnostics {
            decodes: self.shared.stats.decodes.load(Ordering::Relaxed),
            disk_hits: self.shared.stats.disk_hits.load(Ordering::Relaxed),
            failures: self.shared.stats.failures.load(Ordering::Relaxed),
            stale_skips: self.shared.stats.stale_skips.load(Ordering::Relaxed),
            queue_wait_total_us: self
                .shared
                .stats
                .queue_wait_total_us
                .load(Ordering::Relaxed),
            queue_wait_max_us: self.shared.stats.queue_wait_max_us.load(Ordering::Relaxed),
            filesystem_total_us: self
                .shared
                .stats
                .filesystem_total_us
                .load(Ordering::Relaxed),
            filesystem_max_us: self.shared.stats.filesystem_max_us.load(Ordering::Relaxed),
            video_extract_total_us: self
                .shared
                .stats
                .video_extract_total_us
                .load(Ordering::Relaxed),
            video_extract_max_us: self
                .shared
                .stats
                .video_extract_max_us
                .load(Ordering::Relaxed),
            queued_visible,
            queued_prefetch,
            in_flight,
            permit_waiting,
            active_decodes,
            coordinator_queued,
            coordinator_active,
            coordinator_cache_hits,
            coordinator_filesystem_operations,
            prefetch_promotions: self
                .shared
                .stats
                .prefetch_promotions
                .load(Ordering::Relaxed),
            active_visible_reuses: self
                .shared
                .stats
                .active_visible_reuses
                .load(Ordering::Relaxed),
        }
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
    /// Signal cancellation immediately, then move joins to a detached reaper.
    /// A blocked SMB/FFmpeg call may finish later, but replacing the engine can
    /// never freeze the UI thread waiting for that external call.
    fn drop(&mut self) {
        if let Ok(mut queues) = self.shared.queues.lock() {
            queues.shutdown = true;
            queues.visible.clear();
            queues.prefetch.clear();
            queues.video_visible.clear();
            queues.video_prefetch.clear();
        }
        cancel_pending_io(&self.shared, |_| true);
        self.shared.wake.notify_all();
        let workers = std::mem::take(&mut self.workers);
        if !workers.is_empty() {
            // If spawning the reaper itself fails, dropping the captured join
            // handles still detaches them; Drop remains nonblocking either way.
            let _ = thread::Builder::new()
                .name("thumb-worker-reaper".to_string())
                .spawn(move || {
                    for worker in workers {
                        let _ = worker.join();
                    }
                });
        }
    }
}

fn push_queued_job(queues: &mut Queues, job: Job, video: bool) {
    match (video, job.priority) {
        // FIFO within each band prevents starvation. A fast scroll bumps the
        // generation, so old viewport work is cancelled separately.
        (false, ThumbPriority::Visible) => queues.visible.push_back(job),
        (false, ThumbPriority::Prefetch) => queues.prefetch.push_back(job),
        (true, ThumbPriority::Visible) => queues.video_visible.push_back(job),
        (true, ThumbPriority::Prefetch) => queues.video_prefetch.push_back(job),
    }
}

fn remove_queued_job(queues: &mut Queues, key: &ThumbKey, state: JobState) -> Option<Job> {
    let queue = match (state.video, state.priority) {
        (false, ThumbPriority::Visible) => &mut queues.visible,
        (false, ThumbPriority::Prefetch) => &mut queues.prefetch,
        (true, ThumbPriority::Visible) => &mut queues.video_visible,
        (true, ThumbPriority::Prefetch) => &mut queues.video_prefetch,
    };
    queue
        .iter()
        .position(|job| job.id == state.id && &job.key == key)
        .and_then(|position| queue.remove(position))
}

fn purge_queued_job(queues: &mut Queues, key: &ThumbKey, id: u64) {
    for queue in [
        &mut queues.visible,
        &mut queues.prefetch,
        &mut queues.video_visible,
        &mut queues.video_prefetch,
    ] {
        queue.retain(|job| job.id != id || &job.key != key);
    }
}

fn worker_loop(shared: Arc<EngineShared>, tx: mpsc::Sender<ThumbOutcome>, kind: WorkerKind) {
    'worker: loop {
        let mut job = {
            let mut queues = match shared.queues.lock() {
                Ok(q) => q,
                Err(_) => return,
            };
            loop {
                if queues.shutdown {
                    return;
                }
                let job = match kind {
                    WorkerKind::ImageVisible => queues.visible.pop_front(),
                    WorkerKind::ImageGeneral => queues
                        .visible
                        .pop_front()
                        .or_else(|| queues.prefetch.pop_front()),
                    WorkerKind::VideoVisible => queues.video_visible.pop_front(),
                    WorkerKind::VideoPrefetch => queues.video_prefetch.pop_front(),
                };
                if let Some(mut job) = job {
                    let Some(state) = queues.jobs.get_mut(&job.key) else {
                        continue;
                    };
                    if state.id != job.id || state.phase != JobPhase::Queued {
                        continue;
                    }
                    // A visible request may have promoted/refreshed the queued
                    // state before this worker acquired the lock.
                    job.priority = state.priority;
                    job.generation = state.generation;
                    state.phase = JobPhase::PermitWaiting;
                    break job;
                }
                queues = match shared.wake.wait(queues) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
        };

        record_timing(
            &shared.stats.queue_wait_total_us,
            &shared.stats.queue_wait_max_us,
            job.queued_at.elapsed(),
        );

        // A visible request can promote a coordinator-waiting prefetch. The
        // worker cancels and re-enqueues at visible priority; at most one
        // worker ever owns this key.
        let (permit, class) = loop {
            let desired = {
                let mut queues = match shared.queues.lock() {
                    Ok(queues) => queues,
                    Err(_) => return,
                };
                let current = shared.generation.load(Ordering::SeqCst);
                let Some(state) = queues.jobs.get(&job.key).copied() else {
                    continue 'worker;
                };
                if state.id != job.id || state.phase != JobPhase::PermitWaiting {
                    continue 'worker;
                }
                if queues.shutdown || state.generation < current {
                    queues.jobs.remove(&job.key);
                    shared.stats.stale_skips.fetch_add(1, Ordering::Relaxed);
                    continue 'worker;
                }
                job.priority = state.priority;
                job.generation = state.generation;
                state
            };
            let class = work_class(desired.priority);
            let request = shared
                .coordinator
                .enqueue(shared.root_identity.clone(), class);
            if let Ok(mut pending) = shared.pending_io.lock() {
                pending.insert(
                    job.id,
                    PendingIo {
                        generation: desired.generation,
                        request: request.clone(),
                    },
                );
            }
            // Close the publication race with request-side promotion, a
            // generation bump, or engine shutdown.
            let still_desired = shared
                .queues
                .lock()
                .map(|queues| {
                    !queues.shutdown
                        && queues.jobs.get(&job.key).is_some_and(|state| {
                            state.id == job.id
                                && state.phase == JobPhase::PermitWaiting
                                && state.priority == desired.priority
                                && state.generation == desired.generation
                        })
                })
                .unwrap_or(false);
            if !still_desired {
                request.cancel();
            }
            let acquired = request.wait();
            if let Ok(mut pending) = shared.pending_io.lock() {
                pending.remove(&job.id);
            }
            match acquired {
                Ok(permit) => {
                    let mut queues = match shared.queues.lock() {
                        Ok(queues) => queues,
                        Err(_) => {
                            permit.finish(PermitOutcome::WorkerShutdown);
                            return;
                        }
                    };
                    let current = shared.generation.load(Ordering::SeqCst);
                    if queues.shutdown {
                        remove_job_state(&mut queues, &job.key, job.id);
                        permit.finish(PermitOutcome::WorkerShutdown);
                        continue 'worker;
                    }
                    let Some(state) = queues.jobs.get_mut(&job.key) else {
                        permit.finish(PermitOutcome::Cancelled);
                        continue 'worker;
                    };
                    if state.id != job.id || state.phase != JobPhase::PermitWaiting {
                        permit.finish(PermitOutcome::Cancelled);
                        continue 'worker;
                    }
                    if state.generation < current {
                        queues.jobs.remove(&job.key);
                        permit.finish(PermitOutcome::Stale);
                        shared.stats.stale_skips.fetch_add(1, Ordering::Relaxed);
                        continue 'worker;
                    }
                    if state.priority != desired.priority || state.generation != desired.generation
                    {
                        // Promotion won the race after this permit was granted.
                        permit.finish(PermitOutcome::Cancelled);
                        continue;
                    }
                    state.phase = JobPhase::Decoding;
                    break (permit, class);
                }
                Err(_) => {
                    let mut queues = match shared.queues.lock() {
                        Ok(queues) => queues,
                        Err(_) => return,
                    };
                    let current = shared.generation.load(Ordering::SeqCst);
                    let Some(state) = queues.jobs.get(&job.key).copied() else {
                        continue 'worker;
                    };
                    if queues.shutdown || state.generation < current {
                        queues.jobs.remove(&job.key);
                        shared.stats.stale_skips.fetch_add(1, Ordering::Relaxed);
                        continue 'worker;
                    }
                    if state.id == job.id && state.phase == JobPhase::PermitWaiting {
                        // A request-side promotion cancelled the prefetch
                        // permit. Loop and enqueue the same job as Visible.
                        continue;
                    }
                    queues.jobs.remove(&job.key);
                    continue 'worker;
                }
            }
        };

        let outcome = produce_thumbnail(&shared, &job.key, class);
        let shutdown = shared
            .queues
            .lock()
            .map(|queues| queues.shutdown)
            .unwrap_or(true);
        let stale = job.generation < shared.generation.load(Ordering::SeqCst);
        if shutdown || stale {
            permit.finish(if shutdown {
                PermitOutcome::WorkerShutdown
            } else {
                PermitOutcome::Stale
            });
            shared.stats.stale_skips.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut queues) = shared.queues.lock() {
                remove_job_state(&mut queues, &job.key, job.id);
            }
            continue;
        }
        permit.finish(match &outcome {
            ThumbOutcome::Ready(_) => PermitOutcome::Success,
            ThumbOutcome::Failed { .. } => PermitOutcome::Error,
        });
        if let Ok(mut queues) = shared.queues.lock() {
            remove_job_state(&mut queues, &job.key, job.id);
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

fn work_class(priority: ThumbPriority) -> WorkClass {
    match priority {
        ThumbPriority::Visible => WorkClass::Visible,
        ThumbPriority::Prefetch => WorkClass::Prefetch,
    }
}

fn remove_job_state(queues: &mut Queues, key: &ThumbKey, id: u64) {
    if queues.jobs.get(key).is_some_and(|state| state.id == id) {
        queues.jobs.remove(key);
    }
}

fn cancel_pending_job(shared: &EngineShared, id: u64) {
    let request = shared
        .pending_io
        .lock()
        .ok()
        .and_then(|pending| pending.get(&id).map(|entry| entry.request.clone()));
    if let Some(request) = request {
        request.cancel();
    }
}

fn cancel_pending_io(shared: &EngineShared, predicate: impl Fn(&PendingIo) -> bool) {
    let requests = shared
        .pending_io
        .lock()
        .map(|pending| {
            pending
                .values()
                .filter(|entry| predicate(entry))
                .map(|entry| entry.request.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for request in requests {
        request.cancel();
    }
}

/// Decode-or-load one thumbnail: disk cache first, else full decode + resize +
/// cache write. All failures become `Failed` outcomes (memoized by the UI).
fn produce_thumbnail(shared: &EngineShared, key: &ThumbKey, class: WorkClass) -> ThumbOutcome {
    let fs_started = Instant::now();
    let metadata_result = std::fs::metadata(&key.path);
    record_filesystem_timing(shared, class, fs_started.elapsed());
    let meta = match metadata_result {
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
    let cache_path = cache_path_for_root(
        &shared.cache_root,
        &shared.root_identity,
        &shared.source_root,
        &key.path,
        mtime,
        meta.len(),
        key.edge,
    );

    // Disk hit: cached thumbs are pre-rotated and pre-sized.
    let cache_read_started = Instant::now();
    let cached_bytes = std::fs::read(&cache_path);
    record_filesystem_timing(shared, class, cache_read_started.elapsed());
    if let Ok(bytes) = cached_bytes {
        if let Ok(img) = image::load_from_memory(&bytes) {
            shared.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
            shared
                .coordinator
                .record_cache_hit(&shared.root_identity, class);
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
        let extract_started = Instant::now();
        let outcome = produce_video_thumbnail(shared, key, &cache_path, class);
        record_timing(
            &shared.stats.video_extract_total_us,
            &shared.stats.video_extract_max_us,
            extract_started.elapsed(),
        );
        record_filesystem_timing(shared, class, extract_started.elapsed());
        return outcome;
    }
    let source_read_started = Instant::now();
    let source_bytes = std::fs::read(&key.path);
    record_filesystem_timing(shared, class, source_read_started.elapsed());
    let bytes = match source_bytes {
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
    // deduped by the job lifecycle map, but distinct keys can share a shard,
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

fn record_timing(total: &AtomicU64, maximum: &AtomicU64, elapsed: Duration) {
    let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    total.fetch_add(micros, Ordering::Relaxed);
    maximum.fetch_max(micros, Ordering::Relaxed);
}

fn record_filesystem_timing(shared: &EngineShared, class: WorkClass, elapsed: Duration) {
    record_timing(
        &shared.stats.filesystem_total_us,
        &shared.stats.filesystem_max_us,
        elapsed,
    );
    shared
        .coordinator
        .record_filesystem_duration(&shared.root_identity, class, elapsed);
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
    class: WorkClass,
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
    let attempt_timeout = if class == WorkClass::Visible {
        // Visible work owns a dedicated worker and cannot block image or
        // prefetch queues. Allow it to survive transient CPU contention from
        // scans/tests; speculative prefetch keeps the tighter cap.
        Duration::from_secs(15)
    } else {
        Duration::from_secs(5)
    };
    for seek in ["1.0", "0"] {
        let _ = std::fs::remove_file(&frame_path);
        match run_ffmpeg_frame(
            &ffmpeg,
            Path::new(&key.path),
            &frame_path,
            key.edge,
            seek,
            attempt_timeout,
        ) {
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
    timeout: Duration,
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
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("ffmpeg exited with {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "ffmpeg thumbnail extraction timed out after {} seconds",
                    timeout.as_secs()
                ));
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

/// Compatibility cache key for callers without a configured media-root
/// identity. New integration code should use [`cache_path_for_root`]. This
/// function is lexical and never performs mapped-drive/network resolution.
pub fn cache_path_for(
    cache_root: &Path,
    source_path: &str,
    mtime: u64,
    size: u64,
    edge: u16,
) -> PathBuf {
    let identity = RootIdentity::new("unscoped", 0, RootKind::Unknown);
    cache_path_for_root(
        cache_root,
        &identity,
        Path::new(""),
        source_path,
        mtime,
        size,
        edge,
    )
}

/// Root-scoped cache file path, sharded by the first two hex chars of
/// sha256(`root-identity|relative-or-display-path|mtime|size|edge`). The root
/// identity is resolved once by the caller, avoiding per-thumbnail WNet/SMB
/// identity lookups while still isolating remapped-root generations safely.
pub fn cache_path_for_root(
    cache_root: &Path,
    root_identity: &RootIdentity,
    source_root: &Path,
    source_path: &str,
    mtime: u64,
    size: u64,
    edge: u16,
) -> PathBuf {
    let path_identity = root_relative_cache_identity(source_root, source_path);
    let mut hasher = Sha256::new();
    hasher.update(normalize_cache_path(&root_identity.key).as_bytes());
    hasher.update(b"|");
    hasher.update(root_identity.generation.to_le_bytes());
    hasher.update(b"|");
    hasher.update([root_identity.kind as u8]);
    hasher.update(b"|");
    hasher.update(path_identity.as_bytes());
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

fn root_relative_cache_identity(source_root: &Path, source_path: &str) -> String {
    let source = normalize_cache_path(source_path);
    let root = normalize_cache_path(source_root.to_string_lossy().as_ref());
    let root = root.trim_end_matches('/');
    if !root.is_empty()
        && source.len() > root.len()
        && source.starts_with(root)
        && source.as_bytes().get(root.len()) == Some(&b'/')
    {
        return source[root.len() + 1..].to_string();
    }
    if source == root {
        return ".".to_string();
    }
    // A path outside the configured display root remains fully scoped by its
    // spelling. It cannot collide with another root because root_identity is
    // also part of the digest.
    source
}

fn normalize_cache_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
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
        let deadline = Instant::now() + Duration::from_secs(35);
        while Instant::now() < deadline {
            engine.drain_ready(8, |pixels| got = Some(pixels));
            if got.is_some() || engine.failure(&src_str, 256).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let failure = engine.failure(&src_str, 256).map(str::to_owned);
        let diagnostics = engine.diagnostics();
        let pixels = got.unwrap_or_else(|| {
            panic!("video thumbnail produced; failure={failure:?}; diagnostics={diagnostics:?}")
        });
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
    fn cache_key_keeps_unrelated_paths_distinct() {
        let root = PathBuf::from("cache");
        let a = cache_path_for(&root, r"\\server-a\share\same.mp4", 7, 99, 256);
        let b = cache_path_for(&root, r"\\server-b\share\same.mp4", 7, 99, 256);
        assert_ne!(a, b);
    }

    #[test]
    fn pre_resolved_root_identity_reuses_cache_across_display_aliases() {
        let root = PathBuf::from("cache");
        let identity = RootIdentity::new("//nas/video", 7, RootKind::Remote);
        let mapped = cache_path_for_root(
            &root,
            &identity,
            Path::new("Z:/Video"),
            "Z:/Video/set/clip.mp4",
            7,
            99,
            256,
        );
        let unc = cache_path_for_root(
            &root,
            &identity,
            Path::new("//nas/video"),
            "//nas/video/set/clip.mp4",
            7,
            99,
            256,
        );
        assert_eq!(mapped, unc);

        let remapped = RootIdentity::new("//other-nas/video", 8, RootKind::Remote);
        assert_ne!(
            mapped,
            cache_path_for_root(
                &root,
                &remapped,
                Path::new("Z:/Video"),
                "Z:/Video/set/clip.mp4",
                7,
                99,
                256,
            ),
            "a remapped generation/root must not inherit an unrelated cache entry"
        );
    }

    #[test]
    fn engine_drop_does_not_wait_for_a_coordinator_blocked_worker() {
        use crate::media_io::{IoPolicy, RootLimits};

        let ws = temp_dir("drop-nonblocking");
        let source = ws.join("waiting.png");
        write_test_png(&source, 32, 32);
        let limits = RootLimits {
            total: 1,
            interactive_reserved: 0,
            bulk_reserved: 0,
            playback_bulk_limit: 1,
        };
        let coordinator = Arc::new(MediaIoCoordinator::with_policy(IoPolicy {
            local: limits,
            remote: limits,
            unknown: limits,
            playback_hysteresis: Duration::from_millis(10),
            priority_burst: 2,
        }));
        let identity = RootIdentity::new("blocked", 1, RootKind::Remote);
        let blocker = coordinator.enqueue(identity.clone(), WorkClass::Visible);
        let blocker_permit = blocker.try_acquire().unwrap().unwrap();
        let mut engine = ThumbnailEngine::new_with_io(
            &ws,
            Arc::clone(&coordinator),
            identity.clone(),
            &ws,
            Box::new(|| {}),
        );
        assert!(engine.request(
            source.to_string_lossy().as_ref(),
            256,
            ThumbPriority::Visible
        ));
        let mut observed_queued = false;
        for _ in 0..100 {
            let queued = coordinator
                .diagnostics()
                .roots
                .iter()
                .find(|root| root.root == identity)
                .map(|root| root.classes.iter().map(|class| class.queued).sum::<usize>())
                .unwrap_or(0);
            if queued > 0 {
                observed_queued = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            observed_queued,
            "worker reached the blocked coordinator wait"
        );
        let started = Instant::now();
        drop(engine);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "engine replacement must not join blocked workers on the caller"
        );
        drop(blocker_permit);
        thread::sleep(Duration::from_millis(20));
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn assert_prefetch_request_promotes_to_visible(extension: &str) {
        use crate::media_io::{ClassDiagnostics, IoPolicy, RootLimits};

        fn class_diagnostics(
            coordinator: &MediaIoCoordinator,
            identity: &RootIdentity,
            class: WorkClass,
        ) -> Option<ClassDiagnostics> {
            coordinator
                .diagnostics()
                .roots
                .into_iter()
                .find(|root| &root.root == identity)
                .and_then(|root| {
                    root.classes
                        .into_iter()
                        .find(|candidate| candidate.class == class)
                })
        }

        let ws = temp_dir(&format!("promote-{extension}"));
        let source = ws.join(format!("asset.{extension}"));
        std::fs::write(&source, b"fixture bytes").unwrap();
        let limits = RootLimits {
            total: 1,
            interactive_reserved: 0,
            bulk_reserved: 0,
            playback_bulk_limit: 1,
        };
        let coordinator = Arc::new(MediaIoCoordinator::with_policy(IoPolicy {
            local: limits,
            remote: limits,
            unknown: limits,
            playback_hysteresis: Duration::from_millis(10),
            priority_burst: 2,
        }));
        let identity = RootIdentity::new(format!("promote-{extension}"), 1, RootKind::Remote);
        let blocker = coordinator.enqueue(identity.clone(), WorkClass::Playback);
        let blocker_permit = blocker.try_acquire().unwrap().unwrap();
        let mut engine = ThumbnailEngine::new_with_io(
            &ws,
            Arc::clone(&coordinator),
            identity.clone(),
            &ws,
            Box::new(|| {}),
        );
        let source = source.to_string_lossy().to_string();
        assert!(engine.request(&source, 256, ThumbPriority::Prefetch));

        let mut prefetch_waiting = false;
        for _ in 0..500 {
            prefetch_waiting = class_diagnostics(&coordinator, &identity, WorkClass::Prefetch)
                .is_some_and(|diagnostics| diagnostics.queued == 1);
            if prefetch_waiting {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            prefetch_waiting,
            "{extension} prefetch reached coordinator wait"
        );

        assert!(
            engine.request(&source, 256, ThumbPriority::Visible),
            "visible request promotes worker-owned prefetch"
        );
        let mut visible_waiting = false;
        for _ in 0..500 {
            let visible = class_diagnostics(&coordinator, &identity, WorkClass::Visible);
            let prefetch = class_diagnostics(&coordinator, &identity, WorkClass::Prefetch);
            visible_waiting = visible.as_ref().is_some_and(|item| item.queued == 1)
                && prefetch.as_ref().is_some_and(|item| {
                    item.queued == 0 && item.cancelled == 1 && item.enqueued == 1
                });
            if visible_waiting {
                assert_eq!(visible.unwrap().enqueued, 1, "one visible replacement");
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            visible_waiting,
            "{extension} prefetch was cancelled and re-enqueued as visible"
        );
        assert_eq!(engine.diagnostics().prefetch_promotions, 1);

        drop(engine);
        drop(blocker_permit);
        thread::sleep(Duration::from_millis(20));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn image_prefetch_is_promoted_to_visible_coordinator_work() {
        assert_prefetch_request_promotes_to_visible("jpg");
    }

    #[test]
    fn video_prefetch_is_promoted_to_visible_coordinator_work() {
        assert_prefetch_request_promotes_to_visible("mp4");
    }

    #[test]
    fn active_decode_is_reused_without_duplicate_visible_job() {
        let ws = temp_dir("active-reuse");
        let source = ws.join("active.jpg").to_string_lossy().to_string();
        let mut engine = ThumbnailEngine::new(&ws, Box::new(|| {}));
        let key = ThumbKey {
            path: source.clone(),
            edge: 256,
        };
        engine.shared.queues.lock().unwrap().jobs.insert(
            key.clone(),
            JobState {
                id: 91,
                generation: 0,
                priority: ThumbPriority::Prefetch,
                video: false,
                phase: JobPhase::Decoding,
            },
        );

        assert!(!engine.request(&source, 256, ThumbPriority::Visible));
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.active_decodes, 1);
        assert_eq!(diagnostics.active_visible_reuses, 1);
        assert_eq!(diagnostics.queued_visible, 0);
        assert_eq!(diagnostics.in_flight, 1);

        engine.shared.queues.lock().unwrap().jobs.remove(&key);
        drop(engine);
        let _ = std::fs::remove_dir_all(&ws);
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
        let diagnostics_1 = engine.diagnostics();
        assert!(diagnostics_1.queue_wait_total_us > 0);
        assert!(diagnostics_1.filesystem_total_us > 0);
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
        let diagnostics_2 = engine2.diagnostics();
        assert!(diagnostics_2.queue_wait_total_us > 0);
        assert!(diagnostics_2.filesystem_total_us > 0);
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
