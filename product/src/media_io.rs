//! Root-aware arbitration for media filesystem work.
//!
//! Enqueuing a request never waits for filesystem capacity. A worker may then
//! call [`IoRequest::wait`] in the background, or poll with
//! [`IoRequest::try_acquire`]. Requests, permits, and playback leases are RAII
//! objects: dropping any of them returns all coordinator state it owns.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const REQUEST_PENDING: u8 = 0;
const REQUEST_GRANTED: u8 = 1;
const REQUEST_CONSUMED: u8 = 2;
const REQUEST_CANCELLED: u8 = 3;

const OUTCOME_COUNT: usize = 6;

/// A configured media root. The generation deliberately participates in the
/// identity, so a remapped drive cannot inherit permits from its predecessor.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize)]
pub struct RootIdentity {
    pub key: String,
    pub generation: u64,
    pub kind: RootKind,
}

impl RootIdentity {
    pub fn new(key: impl Into<String>, generation: u64, kind: RootKind) -> Self {
        Self {
            key: key.into(),
            generation,
            kind,
        }
    }
}

/// Remote and unknown roots use conservative limits. Unknown is intentionally
/// not treated as local: uncertainty must not fan out network I/O.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Local,
    Remote,
    Unknown,
}

/// Work classes are ordered from most latency-sensitive to least. Fairness is
/// applied across these priorities after a bounded run of higher-priority work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum WorkClass {
    Playback = 0,
    Visible = 1,
    /// Filesystem work a visible row is waiting on. Currently unused: the
    /// per-row hydration it was introduced for is served from the in-memory
    /// cache and never touches the filesystem. Retained as the reserved slot
    /// between visible thumbnails and enumeration so a future row-blocking read
    /// has a class that outranks `Scan` without being promoted to `Visible`.
    Metadata = 2,
    Scan = 3,
    Prefetch = 4,
    /// Whole-folder sweeps that no visible row is waiting on: the stat sweep
    /// behind a size/date sort and the semantic index query.
    ///
    /// These ran as `Metadata` (WP-069), which outranks `Prefetch` — so opening
    /// a large folder with a size sort let a 141k-file stat sweep take permits
    /// ahead of overscan thumbnails. Thumbnails are the reason the app exists
    /// for large folders, so nothing a row is not waiting on may outrank them.
    Background = 5,
}

impl WorkClass {
    pub const ALL: [Self; 6] = [
        Self::Playback,
        Self::Visible,
        Self::Metadata,
        Self::Scan,
        Self::Prefetch,
        Self::Background,
    ];

    const COUNT: usize = Self::ALL.len();

    fn index(self) -> usize {
        self as usize
    }

    fn is_interactive(self) -> bool {
        matches!(self, Self::Playback | Self::Visible)
    }

    fn priority(self) -> u8 {
        self as u8
    }
}

/// Per-root concurrency limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RootLimits {
    pub total: usize,
    /// Capacity bulk work must leave available while interactive work queues.
    pub interactive_reserved: usize,
    /// Capacity interactive work must leave available while bulk work queues.
    pub bulk_reserved: usize,
    /// Maximum concurrent bulk requests while playback is active or hysteresis
    /// is still in effect. At least one is retained so reconciliation advances.
    pub playback_bulk_limit: usize,
}

impl RootLimits {
    fn sanitized(self) -> Self {
        let total = self.total.max(1);
        let maximum_reservation = total.saturating_sub(1);
        Self {
            total,
            interactive_reserved: self.interactive_reserved.min(maximum_reservation),
            bulk_reserved: self.bulk_reserved.min(maximum_reservation),
            playback_bulk_limit: self.playback_bulk_limit.clamp(1, total),
        }
    }
}

/// Coordinator policy. Defaults are deliberately independent of logical CPU
/// count so a large workstation cannot accidentally flood an SMB root.
#[derive(Clone, Debug)]
pub struct IoPolicy {
    pub local: RootLimits,
    pub remote: RootLimits,
    pub unknown: RootLimits,
    pub playback_hysteresis: Duration,
    /// Number of consecutive grants at one priority before an older lower-
    /// priority request gets a turn.
    pub priority_burst: u32,
}

impl Default for IoPolicy {
    fn default() -> Self {
        Self {
            local: RootLimits {
                total: 12,
                interactive_reserved: 2,
                bulk_reserved: 1,
                playback_bulk_limit: 2,
            },
            remote: RootLimits {
                total: 4,
                interactive_reserved: 1,
                bulk_reserved: 1,
                playback_bulk_limit: 1,
            },
            unknown: RootLimits {
                total: 2,
                interactive_reserved: 1,
                bulk_reserved: 1,
                playback_bulk_limit: 1,
            },
            playback_hysteresis: Duration::from_millis(750),
            priority_burst: 4,
        }
    }
}

