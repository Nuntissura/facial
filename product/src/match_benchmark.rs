//! Bounded, explicit WP-087 render-sample capture.
//!
//! Construction performs configuration, hashing and file setup on a dedicated
//! writer thread and waits for its ready handshake before the first paint.
//! The per-frame hook only validates values and `try_send`s into a bounded
//! channel; it never opens files or serializes data.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const CONFIG_ENV: &str = "FACIAL_MATCH_BENCHMARK_CONFIG";
const CONFIG_MAX_BYTES: u64 = 64 * 1024;
const EVIDENCE_MAX_BYTES: u64 = 1024 * 1024;
const SESSION_SECONDS: u64 = 150;
const WARMUP_SECONDS: u64 = 30;
const MEASURE_SECONDS: u64 = 120;
const MAX_FRAME_RECORDS: u64 = 100_000;
const MAX_JSONL_BYTES: u64 = 20 * 1024 * 1024;
const CHANNEL_CAPACITY: usize = 4096;
const MAX_LINE_BYTES: usize = 16 * 1024;
const CARGO_LOCK_BYTES: &[u8] = include_bytes!("../Cargo.lock");
const ADMISSION_SEALED: u64 = 1 << 63;
const ADMISSION_CLOSE_WAIT: Duration = Duration::from_millis(250);

static MEDIA_LABEL_MODE: AtomicBool = AtomicBool::new(false);
static MATCH_WORKERS: AtomicU64 = AtomicU64::new(0);
static MODEL_LOADS: AtomicU64 = AtomicU64::new(0);
static INDEX_QUERIES: AtomicU64 = AtomicU64::new(0);
static MATCH_DATABASE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static DISPLAY_OBSERVATIONS: AtomicU64 = AtomicU64::new(0);
static DISPLAY_INVALID: AtomicBool = AtomicBool::new(false);
static PREVIOUS_DISPLAY_VALID: AtomicBool = AtomicBool::new(false);
static VISIBLE_TILE_LOOKUPS: AtomicU64 = AtomicU64::new(0);
static PREVIOUS_TILE_LOOKUPS: AtomicU64 = AtomicU64::new(0);
static VISIBLE_TILE_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static VISIBLE_TILE_MAX: AtomicU64 = AtomicU64::new(0);
static VISIBLE_WORK_FRAMES: AtomicU64 = AtomicU64::new(0);
static MEDIA_LABEL_CONFIG_SHA256: OnceLock<String> = OnceLock::new();