impl IoPolicy {
    fn sanitized(mut self) -> Self {
        self.local = self.local.sanitized();
        self.remote = self.remote.sanitized();
        self.unknown = self.unknown.sanitized();
        self.priority_burst = self.priority_burst.max(1);
        self
    }

    fn limits(&self, kind: RootKind) -> RootLimits {
        match kind {
            RootKind::Local => self.local,
            RootKind::Remote => self.remote,
            RootKind::Unknown => self.unknown,
        }
    }
}

/// Monotonic time source. Production uses [`SystemClock`]; tests can advance a
/// [`ManualClock`] without sleeping.
pub trait CoordinatorClock: Send + Sync + 'static {
    fn now_micros(&self) -> u64;
}

pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl CoordinatorClock for SystemClock {
    fn now_micros(&self) -> u64 {
        duration_micros_saturating(self.origin.elapsed())
    }
}

/// Deterministic monotonic clock for coordinator tests and diagnostic probes.
pub struct ManualClock {
    micros: AtomicU64,
}

impl ManualClock {
    pub fn new(initial: Duration) -> Self {
        Self {
            micros: AtomicU64::new(duration_micros_saturating(initial)),
        }
    }

    pub fn advance(&self, amount: Duration) {
        self.micros
            .fetch_add(duration_micros_saturating(amount), Ordering::SeqCst);
    }
}

impl CoordinatorClock for ManualClock {
    fn now_micros(&self) -> u64 {
        self.micros.load(Ordering::SeqCst)
    }
}

fn duration_micros_saturating(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct MediaIoCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    state: Mutex<CoordinatorState>,
    changed: Condvar,
    policy: IoPolicy,
    clock: Arc<dyn CoordinatorClock>,
}

#[derive(Default)]
struct CoordinatorState {
    next_request_id: u64,
    next_sequence: u64,
    roots: HashMap<RootIdentity, RootState>,
}

struct RootState {
    queue: VecDeque<QueueEntry>,
    active: [usize; WorkClass::COUNT],
    stats: [ClassStats; WorkClass::COUNT],
    playback_leases: usize,
    playback_hysteresis_until_us: u64,
    last_priority: Option<u8>,
    priority_streak: u32,
}

impl Default for RootState {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            active: [0; WorkClass::COUNT],
            stats: std::array::from_fn(|_| ClassStats::default()),
            playback_leases: 0,
            playback_hysteresis_until_us: 0,
            last_priority: None,
            priority_streak: 0,
        }
    }
}

#[derive(Default)]
struct ClassStats {
    enqueued: u64,
    granted: u64,
    cancelled: u64,
    released: u64,
    max_queued: usize,
    queue_wait_total_us: u64,
    queue_wait_max_us: u64,
    cache_hits: u64,
    filesystem_operations: u64,
    filesystem_total_us: u64,
    filesystem_max_us: u64,
    release_outcomes: [u64; OUTCOME_COUNT],
}

struct QueueEntry {
    id: u64,
    sequence: u64,
    class: WorkClass,
    enqueued_us: u64,
    request: Weak<RequestCore>,
}

impl MediaIoCoordinator {
    pub fn new() -> Self {
        Self::with_policy(IoPolicy::default())
    }

    pub fn with_policy(policy: IoPolicy) -> Self {
        Self::with_clock(policy, Arc::new(SystemClock::default()))
    }