pub(crate) fn media_label_mode() -> bool {
    MEDIA_LABEL_MODE.load(Ordering::Acquire)
}
pub(crate) fn note_match_worker() {
    MATCH_WORKERS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_model_load() {
    MODEL_LOADS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_index_query() {
    INDEX_QUERIES.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_match_database_request() {
    MATCH_DATABASE_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn media_label_files(root: &Path) -> Vec<String> {
    (0..50_000)
        .map(|index| {
            root.join(format!("label-pool-{index:05}.png"))
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn media_label_fixture_sha256() -> String {
    let names: Vec<String> = (0..50_000)
        .map(|index| format!("label-pool-{index:05}.png"))
        .collect();
    sha256_bytes(&serde_json::to_vec(&names).expect("bounded fixture serializes"))
}

fn config_binding_matches(expected: Option<&String>, bytes: &[u8]) -> bool {
    expected.is_some_and(|digest| digest == &sha256_bytes(bytes))
}

fn paired_frame_work(
    previous: u64,
    current: u64,
    previous_display: bool,
    current_display: bool,
) -> Option<u64> {
    if !previous_display || !current_display {
        return None;
    }
    current.checked_sub(previous)
}

/// Explicit native fixture launch: inspect isolation before any service exists.
pub(crate) fn configure_media_label_launch(
    config: &mut crate::config::AppConfig,
) -> Result<RenderState, String> {
    let path = env::var_os(CONFIG_ENV)
        .ok_or("Media label benchmark requires FACIAL_MATCH_BENCHMARK_CONFIG")?;
    let bytes = read_bounded_regular_file(Path::new(&path), CONFIG_MAX_BYTES, "benchmark config")?;
    let capture: CaptureConfig =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid benchmark config: {e}"))?;
    validate_config(&capture)?;
    if !capture.state.is_media_labels() {
        return Err("Media label launch requires a media_labels state".into());
    }
    let root = PathBuf::from(
        capture
            .media_labels_workspace
            .as_ref()
            .ok_or("Media label benchmark requires media_labels_workspace")?,
    );
    let root = inspect_media_label_workspace(&root, &config.workspace_root, &config.repo_root)?;
    let state = root.join(".facial");
    fs::create_dir(&state).map_err(|e| format!("claim fresh benchmark state: {e}"))?;
    config.workspace_root = root;
    config.worktrees_root = state.join("worktrees");
    config.api_root = state.join("data").join("api");
    config.debug_log_path = state.join("debug.log");
    config.model_registry_path = state.join("model_registry.json");
    config.settings_path_override = Some(state.join("settings.json"));
    config.copy_location = None;
    config.identity_manifest_path = None;
    config.identity_model_path = None;
    config.identity_detector_path = None;
    config.identity_reference_dir = None;
    config.identity_negative_dir = None;
    config.landmark_model_path = None;
    config.font_size_pt = 19.0;
    MEDIA_LABEL_CONFIG_SHA256
        .set(sha256_bytes(&bytes))
        .map_err(|_| "Media label benchmark already initialized")?;
    MEDIA_LABEL_MODE.store(true, Ordering::Release);
    Ok(capture.state)
}

fn inspect_media_label_workspace(
    root: &Path,
    workspace: &Path,
    repo: &Path,
) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("Media label workspace must be an absolute fresh directory".into());
    }
    for component in root.ancestors() {
        let metadata = fs::symlink_metadata(component)
            .map_err(|e| format!("inspect benchmark workspace ancestor: {e}"))?;
        if is_reparse_or_symlink(&metadata) || !metadata.is_dir() {
            return Err("Media label workspace cannot cross symlinks or reparse points".into());
        }
    }
    let root = fs::canonicalize(&root).map_err(|e| format!("inspect benchmark workspace: {e}"))?;
    reject_reparse_components(&root, &root)?;
    if root == fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
        || root == fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf())
        || fs::read_dir(&root)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err("Media label benchmark requires a fresh empty dedicated workspace distinct from configured workspace and repository".into());
    }
    Ok(root)
}

pub(crate) fn observe_media_label_runtime(
    ctx: &eframe::egui::Context,
    lookups: u64,
    recorded: bool,
    font_size: f32,
) {
    let previous = PREVIOUS_TILE_LOOKUPS.swap(lookups, Ordering::AcqRel);
    let (native, size) = ctx.input(|i| {
        (
            i.viewport().native_pixels_per_point,
            i.viewport().inner_rect.map(|r| r.size()),
        )
    });
    let valid = native == Some(1.0)
        && ctx.pixels_per_point() == 1.0
        && size.is_some_and(|s| s.x == 1920.0 && s.y == 1080.0)
        && font_size == 19.0
        && ctx
            .style()
            .text_styles
            .get(&eframe::egui::TextStyle::Body)
            .is_some_and(|font| {
                font.size == 19.0 && font.family == eframe::egui::FontFamily::Proportional
            });
    let previous_display = PREVIOUS_DISPLAY_VALID.swap(valid, Ordering::AcqRel);
    if !recorded {
        return;
    }
    let Some(work) = paired_frame_work(previous, lookups, previous_display, valid) else {
        DISPLAY_INVALID.store(true, Ordering::Release);
        return;
    };
    DISPLAY_OBSERVATIONS.fetch_add(1, Ordering::Relaxed);
    VISIBLE_TILE_LOOKUPS.fetch_add(work, Ordering::Relaxed);
    VISIBLE_TILE_MIN.fetch_min(work, Ordering::Relaxed);
    VISIBLE_TILE_MAX.fetch_max(work, Ordering::Relaxed);
    VISIBLE_WORK_FRAMES.fetch_add(1, Ordering::Relaxed);
}

struct AdmissionTicket<'a>(&'a AtomicU64);

impl Drop for AdmissionTicket<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn try_admit(admission: &AtomicU64) -> Option<AdmissionTicket<'_>> {
    let mut state = admission.load(Ordering::Acquire);
    loop {
        if state & ADMISSION_SEALED != 0 {
            return None;
        }
        match admission.compare_exchange_weak(state, state + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Some(AdmissionTicket(admission)),
            Err(observed) => state = observed,
        }
    }
}

fn seal_admission(admission: &AtomicU64, wait: Duration) -> bool {
    admission.fetch_or(ADMISSION_SEALED, Ordering::AcqRel);
    let started = Instant::now();
    while admission.load(Ordering::Acquire) != ADMISSION_SEALED {
        if started.elapsed() >= wait {
            return false;
        }
        thread::yield_now();
    }
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RenderState {
    MatchDisabled,
    ActiveQuiet,
    TypicalFaceEdit,
    #[serde(rename = "pathological_1000_face_edit")]
    Pathological1000FaceEdit,
    MediaLabelsBaseline,
    MediaLabelsCandidate,
}

impl RenderState {
    pub(crate) fn is_media_labels(self) -> bool {
        matches!(self, Self::MediaLabelsBaseline | Self::MediaLabelsCandidate)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureConfig {
    schema_version: u32,
    run_id: String,
    state: RenderState,
    git_commit: String,
    model_generation: String,
    schema_generation: String,
    fixture_generation: String,
    cache_state: String,
    input_script_sha256: String,
    hardware_manifest_sha256: String,
    display_profile_sha256: String,
    power_mode: String,
    admission_counts: Option<AdmissionCounts>,
    admission_evidence: Option<EvidenceReference>,
    #[serde(default)]
    media_labels_workspace: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AdmissionCounts {
    match_workers: u64,
    model_loads: u64,
    match_index_queries: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceReference {
    /// Optional workspace-relative measurement-input reference, not proof of
    /// admission state for the interval that is about to be captured.
    path: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct FrameRecord {
    record_type: &'static str,
    frame_end_timestamp_us: u64,
    frame_duration_us: u64,
}

#[derive(Serialize)]
struct EndRecord {
    record_type: &'static str,
    outcome: &'static str,
    sample_count: u64,
    observed_at_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_evidence: Option<serde_json::Value>,
}

#[derive(Debug)]
enum WriterMessage {
    Start,
    Begin(Instant),
    Frame(FrameRecord),
}

/// A clone-free paint-path handle to one explicitly configured capture.
pub(crate) struct MatchBenchmarkCapture {
    origin: Instant,
    sender: SyncSender<WriterMessage>,
    invalidated: Arc<AtomicBool>,
    admission: Arc<AtomicU64>,
    last_timestamp_us: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleResult {
    Warmup,
    Recorded,
    MissingPreviousFrame,
    OutsideMeasurement,
    Invalidated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SamplePhase {
    Warmup,
    Measure,
    Finished,
}

fn sample_phase(timestamp_us: u64) -> SamplePhase {
    if timestamp_us < WARMUP_SECONDS * 1_000_000 {
        SamplePhase::Warmup
    } else if timestamp_us < SESSION_SECONDS * 1_000_000 {
        SamplePhase::Measure
    } else {
        SamplePhase::Finished
    }
}

impl MatchBenchmarkCapture {
    pub(crate) fn finished(&self) -> bool {
        self.origin.elapsed() >= Duration::from_secs(SESSION_SECONDS)
    }

    /// Keep runtime observations inside terminal sealing, associated with the
    /// previous native frame whose CPU duration is being recorded.
    pub(crate) fn observe_media_label_frame(
        &self,
        cpu: Option<f32>,
        observed: Instant,
        ctx: &eframe::egui::Context,
        lookups: u64,
        font_size: f32,
    ) -> SampleResult {
        let Some(_ticket) = try_admit(&self.admission) else {
            return SampleResult::OutsideMeasurement;
        };
        if ctx.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    eframe::egui::Event::Key { .. }
                        | eframe::egui::Event::PointerButton { .. }
                        | eframe::egui::Event::Text(_)
                        | eframe::egui::Event::Scroll(_)
                        | eframe::egui::Event::Zoom(_)
                        | eframe::egui::Event::Touch { .. }
                        | eframe::egui::Event::MouseWheel { .. }
                )
            })
        }) {
            self.invalidated.store(true, Ordering::Release);
        }
        let result = self.observe_previous_frame(cpu, observed);
        observe_media_label_runtime(ctx, lookups, result == SampleResult::Recorded, font_size);
        result
    }
    /// Start the configured run before the first paint. Returns `Ok(None)` when
    /// capture is not explicitly enabled. Setup I/O occurs on the writer.
    pub(crate) fn from_environment(workspace_root: &Path) -> Result<Option<Self>, String> {
        let Some(config_path) = env::var_os(CONFIG_ENV) else {
            return Ok(None);
        };
        let config_path = PathBuf::from(config_path);
        let workspace = workspace_root.to_path_buf();
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let invalidated = Arc::new(AtomicBool::new(false));
        let worker_invalidated = Arc::clone(&invalidated);
        let admission = Arc::new(AtomicU64::new(0));
        let worker_admission = Arc::clone(&admission);
        let worker_workspace = workspace.clone();
        thread::Builder::new()
            .name("match-benchmark-writer".to_string())
            .spawn(move || {
                let prepared = prepare_run(&worker_workspace, &config_path);
                let (mut output, config, header, output_path) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                match receiver.recv() {
                    Ok(WriterMessage::Start) => {}
                    _ => return,
                }
                if let Err(error) = write_header(&mut output, &header) {
                    worker_invalidated.store(true, Ordering::Release);
                    let _ = output.flush();
                    let _ = started_sender.send(Err(error.to_string()));
                    eprintln!("Match benchmark writer could not write its header: {error}");
                    return;
                }
                if let Err(error) = output.flush() {
                    worker_invalidated.store(true, Ordering::Release);
                    let _ = started_sender
                        .send(Err(format!("could not flush benchmark header: {error}")));
                    return;
                }
                if started_sender.send(Ok(())).is_err() {
                    worker_invalidated.store(true, Ordering::Release);
                    return;
                }
                let origin = match receiver.recv() {
                    Ok(WriterMessage::Begin(origin)) => origin,
                    _ => return,
                };
                run_writer(
                    &receiver,
                    &mut output,
                    &config,
                    origin,
                    &worker_invalidated,
                    &worker_admission,
                    &output_path,
                );
            })
            .map_err(|error| format!("could not start benchmark writer: {error}"))?;

        ready_receiver
            .recv()
            .map_err(|_| "benchmark writer exited before setup completed".to_string())??;
        sender
            .send(WriterMessage::Start)
            .map_err(|_| "benchmark writer exited before capture started".to_string())?;
        started_receiver
            .recv()
            .map_err(|_| "benchmark writer exited before its header was flushed".to_string())??;
        let origin = Instant::now();
        sender
            .send(WriterMessage::Begin(origin))
            .map_err(|_| "benchmark writer exited before capture timing began".to_string())?;
        Ok(Some(Self {
            origin,
            sender,
            invalidated,
            admission,
            last_timestamp_us: AtomicU64::new(0),
        }))
    }

    /// Record eframe's prior-frame CPU duration at the current monotonic
    /// observation point. This method performs no file or database work.
    pub(crate) fn observe_previous_frame(
        &self,
        previous_frame_cpu_seconds: Option<f32>,
        _observed_at: Instant,
    ) -> SampleResult {
        let Some(_ticket) = try_admit(&self.admission) else {
            return SampleResult::OutsideMeasurement;
        };
        // Admission can occur after the caller's update-start timestamp if the
        // caller was suspended. Classify at admission, never with that stale time.
        let observed_at = Instant::now();
        if self.invalidated.load(Ordering::Acquire) {
            return SampleResult::Invalidated;
        }
        let Some(timestamp_us) = elapsed_us(self.origin, observed_at) else {
            self.invalidated.store(true, Ordering::Release);
            return SampleResult::Invalidated;
        };
        if sample_phase(timestamp_us) == SamplePhase::Finished {
            return SampleResult::OutsideMeasurement;
        }
        let Some(cpu_seconds) = previous_frame_cpu_seconds else {
            return SampleResult::MissingPreviousFrame;
        };
        if !cpu_seconds.is_finite() || cpu_seconds < 0.0 {
            self.invalidated.store(true, Ordering::Release);
            return SampleResult::Invalidated;
        }
        match sample_phase(timestamp_us) {
            SamplePhase::Warmup => return SampleResult::Warmup,
            SamplePhase::Measure => {}
            SamplePhase::Finished => return SampleResult::OutsideMeasurement,
        }
        let previous_timestamp = self.last_timestamp_us.swap(timestamp_us, Ordering::AcqRel);
        if previous_timestamp != 0 && timestamp_us <= previous_timestamp {
            self.invalidated.store(true, Ordering::Release);
            return SampleResult::Invalidated;
        }
        let frame_duration_us = (f64::from(cpu_seconds) * 1_000_000.0).round() as u64;
        if frame_duration_us == 0 {
            self.invalidated.store(true, Ordering::Release);
            return SampleResult::Invalidated;
        }
        let frame = FrameRecord {
            record_type: "frame",
            frame_end_timestamp_us: timestamp_us,
            frame_duration_us,
        };
        match self.sender.try_send(WriterMessage::Frame(frame)) {
            Ok(()) => SampleResult::Recorded,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.invalidated.store(true, Ordering::Release);
                SampleResult::Invalidated
            }
        }
    }
}

fn prepare_run(
    workspace_root: &Path,
    config_path: &Path,
) -> Result<(BufWriter<File>, CaptureConfig, OwnedRunHeader, PathBuf), String> {
    let workspace = fs::canonicalize(workspace_root)
        .map_err(|error| format!("benchmark workspace root is unavailable: {error}"))?;
    let config_bytes =
        read_bounded_regular_file(config_path, CONFIG_MAX_BYTES, "benchmark config")?;
    let config: CaptureConfig = serde_json::from_slice(&config_bytes)
        .map_err(|error| format!("benchmark config is invalid JSON: {error}"))?;
    validate_config(&config)?;
    if config.state.is_media_labels()
        && (!media_label_mode()
            || !config_binding_matches(MEDIA_LABEL_CONFIG_SHA256.get(), &config_bytes))
    {
        return Err(
            "Media label capture requires the exact explicitly isolated launch configuration"
                .into(),
        );
    }
    if let Some(evidence) = &config.admission_evidence {
        validate_evidence(&workspace, evidence)?;
    }
    let header = owned_header(&config)?;
    let output_path = create_output_file(&workspace, &config.run_id)?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map(BufWriter::new)
        .map_err(|error| format!("could not create benchmark output: {error}"))?;
    reject_reparse_components(&workspace, &output_path)?;
    Ok((output, config, header, output_path))
}

#[derive(Serialize)]
struct OwnedRunHeader {
    schema_version: u32,
    record_type: &'static str,
    run_id: String,
    state: RenderState,
    measurement_start_us: u64,
    measurement_end_us: u64,
    warmup_seconds: u64,
    package_sha256: String,
    app_version: &'static str,
    git_commit: String,
    cargo_lock_sha256: String,
    model_generation: String,
    schema_generation: String,
    fixture_generation: String,
    cache_state: String,
    input_script_sha256: String,
    hardware_manifest_sha256: String,
    display_profile_sha256: String,
    power_mode: String,
    metric_scope: &'static str,
    timestamp_scope: &'static str,
    admission_counts: Option<AdmissionCounts>,
    admission_evidence: Option<EvidenceReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_labels_fixture: Option<serde_json::Value>,
}

fn owned_header(config: &CaptureConfig) -> Result<OwnedRunHeader, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve running executable: {error}"))?;
    let package_sha256 = sha256_path(&executable, u64::MAX)?;
    Ok(OwnedRunHeader {
        schema_version: 1,
        record_type: "run",
        run_id: config.run_id.clone(),
        state: config.state,
        measurement_start_us: WARMUP_SECONDS * 1_000_000,
        measurement_end_us: SESSION_SECONDS * 1_000_000,
        warmup_seconds: WARMUP_SECONDS,
        package_sha256,
        app_version: env!("CARGO_PKG_VERSION"),
        git_commit: config.git_commit.clone(),
        cargo_lock_sha256: sha256_bytes(CARGO_LOCK_BYTES),
        model_generation: if config.state.is_media_labels() { "unconfigured".into() } else { config.model_generation.clone() },
        schema_generation: config.schema_generation.clone(),
        fixture_generation: config.fixture_generation.clone(),
        cache_state: config.cache_state.clone(),
        input_script_sha256: config.input_script_sha256.clone(),
        hardware_manifest_sha256: config.hardware_manifest_sha256.clone(),
        display_profile_sha256: config.display_profile_sha256.clone(),
        power_mode: config.power_mode.clone(),
        metric_scope: "eframe_update_render_cpu_time_excluding_vsync",
        timestamp_scope:
            "previous_frame_cpu_usage_observed_at_next_update_monotonic_since_capture_start",
        admission_counts: config
            .admission_counts
            .as_ref()
            .map(|counts| AdmissionCounts {
                match_workers: counts.match_workers,
                model_loads: counts.model_loads,
                match_index_queries: counts.match_index_queries,
            }),
        admission_evidence: config
            .admission_evidence
            .as_ref()
            .map(|evidence| EvidenceReference {
                path: evidence.path.clone(),
                sha256: evidence.sha256.clone(),
            }),
        media_labels_fixture: config.state.is_media_labels().then(|| serde_json::json!({
            "fixture_sha256": media_label_fixture_sha256(), "rows": 50_000,
            "assignment": if config.state == RenderState::MediaLabelsBaseline { "empty" } else { "five_ordered" },
            "build_ui_sha256": sha256_bytes(include_bytes!("ui.rs")),
            "build_lib_sha256": sha256_bytes(include_bytes!("lib.rs")),
            "build_collector_sha256": sha256_bytes(include_bytes!("match_benchmark.rs"))
        })),
    })
}

fn validate_config(config: &CaptureConfig) -> Result<(), String> {
    if config.state.is_media_labels() != config.media_labels_workspace.is_some() {
        return Err(
            "Media label states require media_labels_workspace; other states must omit it".into(),
        );
    }
    if config.schema_version != 1 {
        return Err("benchmark config schema_version must be 1".to_string());
    }
    if !valid_run_id(&config.run_id) {
        return Err("run_id must be 1-64 ASCII letters, digits, hyphens or underscores and start alphanumeric".to_string());
    }
    if !(config.git_commit.len() == 40 || config.git_commit.len() == 64)
        || !config
            .git_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("git_commit must be a full hexadecimal commit ID".to_string());
    }
    for (name, value) in [
        ("git_commit", &config.git_commit),
        ("model_generation", &config.model_generation),
        ("schema_generation", &config.schema_generation),
        ("fixture_generation", &config.fixture_generation),
        ("cache_state", &config.cache_state),
        ("power_mode", &config.power_mode),
    ] {
        if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(format!("{name} must be a non-empty bounded metadata value"));
        }
    }
    for (name, value) in [
        ("input_script_sha256", &config.input_script_sha256),
        ("hardware_manifest_sha256", &config.hardware_manifest_sha256),
        ("display_profile_sha256", &config.display_profile_sha256),
    ] {
        if !valid_sha256(value) {
            return Err(format!(
                "{name} must be 64 lowercase hexadecimal characters"
            ));
        }
    }
    if let Some(evidence) = &config.admission_evidence {
        if !valid_sha256(&evidence.sha256) {
            return Err(
                "admission evidence SHA-256 must be 64 lowercase hexadecimal characters"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn valid_run_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_bounded_regular_file(path: &Path, cap: u64, label: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {label}: {error}"))?;
    if is_reparse_or_symlink(&metadata) || !metadata.is_file() {
        return Err(format!("{label} must be a regular non-reparse file"));
    }
    if metadata.len() > cap {
        return Err(format!("{label} exceeds the {cap}-byte limit"));
    }
    let file = File::open(path).map_err(|error| format!("could not open {label}: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {label}: {error}"))?;
    if bytes.len() as u64 > cap {
        return Err(format!(
            "{label} grew beyond the {cap}-byte limit while reading"
        ));
    }
    Ok(bytes)
}

fn validate_evidence(workspace: &Path, evidence: &EvidenceReference) -> Result<(), String> {
    if !valid_sha256(&evidence.sha256) {
        return Err(
            "admission evidence SHA-256 must be 64 lowercase hexadecimal characters".to_string(),
        );
    }
    let relative = Path::new(&evidence.path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(
            "admission evidence path must be workspace-relative and contain no traversal"
                .to_string(),
        );
    }
    let path = workspace.join(relative);
    reject_reparse_components(workspace, &path)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("admission evidence reference is unavailable: {error}"))?;
    if is_reparse_or_symlink(&metadata)
        || !metadata.is_file()
        || metadata.len() > EVIDENCE_MAX_BYTES
    {
        return Err(
            "admission evidence must be a regular workspace file no larger than 1 MiB".to_string(),
        );
    }
    let observed = sha256_path(&path, EVIDENCE_MAX_BYTES)?;
    if observed != evidence.sha256 {
        return Err("admission evidence SHA-256 does not match its referenced file".to_string());
    }
    Ok(())
}

fn create_output_file(workspace: &Path, run_id: &str) -> Result<PathBuf, String> {
    let facial = workspace.join(".facial");
    ensure_child_directory(workspace, &facial)?;
    let benchmarks = facial.join("benchmarks");
    ensure_child_directory(&facial, &benchmarks)?;
    let canonical = fs::canonicalize(&benchmarks)
        .map_err(|error| format!("could not resolve benchmark output directory: {error}"))?;
    if canonical != benchmarks {
        return Err("benchmark output directory escaped its canonical workspace path".to_string());
    }
    Ok(benchmarks.join(format!("{run_id}.jsonl")))
}

fn ensure_child_directory(parent: &Path, child: &Path) -> Result<(), String> {
    if child.exists() {
        let metadata = fs::symlink_metadata(child)
            .map_err(|error| format!("could not inspect benchmark directory: {error}"))?;
        if is_reparse_or_symlink(&metadata) || !metadata.is_dir() {
            return Err("benchmark output parent must be a plain directory, never a symlink or reparse point".to_string());
        }
    } else {
        fs::create_dir(child)
            .map_err(|error| format!("could not create benchmark directory: {error}"))?;
    }
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("could not resolve benchmark parent: {error}"))?;
    let canonical_child = fs::canonicalize(child)
        .map_err(|error| format!("could not resolve benchmark directory: {error}"))?;
    if canonical_child.parent() != Some(canonical_parent.as_path()) {
        return Err("benchmark output parent escaped its workspace directory".to_string());
    }
    Ok(())
}

fn reject_reparse_components(root: &Path, target: &Path) -> Result<(), String> {
    let root = fs::canonicalize(root)
        .map_err(|error| format!("could not resolve workspace root: {error}"))?;
    if !target.starts_with(&root) {
        return Err("referenced evidence path escaped the workspace root".to_string());
    }
    let relative = target
        .strip_prefix(&root)
        .map_err(|_| "referenced evidence path escaped the workspace root".to_string())?;
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("referenced evidence path contains a non-normal component".to_string());
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("could not inspect evidence path component: {error}"))?;
        if is_reparse_or_symlink(&metadata) {
            return Err("referenced evidence path crosses a symlink or reparse point".to_string());
        }
    }
    if !current.starts_with(&root) {
        return Err("referenced evidence path escaped the workspace root".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn run_writer(
    receiver: &Receiver<WriterMessage>,
    output: &mut BufWriter<File>,
    config: &CaptureConfig,
    origin: Instant,
    invalidated: &AtomicBool,
    admission: &AtomicU64,
    output_path: &Path,
) {
    let deadline = origin + Duration::from_secs(SESSION_SECONDS);
    let mut sample_count = 0u64;
    let mut written_bytes = match output.get_ref().metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => MAX_JSONL_BYTES,
    };
    let mut outcome = "completed";

    loop {
        if invalidated.load(Ordering::Acquire) {
            outcome = "overflow_or_invalid_sample";
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(WriterMessage::Start | WriterMessage::Begin(_)) => {
                outcome = "invalid_duplicate_start";
                break;
            }
            Ok(WriterMessage::Frame(frame)) => {
                if sample_count >= MAX_FRAME_RECORDS {
                    outcome = "record_limit_exceeded";
                    invalidated.store(true, Ordering::Release);
                    break;
                }
                match write_record(output, &frame, &mut written_bytes) {
                    Ok(()) => sample_count += 1,
                    Err(error) => {
                        outcome = "write_or_byte_limit_failure";
                        invalidated.store(true, Ordering::Release);
                        eprintln!("Match benchmark writer failed: {error}");
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                outcome = "capture_interrupted";
                break;
            }
        }
    }

    // Seal first, then await only short in-flight paint hooks. No producer holds
    // a ticket over filesystem work, and no new producer can race the drain/end.
    if !seal_admission(admission, ADMISSION_CLOSE_WAIT) {
        outcome = "producer_close_timeout";
        invalidated.store(true, Ordering::Release);
    }
    if outcome == "completed" {
        loop {
            match receiver.try_recv() {
                Ok(WriterMessage::Frame(frame)) => {
                    if sample_count >= MAX_FRAME_RECORDS {
                        outcome = "record_limit_exceeded";
                        invalidated.store(true, Ordering::Release);
                        break;
                    }
                    if let Err(error) = write_record(output, &frame, &mut written_bytes) {
                        outcome = "write_or_byte_limit_failure";
                        invalidated.store(true, Ordering::Release);
                        eprintln!("Match benchmark writer failed while draining: {error}");
                        break;
                    }
                    sample_count += 1;
                }
                Ok(WriterMessage::Start | WriterMessage::Begin(_)) => {
                    outcome = "invalid_duplicate_start";
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    outcome = "capture_interrupted";
                    break;
                }
            }
        }
    }

    if invalidated.load(Ordering::Acquire) && outcome == "completed" {
        outcome = "overflow_or_invalid_sample";
    }

    let observed_at_us = elapsed_us(origin, Instant::now()).unwrap_or(SESSION_SECONDS * 1_000_000);
    if outcome == "completed" && observed_at_us < SESSION_SECONDS * 1_000_000 {
        outcome = "capture_interrupted";
    }
    let end = EndRecord {
        record_type: "end",
        outcome,
        sample_count,
        observed_at_us,
        runtime_evidence: config.state.is_media_labels().then(|| {
            serde_json::json!({
                "match_workers": MATCH_WORKERS.load(Ordering::Acquire),
                "model_loads": MODEL_LOADS.load(Ordering::Acquire),
                "match_index_queries": INDEX_QUERIES.load(Ordering::Acquire),
                "match_database_requests": MATCH_DATABASE_REQUESTS.load(Ordering::Acquire),
                "visible_tile_lookups": VISIBLE_TILE_LOOKUPS.load(Ordering::Acquire),
                "visible_tile_lookups_min": VISIBLE_TILE_MIN.load(Ordering::Acquire),
                "visible_tile_lookups_max": VISIBLE_TILE_MAX.load(Ordering::Acquire),
                "visible_work_frames": VISIBLE_WORK_FRAMES.load(Ordering::Acquire),
                "display_observations": DISPLAY_OBSERVATIONS.load(Ordering::Acquire),
                "display_valid": !DISPLAY_INVALID.load(Ordering::Acquire),
                "viewport_physical_px": [1920,1080], "native_pixels_per_point":1.0,
                "egui_pixels_per_point":1.0, "font_size_pt":19.0,"font_family":"Inter",
                "fixture_sha256":media_label_fixture_sha256()
            })
        }),
    };
    if let Err(error) = finish_output(output, &end, &mut written_bytes, invalidated) {
        eprintln!(
            "Match benchmark run {} terminal write/flush failed: {error}; inspect {}",
            config.run_id,
            output_path.display()
        );
    }
    if outcome != "completed" {
        eprintln!(
            "Match benchmark run {} is invalid ({outcome}); inspect {}",
            config.run_id,
            output_path.display()
        );
    }
}

fn write_header(output: &mut BufWriter<File>, header: &OwnedRunHeader) -> io::Result<()> {
    let mut bytes_written = 0;
    write_record(output, header, &mut bytes_written)
}

fn finish_output<W: Write>(
    output: &mut BufWriter<W>,
    end: &EndRecord,
    written_bytes: &mut u64,
    invalidated: &AtomicBool,
) -> io::Result<()> {
    let result = write_record(output, end, written_bytes).and_then(|()| output.flush());
    if result.is_err() {
        invalidated.store(true, Ordering::Release);
    }
    result
}

fn write_record<T: Serialize, W: Write>(
    output: &mut BufWriter<W>,
    record: &T,
    written_bytes: &mut u64,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark record exceeds line bound",
        ));
    }
    let next = written_bytes.saturating_add(line.len() as u64);
    if next > MAX_JSONL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark output exceeded 20 MiB",
        ));
    }
    output.write_all(&line)?;
    *written_bytes = next;
    Ok(())
}