    pub fn with_clock(policy: IoPolicy, clock: Arc<dyn CoordinatorClock>) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                state: Mutex::new(CoordinatorState::default()),
                changed: Condvar::new(),
                policy: policy.sanitized(),
                clock,
            }),
        }
    }

    /// Enqueue without waiting for capacity. The returned request can be moved
    /// to a background worker and waited there.
    pub fn enqueue(&self, root: RootIdentity, class: WorkClass) -> IoRequest {
        let now = self.inner.clock.now_micros();
        let mut state = lock_unpoisoned(&self.inner.state);
        let id = state.next_request_id;
        state.next_request_id = state.next_request_id.wrapping_add(1);
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);

        let core = Arc::new(RequestCore {
            coordinator: Arc::downgrade(&self.inner),
            id,
            root: root.clone(),
            class,
            status: AtomicU8::new(REQUEST_PENDING),
        });

        let root_state = state.roots.entry(root.clone()).or_default();
        let class_stats = &mut root_state.stats[class.index()];
        class_stats.enqueued = class_stats.enqueued.saturating_add(1);
        root_state.queue.push_back(QueueEntry {
            id,
            sequence,
            class,
            enqueued_us: now,
            request: Arc::downgrade(&core),
        });
        let queued = root_state
            .queue
            .iter()
            .filter(|entry| entry.class == class)
            .count();
        class_stats.max_queued = class_stats.max_queued.max(queued);

        schedule_root(&self.inner.policy, &root, root_state, now);
        drop(state);
        self.inner.changed.notify_all();
        IoRequest { core }
    }

    /// Mark playback active for a root. The final lease starts the hysteresis
    /// window when dropped, avoiding rapid throttle oscillation during seeks or
    /// brief player state transitions.
    pub fn begin_playback(&self, root: RootIdentity) -> PlaybackLease {
        let now = self.inner.clock.now_micros();
        let mut state = lock_unpoisoned(&self.inner.state);
        let root_state = state.roots.entry(root.clone()).or_default();
        root_state.playback_leases = root_state.playback_leases.saturating_add(1);
        root_state.playback_hysteresis_until_us = 0;
        schedule_root(&self.inner.policy, &root, root_state, now);
        drop(state);
        self.inner.changed.notify_all();
        PlaybackLease {
            coordinator: Arc::downgrade(&self.inner),
            root,
            active: true,
        }
    }

    /// Re-evaluate time-based policy. Production waiters also wake when their
    /// hysteresis deadline expires; this method lets a manual clock advance
    /// without sleeping.
    pub fn refresh(&self) {
        let now = self.inner.clock.now_micros();
        let mut state = lock_unpoisoned(&self.inner.state);
        for (root, root_state) in state.roots.iter_mut() {
            schedule_root(&self.inner.policy, root, root_state, now);
        }
        drop(state);
        self.inner.changed.notify_all();
    }

    /// Record a cache hit against the work class that avoided filesystem I/O.
    pub fn record_cache_hit(&self, root: &RootIdentity, class: WorkClass) {
        let mut state = lock_unpoisoned(&self.inner.state);
        let stats = &mut state.roots.entry(root.clone()).or_default().stats[class.index()];
        stats.cache_hits = stats.cache_hits.saturating_add(1);
    }

    /// Attribute a completed filesystem operation to a root and work class.
    pub fn record_filesystem_duration(
        &self,
        root: &RootIdentity,
        class: WorkClass,
        duration: Duration,
    ) {
        let micros = duration_micros_saturating(duration);
        let mut state = lock_unpoisoned(&self.inner.state);
        let stats = &mut state.roots.entry(root.clone()).or_default().stats[class.index()];
        stats.filesystem_operations = stats.filesystem_operations.saturating_add(1);
        stats.filesystem_total_us = stats.filesystem_total_us.saturating_add(micros);
        stats.filesystem_max_us = stats.filesystem_max_us.max(micros);
    }

    pub fn diagnostics(&self) -> IoDiagnostics {
        let now = self.inner.clock.now_micros();
        let state = lock_unpoisoned(&self.inner.state);
        let mut roots = Vec::with_capacity(state.roots.len());
        for (root, root_state) in &state.roots {
            let limits = self.inner.policy.limits(root.kind);
            let playback_throttled = is_playback_throttled(root_state, now);
            let classes = WorkClass::ALL
                .iter()
                .copied()
                .map(|class| {
                    let stats = &root_state.stats[class.index()];
                    ClassDiagnostics {
                        class,
                        queued: root_state
                            .queue
                            .iter()
                            .filter(|entry| entry.class == class)
                            .count(),
                        active: root_state.active[class.index()],
                        enqueued: stats.enqueued,
                        granted: stats.granted,
                        cancelled: stats.cancelled,
                        released: stats.released,
                        max_queued: stats.max_queued,
                        queue_wait_total_us: stats.queue_wait_total_us,
                        queue_wait_max_us: stats.queue_wait_max_us,
                        cache_hits: stats.cache_hits,
                        filesystem_operations: stats.filesystem_operations,
                        filesystem_total_us: stats.filesystem_total_us,
                        filesystem_max_us: stats.filesystem_max_us,
                        release_outcomes: ReleaseOutcomeDiagnostics::from_counts(
                            stats.release_outcomes,
                        ),
                    }
                })
                .collect();
            roots.push(RootDiagnostics {
                root: root.clone(),
                limits,
                playback_leases: root_state.playback_leases,
                playback_throttled,
                hysteresis_remaining_us: root_state
                    .playback_hysteresis_until_us
                    .saturating_sub(now),
                effective_bulk_limit: limits
                    .total
                    .saturating_sub(limits.interactive_reserved)
                    .min(if playback_throttled {
                        limits.playback_bulk_limit
                    } else {
                        limits.total
                    }),
                classes,
            });
        }
        roots.sort_by(|left, right| {
            left.root
                .key
                .cmp(&right.root.key)
                .then(left.root.generation.cmp(&right.root.generation))
                .then((left.root.kind as u8).cmp(&(right.root.kind as u8)))
        });
        IoDiagnostics { roots }
    }
}