fn elapsed_us(origin: Instant, observed_at: Instant) -> Option<u64> {
    observed_at
        .checked_duration_since(origin)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_path(path: &Path, cap: u64) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect file for hashing: {error}"))?;
    if is_reparse_or_symlink(&metadata) || !metadata.is_file() || metadata.len() > cap {
        return Err("file to hash must be a bounded regular non-reparse file".to_string());
    }
    let mut file =
        File::open(path).map_err(|error| format!("could not open file for hashing: {error}"))?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut block = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut block)
            .map_err(|error| format!("could not hash file: {error}"))?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > cap {
            return Err("file to hash grew beyond its configured bound".to_string());
        }
        hash.update(&block[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wp087_media_label_workspace_requires_fresh_distinct_directory() {
        let root = env::temp_dir().join(format!("facial-label-isolation-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let workspace = root.join("workspace");
        let repo = root.join("repo");
        let fresh = root.join("fresh");
        for directory in [&workspace, &repo, &fresh] {
            fs::create_dir(directory).unwrap();
        }
        assert!(inspect_media_label_workspace(&fresh, &workspace, &repo).is_ok());
        assert!(inspect_media_label_workspace(&workspace, &workspace, &repo).is_err());
        assert!(inspect_media_label_workspace(&repo, &workspace, &repo).is_err());
        assert!(inspect_media_label_workspace(Path::new("relative"), &workspace, &repo).is_err());
        fs::write(fresh.join("operator-data"), b"preserve").unwrap();
        assert!(inspect_media_label_workspace(&fresh, &workspace, &repo).is_err());
        assert_eq!(fs::read(fresh.join("operator-data")).unwrap(), b"preserve");
        assert!(!fresh.join(".facial").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wp087_media_label_config_binding_rejects_changed_launch_bytes() {
        let digest = sha256_bytes(b"original-launch");
        assert!(config_binding_matches(Some(&digest), b"original-launch"));
        assert!(!config_binding_matches(Some(&digest), b"changed-launch"));
        assert!(!config_binding_matches(None, b"original-launch"));
    }

    #[test]
    fn wp087_media_label_pairing_rejects_prior_display_and_counter_reset() {
        assert_eq!(paired_frame_work(90, 108, true, true), Some(18));
        assert_eq!(paired_frame_work(90, 108, false, true), None);
        assert_eq!(paired_frame_work(90, 108, true, false), None);
        assert_eq!(paired_frame_work(108, 90, true, true), None);
    }

    #[test]
    fn wp087_media_label_seal_cannot_snapshot_partial_frame_telemetry() {
        let admission = AtomicU64::new(0);
        let telemetry = AtomicU64::new(0);
        let ticket = try_admit(&admission).unwrap();
        assert!(!seal_admission(&admission, Duration::ZERO));
        telemetry.store(18, Ordering::Release);
        drop(ticket);
        assert!(seal_admission(&admission, Duration::ZERO));
        assert_eq!(telemetry.load(Ordering::Acquire), 18);
    }

    #[test]
    fn wp087_terminal_seal_waits_for_late_producer_failure() {
        let workspace =
            env::temp_dir().join(format!("facial-benchmark-seal-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&workspace).unwrap();
        let path = workspace.join("race.jsonl");
        let admission = Arc::new(AtomicU64::new(0));
        let invalidated = Arc::new(AtomicBool::new(false));
        let (producer_ready, ready) = mpsc::sync_channel(1);
        let (release, producer_release) = mpsc::sync_channel(1);
        let producer_admission = Arc::clone(&admission);
        let producer_invalidated = Arc::clone(&invalidated);
        let producer = thread::spawn(move || {
            let _ticket = try_admit(&producer_admission).unwrap();
            producer_ready.send(()).unwrap();
            producer_release.recv().unwrap();
            producer_invalidated.store(true, Ordering::Release);
        });
        ready.recv().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let writer_admission = Arc::clone(&admission);
        let writer_invalidated = Arc::clone(&invalidated);
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let mut output = BufWriter::new(File::create(&writer_path).unwrap());
            run_writer(
                &receiver,
                &mut output,
                &valid_test_config(),
                Instant::now() - Duration::from_secs(SESSION_SECONDS),
                &writer_invalidated,
                &writer_admission,
                &writer_path,
            );
        });
        let waiting = Instant::now();
        while admission.load(Ordering::Acquire) & ADMISSION_SEALED == 0 {
            assert!(
                waiting.elapsed() < Duration::from_secs(5),
                "writer did not seal admission"
            );
            thread::yield_now();
        }
        assert!(
            try_admit(&admission).is_none(),
            "new producer admitted after terminal seal"
        );
        release.send(()).unwrap();
        producer.join().unwrap();
        writer.join().unwrap();
        drop(sender);
        let end: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert!(matches!(
            end["outcome"].as_str(),
            Some("overflow_or_invalid_sample" | "producer_close_timeout")
        ));
        assert!(invalidated.load(Ordering::Acquire));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn wp087_terminal_seal_timeout_is_bounded_and_blocks_new_admission() {
        let admission = AtomicU64::new(0);
        let ticket = try_admit(&admission).unwrap();
        assert!(!seal_admission(&admission, Duration::ZERO));
        assert!(try_admit(&admission).is_none());
        drop(ticket);
        assert!(seal_admission(&admission, Duration::ZERO));
    }

    struct TerminalFailure {
        fail_flush: bool,
    }

    impl Write for TerminalFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_flush {
                Ok(bytes.len())
            } else {
                Err(io::Error::other("injected terminal write failure"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected terminal flush failure"))
        }
    }

    #[test]
    fn wp087_terminal_write_and_flush_failures_invalidate_capture() {
        for fail_flush in [false, true] {
            let mut output = BufWriter::with_capacity(1, TerminalFailure { fail_flush });
            let invalidated = AtomicBool::new(false);
            let end = EndRecord {
                record_type: "end",
                outcome: "completed",
                sample_count: 7_200,
                observed_at_us: 150_000_000,
                runtime_evidence: None,
            };
            let error = finish_output(&mut output, &end, &mut 0, &invalidated).unwrap_err();
            assert!(invalidated.load(Ordering::Acquire));
            assert!(error.to_string().contains(if fail_flush {
                "flush failure"
            } else {
                "write failure"
            }));
        }
    }

    #[test]
    fn wp087_admission_after_deadline_ignores_stale_update_timestamp() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let origin = Instant::now() - Duration::from_secs(SESSION_SECONDS);
        let capture = MatchBenchmarkCapture {
            origin,
            sender,
            invalidated: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(AtomicU64::new(0)),
            last_timestamp_us: AtomicU64::new(0),
        };
        assert_eq!(
            capture.observe_previous_frame(Some(0.001), origin + Duration::from_secs(31)),
            SampleResult::OutsideMeasurement
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    fn valid_test_config() -> CaptureConfig {
        let run_id = "run-1".to_string();
        CaptureConfig {
            schema_version: 1,
            run_id: run_id.clone(),
            state: RenderState::MatchDisabled,
            git_commit: "a".repeat(40),
            model_generation: "model-gen-1".to_string(),
            schema_generation: "schema-gen-1".to_string(),
            fixture_generation: "fixture-1".to_string(),
            cache_state: "warm".to_string(),
            input_script_sha256: "b".repeat(64),
            hardware_manifest_sha256: "c".repeat(64),
            display_profile_sha256: "d".repeat(64),
            power_mode: "balanced".to_string(),
            admission_counts: Some(AdmissionCounts {
                match_workers: 0,
                model_loads: 0,
                match_index_queries: 0,
            }),
            admission_evidence: Some(EvidenceReference {
                path: "receipts/admission.json".to_string(),
                sha256: "e".repeat(64),
            }),
            media_labels_workspace: None,
        }
    }

    #[test]
    fn wp087_benchmark_run_ids_are_safe_filenames() {
        assert!(valid_run_id("run-2026_09-a1"));
        for invalid in ["", "../escape", "a/b", "a\\b", ".", "a b", "a\n", "_starts"] {
            assert!(!valid_run_id(invalid), "accepted unsafe run id {invalid:?}");
        }
        assert!(!valid_run_id(&"a".repeat(65)));
    }

    #[test]
    fn wp087_benchmark_sha256_requires_lowercase_digest() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(!valid_sha256(&"0".repeat(63)));
    }

    #[test]
    fn wp087_benchmark_window_boundaries_are_exact() {
        let start = Duration::from_secs(WARMUP_SECONDS).as_micros() as u64;
        let end = Duration::from_secs(SESSION_SECONDS).as_micros() as u64;
        assert_eq!(start, 30_000_000);
        assert_eq!(end, 150_000_000);
        assert_eq!(end - start, u64::from(MEASURE_SECONDS) * 1_000_000);
        assert_eq!(sample_phase(start - 1), SamplePhase::Warmup);
        assert_eq!(sample_phase(start), SamplePhase::Measure);
        assert_eq!(sample_phase(end - 1), SamplePhase::Measure);
        assert_eq!(sample_phase(end), SamplePhase::Finished);
    }

    #[test]
    fn wp087_benchmark_sample_channel_overflow_fails_closed() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let invalidated = AtomicBool::new(false);
        sender.send(WriterMessage::Start).unwrap();
        let result = sender.try_send(WriterMessage::Frame(FrameRecord {
            record_type: "frame",
            frame_end_timestamp_us: 30_000_000,
            frame_duration_us: 1_000,
        }));
        if matches!(result, Err(TrySendError::Full(_))) {
            invalidated.store(true, Ordering::Release);
        }
        assert!(invalidated.load(Ordering::Acquire));
        assert!(matches!(receiver.try_recv(), Ok(WriterMessage::Start)));
    }

    #[test]
    fn wp087_benchmark_config_rejects_unknown_and_duplicate_fields() {
        let unknown = r#"{"schema_version":1,"run_id":"run-1","unexpected":true}"#;
        assert!(serde_json::from_str::<CaptureConfig>(unknown).is_err());
        let duplicate = r#"{"schema_version":1,"schema_version":1}"#;
        assert!(serde_json::from_str::<CaptureConfig>(duplicate).is_err());
    }

    #[test]
    fn wp087_benchmark_unknown_admission_is_preserved_as_unknown() {
        let mut config = valid_test_config();
        config.admission_counts = None;
        config.admission_evidence = None;
        assert!(validate_config(&config).is_ok());
        let header = serde_json::to_value(owned_header(&config).unwrap()).unwrap();
        assert!(header["admission_counts"].is_null());
        assert!(header["admission_evidence"].is_null());
    }

    #[test]
    fn wp087_benchmark_nonzero_declared_admission_does_not_claim_runtime_proof() {
        let mut config = valid_test_config();
        config.admission_counts.as_mut().unwrap().match_workers = 1;
        assert!(validate_config(&config).is_ok());
        let header = serde_json::to_value(owned_header(&config).unwrap()).unwrap();
        assert_eq!(header["admission_counts"]["match_workers"], 1);
    }

    #[test]
    fn wp087_benchmark_rejects_escaping_evidence_paths() {
        let workspace =
            std::env::temp_dir().join(format!("facial-match-benchmark-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&workspace).unwrap();
        let evidence = EvidenceReference {
            path: "../outside.json".to_string(),
            sha256: "a".repeat(64),
        };
        assert!(validate_evidence(&workspace, &evidence).is_err());
        fs::remove_dir(&workspace).unwrap();
    }
}