impl Default for MediaIoCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// A queued request. Clones refer to the same request; explicit cancellation by
/// any clone cancels it for all waiters. Dropping the final clone also cancels.
#[derive(Clone)]
pub struct IoRequest {
    core: Arc<RequestCore>,
}

struct RequestCore {
    coordinator: Weak<CoordinatorInner>,
    id: u64,
    root: RootIdentity,
    class: WorkClass,
    status: AtomicU8,
}

impl Drop for RequestCore {
    fn drop(&mut self) {
        if let Some(coordinator) = self.coordinator.upgrade() {
            cancel_request(&coordinator, self, false);
        }
    }
}

impl IoRequest {
    pub fn root(&self) -> &RootIdentity {
        &self.core.root
    }

    pub fn class(&self) -> WorkClass {
        self.core.class
    }

    /// Cancel whether the request is queued or granted-but-not-consumed.
    pub fn cancel(&self) -> bool {
        let Some(coordinator) = self.core.coordinator.upgrade() else {
            return false;
        };
        cancel_request(&coordinator, &self.core, true)
    }

    /// Poll without blocking. `Ok(None)` means the request remains queued.
    pub fn try_acquire(&self) -> Result<Option<IoPermit>, WaitError> {
        let Some(coordinator) = self.core.coordinator.upgrade() else {
            return Err(WaitError::CoordinatorDropped);
        };
        let mut state = lock_unpoisoned(&coordinator.state);
        consume_if_granted(&coordinator, &mut state, &self.core)
            .map(Some)
            .or_else(|error| {
                if error == WaitError::Pending {
                    Ok(None)
                } else {
                    Err(error)
                }
            })
    }

    /// Wait for a permit. This is intended for a background worker, never the UI
    /// thread. Cancellation and coordinator shutdown wake the wait immediately.
    pub fn wait(&self) -> Result<IoPermit, WaitError> {
        let Some(coordinator) = self.core.coordinator.upgrade() else {
            return Err(WaitError::CoordinatorDropped);
        };
        let mut state = lock_unpoisoned(&coordinator.state);
        loop {
            match consume_if_granted(&coordinator, &mut state, &self.core) {
                Ok(permit) => return Ok(permit),
                Err(WaitError::Pending) => {
                    let now = coordinator.clock.now_micros();
                    let deadline = state
                        .roots
                        .get(&self.core.root)
                        .map(|root| root.playback_hysteresis_until_us)
                        .unwrap_or(0);
                    if deadline > now {
                        let timeout = Duration::from_micros(deadline - now);
                        let waited = coordinator.changed.wait_timeout(state, timeout);
                        let (next_state, _) =
                            waited.unwrap_or_else(|poisoned| poisoned.into_inner());
                        state = next_state;
                        let refreshed_now = coordinator.clock.now_micros();
                        if let Some(root_state) = state.roots.get_mut(&self.core.root) {
                            schedule_root(
                                &coordinator.policy,
                                &self.core.root,
                                root_state,
                                refreshed_now,
                            );
                        }
                    } else {
                        state = coordinator
                            .changed
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    Pending,
    Cancelled,
    AlreadyAcquired,
    CoordinatorDropped,
}

impl fmt::Display for WaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "I/O request is still pending",
            Self::Cancelled => "I/O request was cancelled",
            Self::AlreadyAcquired => "I/O request permit was already acquired",
            Self::CoordinatorDropped => "I/O coordinator was dropped",
        })
    }
}

impl std::error::Error for WaitError {}

fn consume_if_granted(
    coordinator: &Arc<CoordinatorInner>,
    _state: &mut CoordinatorState,
    request: &RequestCore,
) -> Result<IoPermit, WaitError> {
    match request.status.load(Ordering::Acquire) {
        REQUEST_PENDING => Err(WaitError::Pending),
        REQUEST_CANCELLED => Err(WaitError::Cancelled),
        REQUEST_CONSUMED => Err(WaitError::AlreadyAcquired),
        REQUEST_GRANTED => {
            request.status.store(REQUEST_CONSUMED, Ordering::Release);
            Ok(IoPermit {
                coordinator: Arc::downgrade(coordinator),
                root: request.root.clone(),
                class: request.class,
                released: false,
            })
        }
        _ => Err(WaitError::Cancelled),
    }
}

fn cancel_request(
    coordinator: &Arc<CoordinatorInner>,
    request: &RequestCore,
    notify: bool,
) -> bool {
    let now = coordinator.clock.now_micros();
    let mut state = lock_unpoisoned(&coordinator.state);
    let status = request.status.load(Ordering::Acquire);
    let Some(root_state) = state.roots.get_mut(&request.root) else {
        return false;
    };
    let cancelled = match status {
        REQUEST_PENDING => {
            if let Some(index) = root_state
                .queue
                .iter()
                .position(|entry| entry.id == request.id)
            {
                root_state.queue.remove(index);
                request.status.store(REQUEST_CANCELLED, Ordering::Release);
                let stats = &mut root_state.stats[request.class.index()];
                stats.cancelled = stats.cancelled.saturating_add(1);
                stats.release_outcomes[PermitOutcome::Cancelled.index()] =
                    stats.release_outcomes[PermitOutcome::Cancelled.index()].saturating_add(1);
                true
            } else {
                false
            }
        }
        REQUEST_GRANTED => {
            request.status.store(REQUEST_CANCELLED, Ordering::Release);
            release_active(root_state, request.class, PermitOutcome::Cancelled);
            root_state.stats[request.class.index()].cancelled = root_state.stats
                [request.class.index()]
            .cancelled
            .saturating_add(1);
            true
        }
        _ => false,
    };
    if cancelled {
        schedule_root(&coordinator.policy, &request.root, root_state, now);
    }
    drop(state);
    if cancelled || notify {
        coordinator.changed.notify_all();
    }
    cancelled
}

/// Capacity ownership returned to a worker. Dropping without an explicit
/// outcome is still safe and is reported as [`PermitOutcome::Dropped`].
pub struct IoPermit {
    coordinator: Weak<CoordinatorInner>,
    root: RootIdentity,
    class: WorkClass,
    released: bool,
}

impl IoPermit {
    pub fn root(&self) -> &RootIdentity {
        &self.root
    }

    pub fn class(&self) -> WorkClass {
        self.class
    }

    pub fn finish(mut self, outcome: PermitOutcome) {
        self.release(outcome);
    }

    fn release(&mut self, outcome: PermitOutcome) {
        if self.released {
            return;
        }
        self.released = true;
        let Some(coordinator) = self.coordinator.upgrade() else {
            return;
        };
        let now = coordinator.clock.now_micros();
        let mut state = lock_unpoisoned(&coordinator.state);
        if let Some(root_state) = state.roots.get_mut(&self.root) {
            release_active(root_state, self.class, outcome);
            schedule_root(&coordinator.policy, &self.root, root_state, now);
        }
        drop(state);
        coordinator.changed.notify_all();
    }
}

impl Drop for IoPermit {
    fn drop(&mut self) {
        self.release(PermitOutcome::Dropped);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PermitOutcome {
    Success = 0,
    Error = 1,
    Cancelled = 2,
    Stale = 3,
    WorkerShutdown = 4,
    Dropped = 5,
}

impl PermitOutcome {
    fn index(self) -> usize {
        self as usize
    }
}

fn release_active(root_state: &mut RootState, class: WorkClass, outcome: PermitOutcome) {
    root_state.active[class.index()] = root_state.active[class.index()].saturating_sub(1);
    let stats = &mut root_state.stats[class.index()];
    stats.released = stats.released.saturating_add(1);
    stats.release_outcomes[outcome.index()] =
        stats.release_outcomes[outcome.index()].saturating_add(1);
}

/// Playback ownership. Multiple player surfaces may overlap; throttling ends
/// only after the final lease, followed by the configured hysteresis.
pub struct PlaybackLease {
    coordinator: Weak<CoordinatorInner>,
    root: RootIdentity,
    active: bool,
}

impl PlaybackLease {
    pub fn finish(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let Some(coordinator) = self.coordinator.upgrade() else {
            return;
        };
        let now = coordinator.clock.now_micros();
        let hysteresis = duration_micros_saturating(coordinator.policy.playback_hysteresis);
        let mut state = lock_unpoisoned(&coordinator.state);
        if let Some(root_state) = state.roots.get_mut(&self.root) {
            root_state.playback_leases = root_state.playback_leases.saturating_sub(1);
            if root_state.playback_leases == 0 {
                root_state.playback_hysteresis_until_us = now.saturating_add(hysteresis);
            }
            schedule_root(&coordinator.policy, &self.root, root_state, now);
        }
        drop(state);
        coordinator.changed.notify_all();
    }
}

impl Drop for PlaybackLease {
    fn drop(&mut self) {
        self.release();
    }
}

fn is_playback_throttled(root_state: &RootState, now: u64) -> bool {
    root_state.playback_leases > 0 || now < root_state.playback_hysteresis_until_us
}

fn schedule_root(policy: &IoPolicy, identity: &RootIdentity, root: &mut RootState, now: u64) {
    // A dead weak entry means its final request handle disappeared before its
    // Drop handler could observe the queue. Clean it defensively.
    root.queue.retain(|entry| entry.request.strong_count() > 0);
    let limits = policy.limits(identity.kind);

    while root.active.iter().sum::<usize>() < limits.total {
        let active_interactive = WorkClass::ALL
            .iter()
            .filter(|class| class.is_interactive())
            .map(|class| root.active[class.index()])
            .sum::<usize>();
        let active_bulk = root.active.iter().sum::<usize>() - active_interactive;
        let effective_bulk_limit = limits
            .total
            .saturating_sub(limits.interactive_reserved)
            .min(if is_playback_throttled(root, now) {
                limits.playback_bulk_limit
            } else {
                limits.total
            });
        let interactive_limit = limits.total.saturating_sub(limits.bulk_reserved);

        let mut eligible = Vec::new();
        for (index, entry) in root.queue.iter().enumerate() {
            let admitted = if entry.class.is_interactive() {
                active_interactive < interactive_limit
            } else {
                active_bulk < effective_bulk_limit
            };
            if admitted {
                eligible.push(index);
            }
        }
        if eligible.is_empty() {
            break;
        }

        let highest_priority = eligible
            .iter()
            .map(|index| root.queue[*index].class.priority())
            .min()
            .unwrap_or(u8::MAX);
        let force_fair_turn = root.last_priority == Some(highest_priority)
            && root.priority_streak >= policy.priority_burst;
        let selected = if force_fair_turn {
            eligible
                .iter()
                .copied()
                .filter(|index| root.queue[*index].class.priority() > highest_priority)
                .min_by_key(|index| root.queue[*index].sequence)
        } else {
            None
        }
        .or_else(|| {
            eligible
                .iter()
                .copied()
                .filter(|index| root.queue[*index].class.priority() == highest_priority)
                .min_by_key(|index| root.queue[*index].sequence)
        });
        let Some(index) = selected else {
            break;
        };
        let entry = root
            .queue
            .remove(index)
            .expect("selected queue entry exists");
        let Some(request) = entry.request.upgrade() else {
            continue;
        };
        if request.status.load(Ordering::Acquire) != REQUEST_PENDING {
            continue;
        }
        request.status.store(REQUEST_GRANTED, Ordering::Release);
        root.active[entry.class.index()] = root.active[entry.class.index()].saturating_add(1);
        let stats = &mut root.stats[entry.class.index()];
        stats.granted = stats.granted.saturating_add(1);
        let waited = now.saturating_sub(entry.enqueued_us);
        stats.queue_wait_total_us = stats.queue_wait_total_us.saturating_add(waited);
        stats.queue_wait_max_us = stats.queue_wait_max_us.max(waited);
        let priority = entry.class.priority();
        if root.last_priority == Some(priority) {
            root.priority_streak = root.priority_streak.saturating_add(1);
        } else {
            root.last_priority = Some(priority);
            root.priority_streak = 1;
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct IoDiagnostics {
    pub roots: Vec<RootDiagnostics>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RootDiagnostics {
    pub root: RootIdentity,
    pub limits: RootLimits,
    pub playback_leases: usize,
    pub playback_throttled: bool,
    pub hysteresis_remaining_us: u64,
    pub effective_bulk_limit: usize,
    pub classes: Vec<ClassDiagnostics>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ClassDiagnostics {
    pub class: WorkClass,
    pub queued: usize,
    pub active: usize,
    pub enqueued: u64,
    pub granted: u64,
    pub cancelled: u64,
    pub released: u64,
    pub max_queued: usize,
    pub queue_wait_total_us: u64,
    pub queue_wait_max_us: u64,
    pub cache_hits: u64,
    pub filesystem_operations: u64,
    pub filesystem_total_us: u64,
    pub filesystem_max_us: u64,
    pub release_outcomes: ReleaseOutcomeDiagnostics,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ReleaseOutcomeDiagnostics {
    pub success: u64,
    pub error: u64,
    pub cancelled: u64,
    pub stale: u64,
    pub worker_shutdown: u64,
    pub dropped: u64,
}

impl ReleaseOutcomeDiagnostics {
    fn from_counts(counts: [u64; OUTCOME_COUNT]) -> Self {
        Self {
            success: counts[PermitOutcome::Success.index()],
            error: counts[PermitOutcome::Error.index()],
            cancelled: counts[PermitOutcome::Cancelled.index()],
            stale: counts[PermitOutcome::Stale.index()],
            worker_shutdown: counts[PermitOutcome::WorkerShutdown.index()],
            dropped: counts[PermitOutcome::Dropped.index()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy(total: usize) -> IoPolicy {
        let limits = RootLimits {
            total,
            interactive_reserved: usize::from(total > 1),
            bulk_reserved: usize::from(total > 1),
            playback_bulk_limit: 1,
        };
        IoPolicy {
            local: limits,
            remote: limits,
            unknown: limits,
            playback_hysteresis: Duration::from_millis(100),
            priority_burst: 2,
        }
    }

    fn remote(name: &str) -> RootIdentity {
        RootIdentity::new(name, 1, RootKind::Remote)
    }

    fn class_diagnostics(
        diagnostics: &IoDiagnostics,
        root: &RootIdentity,
        class: WorkClass,
    ) -> ClassDiagnostics {
        diagnostics
            .roots
            .iter()
            .find(|candidate| &candidate.root == root)
            .and_then(|candidate| {
                candidate
                    .classes
                    .iter()
                    .find(|candidate| candidate.class == class)
            })
            .cloned()
            .expect("class diagnostics exist")
    }

    #[test]
    fn remote_limits_and_visible_reservation_are_enforced() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(3), clock);
        let root = remote("nas");

        let blocker_a = coordinator.enqueue(root.clone(), WorkClass::Scan);
        let blocker_b = coordinator.enqueue(root.clone(), WorkClass::Scan);
        let permit_a = blocker_a.try_acquire().unwrap().unwrap();
        let permit_b = blocker_b.try_acquire().unwrap().unwrap();
        let visible = coordinator.enqueue(root.clone(), WorkClass::Visible);
        let third_bulk = coordinator.enqueue(root.clone(), WorkClass::Prefetch);

        assert!(third_bulk.try_acquire().unwrap().is_none());
        let visible_permit = visible.try_acquire().unwrap().unwrap();
        let diagnostics = coordinator.diagnostics();
        assert_eq!(
            class_diagnostics(&diagnostics, &root, WorkClass::Prefetch).queued,
            1
        );
        assert_eq!(
            diagnostics
                .roots
                .iter()
                .find(|item| item.root == root)
                .unwrap()
                .limits
                .total,
            3
        );
        drop((permit_a, permit_b, visible_permit));
    }

    #[test]
    fn priority_is_bounded_and_bulk_gets_a_fair_turn() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(1), clock);
        let root = remote("fair");
        let blocker = coordinator.enqueue(root.clone(), WorkClass::Scan);
        let blocker_permit = blocker.try_acquire().unwrap().unwrap();

        let playback_one = coordinator.enqueue(root.clone(), WorkClass::Playback);
        let playback_two = coordinator.enqueue(root.clone(), WorkClass::Playback);
        let playback_three = coordinator.enqueue(root.clone(), WorkClass::Playback);
        let bulk = coordinator.enqueue(root.clone(), WorkClass::Scan);
        drop(blocker_permit);

        let first = playback_one.try_acquire().unwrap().unwrap();
        drop(first);
        let second = playback_two.try_acquire().unwrap().unwrap();
        drop(second);

        // The burst is capped at two, so the queued bulk request now advances.
        assert!(playback_three.try_acquire().unwrap().is_none());
        let bulk_permit = bulk.try_acquire().unwrap().unwrap();
        drop(bulk_permit);
        assert!(playback_three.try_acquire().unwrap().is_some());
    }

    /// WP-069 layer order. The stat sweep behind a size/date sort and the
    /// semantic index query walk the whole folder and no visible row waits on
    /// them, so they must yield to overscan thumbnail work. Before this they ran
    /// as `Metadata`, which outranks `Prefetch`: sorting a 141k-file root by
    /// size starved the very thumbnails the sort reorders.
    #[test]
    fn whole_folder_sweeps_yield_to_thumbnail_prefetch_but_still_get_a_turn() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(1), clock);
        let root = remote("layers");
        let blocker = coordinator.enqueue(root.clone(), WorkClass::Scan);
        let blocker_permit = blocker.try_acquire().unwrap().unwrap();

        // The sweep queues FIRST, so only priority — not arrival order — can
        // put the later thumbnail work ahead of it.
        let sweep_one = coordinator.enqueue(root.clone(), WorkClass::Background);
        let sweep_two = coordinator.enqueue(root.clone(), WorkClass::Background);
        let prefetch_one = coordinator.enqueue(root.clone(), WorkClass::Prefetch);
        let prefetch_two = coordinator.enqueue(root.clone(), WorkClass::Prefetch);
        drop(blocker_permit);

        let first = prefetch_one.try_acquire().unwrap().unwrap();
        assert!(
            sweep_one.try_acquire().unwrap().is_none(),
            "a whole-folder sweep took the permit ahead of queued overscan thumbnail work"
        );
        drop(first);
        let second = prefetch_two.try_acquire().unwrap().unwrap();
        drop(second);

        // ...and the sweep is not starved: the burst cap hands it a turn.
        assert!(
            sweep_one.try_acquire().unwrap().is_some(),
            "the sweep never got a fair turn, so a size sort could never settle"
        );
        drop(sweep_two);
    }

    #[test]
    fn cancellation_drop_and_permit_drop_release_capacity() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(1), clock);
        let root = remote("cleanup");
        let first = coordinator.enqueue(root.clone(), WorkClass::Visible);
        let first_permit = first.try_acquire().unwrap().unwrap();

        let cancelled = coordinator.enqueue(root.clone(), WorkClass::Scan);
        assert!(cancelled.cancel());
        assert!(matches!(cancelled.try_acquire(), Err(WaitError::Cancelled)));
        let dropped_request = coordinator.enqueue(root.clone(), WorkClass::Prefetch);
        drop(dropped_request);
        let next = coordinator.enqueue(root.clone(), WorkClass::Playback);
        drop(first_permit);
        let next_permit = next.try_acquire().unwrap().unwrap();
        drop(next_permit);

        let diagnostics = coordinator.diagnostics();
        assert_eq!(
            diagnostics
                .roots
                .iter()
                .find(|item| item.root == root)
                .unwrap()
                .classes
                .iter()
                .map(|class| class.active)
                .sum::<usize>(),
            0
        );
        assert_eq!(
            class_diagnostics(&diagnostics, &root, WorkClass::Scan).cancelled,
            1
        );
        assert_eq!(
            class_diagnostics(&diagnostics, &root, WorkClass::Playback)
                .release_outcomes
                .dropped,
            1
        );
    }

    #[test]
    fn background_wait_is_woken_by_cancellation() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(1), clock);
        let root = remote("wait-cancel");
        let blocker = coordinator.enqueue(root.clone(), WorkClass::Visible);
        let blocker_permit = blocker.try_acquire().unwrap().unwrap();
        let pending = coordinator.enqueue(root, WorkClass::Scan);
        let waiter_request = pending.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            waiter_request.wait().map(|_| ())
        });
        ready_rx.recv().unwrap();
        assert!(pending.cancel());
        assert_eq!(waiter.join().unwrap(), Err(WaitError::Cancelled));
        drop(blocker_permit);
    }

    #[test]
    fn roots_and_generations_have_independent_capacity() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(1), clock);
        let root_a = remote("nas-a");
        let root_b = remote("nas-b");
        let remapped_a = RootIdentity::new("nas-a", 2, RootKind::Remote);

        let a = coordinator.enqueue(root_a, WorkClass::Scan);
        let b = coordinator.enqueue(root_b, WorkClass::Scan);
        let remapped = coordinator.enqueue(remapped_a, WorkClass::Scan);
        let permits = [
            a.try_acquire().unwrap().unwrap(),
            b.try_acquire().unwrap().unwrap(),
            remapped.try_acquire().unwrap().unwrap(),
        ];
        assert_eq!(coordinator.diagnostics().roots.len(), 3);
        drop(permits);
    }

    #[test]
    fn playback_throttle_retains_one_bulk_lane_then_expires_after_hysteresis() {
        let clock = Arc::new(ManualClock::new(Duration::ZERO));
        let coordinator = MediaIoCoordinator::with_clock(test_policy(3), clock.clone());
        let root = remote("playback");
        let lease = coordinator.begin_playback(root.clone());
        let first = coordinator.enqueue(root.clone(), WorkClass::Scan);
        let second = coordinator.enqueue(root.clone(), WorkClass::Metadata);
        let third = coordinator.enqueue(root.clone(), WorkClass::Prefetch);

        let first_permit = first.try_acquire().unwrap().unwrap();
        assert!(second.try_acquire().unwrap().is_none());
        assert!(third.try_acquire().unwrap().is_none());
        drop(lease);
        clock.advance(Duration::from_millis(99));
        coordinator.refresh();
        assert!(second.try_acquire().unwrap().is_none());

        clock.advance(Duration::from_millis(2));
        coordinator.refresh();
        let second_permit = second.try_acquire().unwrap().unwrap();
        assert!(third.try_acquire().unwrap().is_none());
        let root_diagnostics = coordinator
            .diagnostics()
            .roots
            .into_iter()
            .find(|item| item.root == root)
            .unwrap();
        assert!(!root_diagnostics.playback_throttled);
        assert_eq!(root_diagnostics.effective_bulk_limit, 2);
        drop(first_permit);
        let third_permit = third.try_acquire().unwrap().unwrap();
        drop((second_permit, third_permit));
    }

    #[test]
    fn diagnostics_attribute_cache_and_filesystem_timing() {
        let coordinator = MediaIoCoordinator::with_policy(test_policy(1));
        let root = remote("diagnostics");
        coordinator.record_cache_hit(&root, WorkClass::Visible);
        coordinator.record_filesystem_duration(
            &root,
            WorkClass::Visible,
            Duration::from_micros(250),
        );
        let diagnostics = class_diagnostics(&coordinator.diagnostics(), &root, WorkClass::Visible);
        assert_eq!(diagnostics.cache_hits, 1);
        assert_eq!(diagnostics.filesystem_operations, 1);
        assert_eq!(diagnostics.filesystem_total_us, 250);
        assert_eq!(diagnostics.filesystem_max_us, 250);
    }
}
