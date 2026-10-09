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
        Arc, Mutex, OnceLock,
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
const MAX_PHASE_PROFILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SWAP_PHASE_PROFILE_BYTES: usize = 40 * 1024 * 1024;
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
static INPUT_INVALID: AtomicBool = AtomicBool::new(false);
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

fn media_label_input_invalid(pointer_present: bool, events: &[eframe::egui::Event]) -> bool {
    pointer_present
        || events.iter().any(|event| {
            matches!(
                event,
                eframe::egui::Event::Key { .. }
                    | eframe::egui::Event::PointerButton { .. }
                    | eframe::egui::Event::PointerMoved(_)
                    | eframe::egui::Event::PointerGone
                    | eframe::egui::Event::Text(_)
                    | eframe::egui::Event::Scroll(_)
                    | eframe::egui::Event::Zoom(_)
                    | eframe::egui::Event::Touch { .. }
                    | eframe::egui::Event::MouseWheel { .. }
            )
        })
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

fn phase_profile_requested() -> bool {
    phase_profile_opt_in(env::var_os("FACIAL_MEDIA_LABEL_PHASE_PROFILE").as_deref())
}

fn phase_profile_opt_in(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

fn swap_profile_requested() -> bool {
    phase_profile_opt_in(env::var_os("FACIAL_MEDIA_LABEL_PUFFIN_SWAP_PROFILE").as_deref())
}

#[derive(Clone, Copy, Serialize)]
struct SwapTiming {
    frame_number: u64,
    start_ns: i64,
    end_ns: i64,
    paint_marker_ns: i64,
    next_root_input_ns: i64,
    wall_us: u64,
    cpu_begin: ThreadCpuCounters,
    cpu_end: ThreadCpuCounters,
    cpu_delta: ThreadCpuCounters,
}

#[derive(Clone, Copy)]
struct SwapSample {
    timing: SwapTiming,
    begin: MarkerPoint,
    end: MarkerPoint,
}

const PUFFIN_GRAPH_MAX_BYTES: u64 = 32 * 1024 * 1024;

fn validate_puffin_graph(graph: &[u8], receipt: &[u8]) -> Result<String, String> {
    let proof: serde_json::Value =
        serde_json::from_slice(receipt).map_err(|_| "invalid graph receipt")?;
    let host = proof["host_triple"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or("graph receipt host missing")?;
    if !cfg!(all(windows, target_arch = "x86_64", target_env = "msvc"))
        || host != "x86_64-pc-windows-msvc"
    {
        return Err("swap diagnostic graph must match compiled Windows x64 MSVC target".into());
    }
    let args = serde_json::json!([
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--features",
        "media-label-puffin-profile",
        "--filter-platform",
        host
    ]);
    if proof["schema_version"] != 1
        || proof["command_args"] != args
        || proof["metadata_sha256"] != sha256_bytes(graph)
        || proof["cargo_manifest_sha256"] != sha256_bytes(include_bytes!("../Cargo.toml"))
        || proof["cargo_lock_sha256"] != sha256_bytes(CARGO_LOCK_BYTES)
        || proof["build_ui_sha256"] != sha256_bytes(include_bytes!("ui.rs"))
        || proof["build_lib_sha256"] != sha256_bytes(include_bytes!("lib.rs"))
        || proof["build_collector_sha256"] != sha256_bytes(include_bytes!("match_benchmark.rs"))
    {
        return Err("graph receipt differs from compiled source or command".into());
    }
    let graph: serde_json::Value =
        serde_json::from_slice(graph).map_err(|_| "invalid Cargo metadata")?;
    if graph["version"] != 1 {
        return Err("Cargo metadata format differs from inspected schema".into());
    }
    let packages = graph["packages"]
        .as_array()
        .filter(|items| items.len() <= 8192)
        .ok_or("graph packages missing or over bound")?;
    let nodes = graph["resolve"]["nodes"]
        .as_array()
        .filter(|items| items.len() <= 8192)
        .ok_or("graph resolution missing or over bound")?;
    let mut puffin_count = 0;
    let mut facial_count = 0;
    let mut framework_counts = [0; 3];
    let mut by_id = std::collections::HashMap::with_capacity(nodes.len());
    for node in nodes {
        let id = node["id"]
            .as_str()
            .filter(|value| value.len() <= 1024)
            .ok_or("graph node ID invalid")?;
        if by_id.insert(id, node).is_some() {
            return Err("duplicate graph node".into());
        }
    }
    let puffin_ids: Vec<_> = packages
        .iter()
        .filter(|package| package["name"] == "puffin")
        .filter_map(|package| package["id"].as_str())
        .filter(|id| by_id.contains_key(id))
        .collect();
    if puffin_ids.len() != 1 {
        return Err("graph puffin identity ambiguous".into());
    }
    for package in packages {
        let id = package["id"]
            .as_str()
            .filter(|value| value.len() <= 1024)
            .ok_or("graph package ID invalid")?;
        let Some(node) = by_id.get(id) else {
            continue;
        };
        let deps = node["deps"]
            .as_array()
            .filter(|items| items.len() <= 8192)
            .ok_or("graph dependencies missing or over bound")?;
        if deps
            .iter()
            .any(|dependency| dependency["pkg"] == puffin_ids[0])
        {
            let name = package["name"]
                .as_str()
                .ok_or("graph package name missing")?;
            if ![
                "facial",
                "eframe",
                "egui",
                "epaint",
                "egui-winit",
                "egui_glow",
                "egui-wgpu",
            ]
            .contains(&name)
            {
                return Err("uninspected puffin consumer could profile another thread".into());
            }
        }
        let features = node["features"]
            .as_array()
            .filter(|items| items.len() <= 256)
            .ok_or("graph features missing or over bound")?;
        if features
            .iter()
            .any(|feature| feature.as_str().is_none_or(|value| value.len() > 128))
        {
            return Err("graph feature invalid".into());
        }
        if package["name"] == "puffin" {
            puffin_count += 1;
            if package["version"] != "0.19.1" || features.iter().any(|feature| feature != "default")
            {
                return Err("puffin must be unique exact version with no features".into());
            }
        }
        if package["name"] == "epaint" && features.iter().any(|feature| feature == "rayon") {
            return Err("profiled background tessellation is forbidden".into());
        }
        if ["egui-winit", "egui_glow", "egui-wgpu"]
            .iter()
            .any(|name| package["name"] == *name)
            && package["version"] != "0.27.2"
        {
            return Err("integration differs from inspected pinned source".into());
        }
        for (index, name) in ["eframe", "egui", "epaint"].iter().enumerate() {
            if package["name"] == *name {
                framework_counts[index] += 1;
                if package["version"] != "0.27.2"
                    || !features.iter().any(|feature| feature == "puffin")
                {
                    return Err("profiled framework differs from inspected pinned source".into());
                }
                if *name == "eframe" && !features.iter().any(|feature| feature == "glow") {
                    return Err("graph lacks inspected Glow backend".into());
                }
            }
        }
        if package["name"] == "facial" {
            facial_count += 1;
            if package["version"] != env!("CARGO_PKG_VERSION")
                || !features
                    .iter()
                    .any(|feature| feature == "media-label-puffin-profile")
            {
                return Err("graph lacks selected Facial diagnostic feature".into());
            }
        }
    }
    if puffin_count != 1 || facial_count != 1 || framework_counts != [1; 3] {
        return Err("graph package identity is ambiguous".into());
    }
    Ok(proof["metadata_sha256"]
        .as_str()
        .ok_or("graph digest missing")?
        .to_owned())
}

fn prepare_swap_profile(workspace: &Path) -> Result<Option<String>, String> {
    if !swap_profile_requested() {
        return Ok(None);
    }
    #[cfg(not(feature = "media-label-puffin-profile"))]
    return Err("swap profiling requires media-label-puffin-profile build feature".into());
    #[cfg(feature = "media-label-puffin-profile")]
    {
        let evidence = workspace
            .parent()
            .ok_or("isolated run root missing")?
            .join("evidence");
        let read = |name: &str, bound| -> Result<Vec<u8>, String> {
            let path = env::var_os(name)
                .ok_or("swap profiling requires actual resolved graph and receipt")?;
            let supplied = Path::new(&path);
            if !supplied.is_absolute() {
                return Err("graph path must be absolute".into());
            }
            let mut component_path = PathBuf::new();
            for component in supplied.components() {
                if matches!(component, Component::ParentDir | Component::CurDir) {
                    return Err("graph path contains non-normal component".into());
                }
                component_path.push(component.as_os_str());
                if matches!(component, Component::Prefix(_)) {
                    continue;
                }
                let metadata =
                    fs::symlink_metadata(&component_path).map_err(|_| "graph path unavailable")?;
                if is_reparse_or_symlink(&metadata) {
                    return Err("graph path crosses reparse component".into());
                }
            }
            let canonical = fs::canonicalize(supplied).map_err(|_| "graph path unavailable")?;
            reject_reparse_components(&evidence, &canonical)?;
            read_bounded_regular_file(&canonical, bound, "swap feature proof")
        };
        let graph = read("FACIAL_MEDIA_LABEL_PUFFIN_GRAPH", PUFFIN_GRAPH_MAX_BYTES)?;
        let receipt = read("FACIAL_MEDIA_LABEL_PUFFIN_GRAPH_RECEIPT", CONFIG_MAX_BYTES)?;
        let digest = validate_puffin_graph(&graph, &receipt)?;
        let parent = workspace.join(".facial").join("benchmarks");
        reject_reparse_components(workspace, &parent)?;
        for (name, bytes) in [
            ("puffin-feature-graph.json", graph),
            ("puffin-feature-graph-receipt.json", receipt),
        ] {
            let path = parent.join(name);
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|_| "cannot create confined graph copy")?;
            reject_reparse_components(workspace, &path)?;
            file.write_all(&bytes)
                .map_err(|_| "cannot write confined graph copy")?;
            file.flush()
                .map_err(|_| "cannot flush confined graph copy")?;
        }
        swap_profiler::install()?;
        Ok(Some(digest))
    }
}

#[cfg(feature = "media-label-puffin-profile")]
mod swap_profiler {
    use super::*;
    use std::cell::RefCell;
    const ENDPOINT_LIMIT: usize = 32_768;
    const STREAM_BYTES: usize = 3 * 1024 * 1024;
    const SCOPE_LIMIT: usize = 16_384;
    const METADATA_LIMIT: usize = 512;
    struct State {
        origin: Instant,
        thread_id: u32,
        endpoints: Vec<(i64, MarkerPoint)>,
        swap_id: Option<puffin::ScopeId>,
        metadata_count: usize,
        lifetime_endpoints: u64,
        frame: Option<u64>,
        pending: Option<SwapSample>,
        failed: bool,
        failure_code: u8,
    }
    thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }
    pub(super) fn install() -> Result<(), String> {
        if puffin::are_scopes_on() {
            return Err("puffin was already active".into());
        }
        let point = marker_point(Instant::now(), current_thread_cpu_counters());
        if point.thread_id == 0 || point.cpu.is_err() {
            return Err("own GUI thread accounting unavailable".into());
        }
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                origin: point.at,
                thread_id: point.thread_id,
                endpoints: Vec::with_capacity(ENDPOINT_LIMIT),
                swap_id: None,
                metadata_count: 0,
                lifetime_endpoints: 0,
                frame: None,
                pending: None,
                failed: false,
                failure_code: 0,
            })
        });
        puffin::ThreadProfiler::initialize(clock, report);
        puffin::set_scopes_on(true);
        Ok(())
    }
    fn fail(state: &mut State, code: u8) {
        state.failed = true;
        if state.failure_code == 0 {
            state.failure_code = code;
        }
        puffin::set_scopes_on(false);
    }
    fn is_root_swap(detail: &puffin::ScopeDetails) -> bool {
        detail.scope_name.as_deref() == Some("swap_buffers")
            && detail.file_path
                == puffin::short_file_name("eframe-0.27.2/src/native/glow_integration.rs")
            && detail.function_name == "GlowWinitRunning::run_ui_and_paint"
            && detail.line_nr == 695
    }
    fn add_endpoint(state: &mut State, ns: i64, point: MarkerPoint) -> Result<(), ()> {
        let code = if state.failed {
            state.failure_code
        } else if state.endpoints.len() >= ENDPOINT_LIMIT || state.lifetime_endpoints >= 400_000_000
        {
            5
        } else if point.thread_id != state.thread_id {
            4
        } else if point.cpu.is_err() {
            2
        } else if state.endpoints.last().is_some_and(|last| last.0 >= ns) {
            1
        } else if state
            .endpoints
            .last()
            .is_some_and(|last| thread_cpu_delta(last.1.cpu.unwrap(), point.cpu.unwrap()).is_none())
        {
            3
        } else {
            0
        };
        if code != 0 {
            fail(state, code);
            return Err(());
        }
        state.lifetime_endpoints += 1;
        state.endpoints.push((ns, point));
        Ok(())
    }
    fn clock() -> i64 {
        STATE.with(|cell| {
            let mut storage = cell.borrow_mut();
            let Some(state) = storage.as_mut() else {
                return 0;
            };
            let at = Instant::now();
            let ns = i64::try_from(at.duration_since(state.origin).as_nanos()).unwrap_or(i64::MAX);
            if state.failed {
                return ns;
            }
            if state.endpoints.len() == ENDPOINT_LIMIT || state.lifetime_endpoints >= 400_000_000 {
                fail(state, 5);
                return ns;
            }
            if state.endpoints.last().is_some_and(|last| last.0 >= ns) {
                fail(state, 1);
                return ns;
            }
            let point = marker_point(at, current_thread_cpu_counters());
            let _ = add_endpoint(state, ns, point);
            ns
        })
    }
    fn report(
        _thread: puffin::ThreadInfo,
        details: &[puffin::ScopeDetails],
        stream: &puffin::StreamInfoRef<'_>,
    ) {
        STATE.with(|cell| {
            let mut storage = cell.borrow_mut();
            let Some(state) = storage.as_mut() else {
                return;
            };
            if state.failed {
                state.endpoints.clear();
                return;
            }
            if stream.stream.len() > STREAM_BYTES
                || stream.num_scopes > SCOPE_LIMIT
                || stream.depth > 64
                || state.metadata_count.saturating_add(details.len()) > METADATA_LIMIT
            {
                fail(state, 7);
                state.endpoints.clear();
                return;
            }
            state.metadata_count += details.len();
            for detail in details {
                if detail.scope_name.as_deref() == Some("swap_buffers") {
                    if !is_root_swap(detail) || state.swap_id.is_some() {
                        fail(state, 6);
                        break;
                    }
                    let mut collection = puffin::ScopeCollection::default();
                    collection.insert(Arc::new(detail.clone()));
                    state.swap_id = collection.fetch_by_name("swap_buffers").copied();
                }
            }
            if !state.failed {
                let bytes = puffin::Stream::from(stream.stream.to_vec());
                let mut count = 0;
                if visit(state, &bytes, 0, 0, &mut count).is_err() || count != stream.num_scopes {
                    fail(state, 8);
                }
            }
            state.endpoints.clear();
        });
    }
    fn visit(
        state: &mut State,
        stream: &puffin::Stream,
        offset: u64,
        depth: usize,
        count: &mut usize,
    ) -> Result<(), ()> {
        visit_until(state, stream, offset, stream.len() as u64, depth, count)
    }
    fn visit_until(
        state: &mut State,
        stream: &puffin::Stream,
        offset: u64,
        limit: u64,
        depth: usize,
        count: &mut usize,
    ) -> Result<(), ()> {
        if depth > 64 || offset > limit || limit > stream.len() as u64 {
            return Err(());
        }
        let mut next_offset = offset;
        let mut reader = puffin::Reader::with_offset(stream, offset).map_err(|_| ())?;
        while next_offset < limit {
            let scope = reader.next().ok_or(())?.map_err(|_| ())?;
            if scope.child_begin_position > scope.child_end_position
                || scope.child_end_position > limit
                || scope.next_sibling_position > limit
            {
                return Err(());
            }
            *count += 1;
            if *count > SCOPE_LIMIT {
                return Err(());
            }
            if Some(scope.id) == state.swap_id {
                let frame = state.frame.ok_or(())?;
                if state.pending.is_some() {
                    fail(state, 9);
                    return Err(());
                }
                if scope.record.duration_ns < 0 {
                    return Err(());
                }
                let end_ns = scope
                    .record
                    .start_ns
                    .checked_add(scope.record.duration_ns)
                    .ok_or(())?;
                let endpoint = |ns| {
                    state
                        .endpoints
                        .binary_search_by_key(&ns, |value| value.0)
                        .ok()
                        .map(|index| state.endpoints[index].1)
                        .ok_or(())
                };
                let begin = endpoint(scope.record.start_ns)?;
                let end = endpoint(end_ns)?;
                let (wall_us, cpu_delta) = marker_interval(begin, end).ok_or(())?;
                state.pending = Some(SwapSample {
                    begin,
                    end,
                    timing: SwapTiming {
                        frame_number: frame,
                        start_ns: scope.record.start_ns,
                        end_ns,
                        paint_marker_ns: 0,
                        next_root_input_ns: 0,
                        wall_us,
                        cpu_begin: begin.cpu.map_err(|_| ())?,
                        cpu_end: end.cpu.map_err(|_| ())?,
                        cpu_delta,
                    },
                });
            }
            if scope.child_end_position > scope.child_begin_position {
                visit_until(
                    state,
                    stream,
                    scope.child_begin_position,
                    scope.child_end_position,
                    depth + 1,
                    count,
                )?;
            }
            next_offset = scope.next_sibling_position;
        }
        if next_offset != limit {
            return Err(());
        }
        Ok(())
    }
    pub(super) fn set_frame(frame: u64) {
        STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                state.frame = Some(frame);
            }
        });
    }
    pub(super) fn take(frame: u64) -> Result<SwapSample, u8> {
        STATE.with(|cell| {
            let mut storage = cell.borrow_mut();
            let state = storage.as_mut().ok_or(10u8)?;
            if state.failed {
                return Err(state.failure_code);
            }
            let Some(sample) = state.pending.take() else {
                fail(state, 9);
                return Err(state.failure_code);
            };
            if sample.timing.frame_number != frame {
                fail(state, 10);
                return Err(state.failure_code);
            }
            Ok(sample)
        })
    }
    pub(super) fn stop() {
        puffin::set_scopes_on(false);
    }
    pub(super) fn bound_to_markers(
        mut sample: SwapSample,
        paint: FrameMarker,
        input: FrameMarker,
    ) -> Result<SwapSample, u8> {
        if paint.frame != sample.timing.frame_number
            || paint.frame.checked_add(1) != Some(input.frame)
            || marker_interval(paint.point, sample.begin).is_none()
            || marker_interval(sample.end, input.point).is_none()
        {
            return Err(11);
        }
        STATE.with(|cell| {
            let storage = cell.borrow();
            let state = storage.as_ref().ok_or(10u8)?;
            let ns = |at: Instant| {
                at.checked_duration_since(state.origin)
                    .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
                    .ok_or(11u8)
            };
            sample.timing.paint_marker_ns = ns(paint.point.at)?;
            sample.timing.next_root_input_ns = ns(input.point.at)?;
            Ok(sample)
        })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn state() -> State {
            State {
                origin: Instant::now(),
                thread_id: 1,
                endpoints: Vec::with_capacity(ENDPOINT_LIMIT),
                swap_id: Some(puffin::ScopeId(std::num::NonZeroU32::new(1).unwrap())),
                metadata_count: 0,
                lifetime_endpoints: 0,
                frame: Some(10),
                pending: None,
                failed: false,
                failure_code: 0,
            }
        }
        fn point(state: &State, ns: u64, cpu: u64) -> MarkerPoint {
            MarkerPoint {
                at: state.origin + Duration::from_nanos(ns),
                thread_id: 1,
                cpu: Ok(ThreadCpuCounters {
                    kernel_100ns: cpu,
                    user_100ns: cpu,
                }),
            }
        }
        #[test]
        fn wp087_phase_profile_puffin_swap_nested_pair_duplicate_missing_and_traversal() {
            let mut state = state();
            let begin = point(&state, 100, 1);
            let end = point(&state, 2_100, 3);
            add_endpoint(&mut state, 100, begin).unwrap();
            add_endpoint(&mut state, 2_100, end).unwrap();
            let mut stream = puffin::Stream::default();
            let outer = stream
                .begin_scope(
                    || 0,
                    puffin::ScopeId(std::num::NonZeroU32::new(2).unwrap()),
                    "",
                )
                .0;
            let offset = stream.begin_scope(|| 100, state.swap_id.unwrap(), "").0;
            stream.end_scope(offset, 2_100);
            stream.end_scope(outer, 3_000);
            let mut count = 0;
            visit(&mut state, &stream, 0, 0, &mut count).unwrap();
            assert_eq!(count, 2);
            let sample = state.pending.unwrap();
            assert_eq!(sample.timing.frame_number, 10);
            assert_eq!(sample.timing.wall_us, 2);
            assert_eq!(sample.timing.cpu_delta.kernel_100ns, 2);
            let marker_state = State {
                origin: state.origin,
                thread_id: 1,
                endpoints: Vec::new(),
                swap_id: None,
                metadata_count: 0,
                lifetime_endpoints: 0,
                frame: None,
                pending: None,
                failed: false,
                failure_code: 0,
            };
            STATE.with(|cell| *cell.borrow_mut() = Some(marker_state));
            let paint = FrameMarker {
                frame: 10,
                point: point(&state, 0, 0),
            };
            let input = FrameMarker {
                frame: 11,
                point: point(&state, 3_000, 4),
            };
            let paired = bound_to_markers(sample, paint, input).unwrap();
            assert_eq!(paired.timing.paint_marker_ns, 0);
            assert_eq!(paired.timing.next_root_input_ns, 3_000);
            assert!(bound_to_markers(
                sample,
                FrameMarker {
                    frame: 10,
                    point: input.point
                },
                input
            )
            .is_err());
            assert!(bound_to_markers(
                sample,
                paint,
                FrameMarker {
                    frame: 10,
                    point: input.point
                }
            )
            .is_err());
            STATE.with(|cell| *cell.borrow_mut() = None);
            assert!(visit(&mut state, &stream, 0, 0, &mut 0).is_err());
            state.pending = None;
            state.endpoints.clear();
            state.failed = false;
            state.failure_code = 0;
            assert!(visit(&mut state, &stream, 0, 0, &mut 0).is_err());
            assert!(visit(&mut state, &stream, 0, 65, &mut 0).is_err());
            let malformed = puffin::Stream::from(vec![b'(', 0]);
            assert!(visit(&mut state, &malformed, 0, 0, &mut 0).is_err());
            assert!(visit(&mut state, &stream, 0, 0, &mut SCOPE_LIMIT).is_err());
        }
        #[test]
        fn wp087_phase_profile_puffin_swap_clock_cpu_and_storage_fail_closed() {
            let mut baseline = state();
            let good = point(&baseline, 100, 4);
            add_endpoint(&mut baseline, 100, good).unwrap();
            let regression = point(&baseline, 200, 3);
            assert!(add_endpoint(&mut baseline, 200, regression).is_err());
            assert!(baseline.failed);
            assert_eq!(baseline.failure_code, 3);
            fail(&mut baseline, 8);
            assert_eq!(baseline.failure_code, 3);
            for failure in 0..4 {
                let mut state = state();
                let first = point(&state, 100, 4);
                add_endpoint(&mut state, 100, first).unwrap();
                let mut next = point(&state, 200, 5);
                let mut ns = 200;
                match failure {
                    0 => ns = 100,
                    1 => next.cpu = Err(()),
                    2 => next.thread_id = 2,
                    _ => state.lifetime_endpoints = 400_000_000,
                }
                assert!(add_endpoint(&mut state, ns, next).is_err());
                assert!(state.failed);
                assert_eq!(state.endpoints.len(), 1);
            }
            let mut bounded = state();
            let first = point(&bounded, 100, 4);
            bounded.endpoints.resize(ENDPOINT_LIMIT, (100, first));
            assert!(add_endpoint(&mut bounded, 200, first).is_err());
            assert_eq!(bounded.endpoints.len(), ENDPOINT_LIMIT);
            STATE.with(|cell| *cell.borrow_mut() = Some(state()));
            assert!(take(10).is_err());
            STATE.with(|cell| {
                let mut storage = cell.borrow_mut();
                let state = storage.as_mut().unwrap();
                let begin = point(state, 100, 4);
                state.pending = Some(SwapSample {
                    begin,
                    end: begin,
                    timing: SwapTiming {
                        frame_number: 10,
                        start_ns: 100,
                        end_ns: 100,
                        paint_marker_ns: 0,
                        next_root_input_ns: 0,
                        wall_us: 0,
                        cpu_begin: begin.cpu.unwrap(),
                        cpu_end: begin.cpu.unwrap(),
                        cpu_delta: ThreadCpuCounters::default(),
                    },
                });
            });
            assert!(take(11).is_err());
            STATE.with(|cell| *cell.borrow_mut() = None);
        }
        #[test]
        fn wp087_phase_profile_puffin_swap_metadata_exact_root_and_stream_bounds() {
            let root = puffin::short_file_name("eframe-0.27.2/src/native/glow_integration.rs");
            assert_ne!(root, "glow_integration.rs");
            assert_eq!(
                puffin::shorten_rust_function_name(
                    "eframe::native::glow_integration::GlowWinitRunning::run_ui_and_paint"
                ),
                "GlowWinitRunning::run_ui_and_paint"
            );
            let detail = puffin::ScopeDetails::from_scope_name("swap_buffers")
                .with_file(root)
                .with_function_name("GlowWinitRunning::run_ui_and_paint")
                .with_line_nr(695);
            assert!(is_root_swap(&detail));
            assert!(!is_root_swap(&detail.clone().with_line_nr(1482)));
            assert!(!is_root_swap(
                &detail.with_function_name("render_immediate_viewport")
            ));
            STATE.with(|cell| *cell.borrow_mut() = Some(state()));
            report(
                puffin::ThreadInfo {
                    start_time_ns: None,
                    name: String::new(),
                },
                &[],
                &puffin::StreamInfoRef {
                    stream: &[],
                    num_scopes: 0,
                    depth: 65,
                    range_ns: (0, 0),
                },
            );
            STATE.with(|cell| {
                assert!(cell.borrow().as_ref().unwrap().failed);
                *cell.borrow_mut() = None;
            });
            // Maximum encoded scope is 149-byte begin + 9-byte end. Clock disables new begins at its cap.
            assert!((ENDPOINT_LIMIT + 1) * 158 < 6 * 1024 * 1024);
        }
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct ThreadCpuCounters {
    kernel_100ns: u64,
    user_100ns: u64,
}

fn thread_cpu_delta(begin: ThreadCpuCounters, end: ThreadCpuCounters) -> Option<ThreadCpuCounters> {
    Some(ThreadCpuCounters {
        kernel_100ns: end.kernel_100ns.checked_sub(begin.kernel_100ns)?,
        user_100ns: end.user_100ns.checked_sub(begin.user_100ns)?,
    })
}

#[derive(Clone, Copy)]
struct MarkerPoint {
    at: Instant,
    cpu: Result<ThreadCpuCounters, ()>,
    thread_id: u32,
}

fn marker_point(at: Instant, cpu: Result<ThreadCpuCounters, ()>) -> MarkerPoint {
    #[cfg(windows)]
    let thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    #[cfg(not(windows))]
    let thread_id = if cfg!(test) { 1 } else { 0 };
    MarkerPoint { at, cpu, thread_id }
}

#[derive(Clone, Copy)]
struct FrameMarker {
    frame: u64,
    point: MarkerPoint,
}

#[derive(Default)]
struct PhaseMarkers {
    update_end: Option<FrameMarker>,
    paint: Option<FrameMarker>,
    input: Option<FrameMarker>,
    inconsistent: bool,
}

#[derive(Serialize)]
struct BetweenUpdatePhases {
    update_end_to_paint_marker_us: u64,
    paint_marker_to_next_root_input_us: u64,
    next_root_input_to_update_entry_us: u64,
    update_end_to_paint_marker_cpu: ThreadCpuCounters,
    paint_marker_to_next_root_input_cpu: ThreadCpuCounters,
    next_root_input_to_update_entry_cpu: ThreadCpuCounters,
    next_update_entry_cpu: ThreadCpuCounters,
}

fn marker_interval(begin: MarkerPoint, end: MarkerPoint) -> Option<(u64, ThreadCpuCounters)> {
    if begin.thread_id == 0 || begin.thread_id != end.thread_id {
        return None;
    }
    Some((
        elapsed_us(begin.at, end.at)?,
        thread_cpu_delta(begin.cpu.ok()?, end.cpu.ok()?)?,
    ))
}

impl PhaseMarkers {
    fn input(&mut self, frame: u64, point: MarkerPoint) {
        if self.input.is_some() {
            self.inconsistent = true;
            return;
        }
        self.input = Some(FrameMarker { frame, point });
    }
    fn paint(&mut self, frame: u64, point: MarkerPoint) {
        if self.paint.is_some() || self.update_end.is_none_or(|end| end.frame != frame) {
            self.inconsistent = true;
            return;
        }
        self.paint = Some(FrameMarker { frame, point });
    }
    fn finish(&mut self, frame: u64, point: MarkerPoint) {
        if self.update_end.is_some() {
            self.inconsistent = true;
            return;
        }
        self.update_end = Some(FrameMarker { frame, point });
    }
    fn take(
        &mut self,
        frame: u64,
        entry: MarkerPoint,
        first_frame: bool,
    ) -> Option<BetweenUpdatePhases> {
        let end = self.update_end.take();
        let paint = self.paint.take();
        let input = self.input.take();
        if first_frame {
            if end.is_some()
                || paint.is_some()
                || input
                    .is_none_or(|i| i.frame != frame || marker_interval(i.point, entry).is_none())
            {
                self.inconsistent = true;
            }
            return None;
        }
        let paired = (|| {
            let (end, paint, input) = (end?, paint?, input?);
            if end.frame.checked_add(1)? != frame
                || paint.frame != end.frame
                || input.frame != frame
            {
                return None;
            }
            let (update_end_to_paint_marker_us, update_end_to_paint_marker_cpu) =
                marker_interval(end.point, paint.point)?;
            let (paint_marker_to_next_root_input_us, paint_marker_to_next_root_input_cpu) =
                marker_interval(paint.point, input.point)?;
            let (next_root_input_to_update_entry_us, next_root_input_to_update_entry_cpu) =
                marker_interval(input.point, entry)?;
            Some(BetweenUpdatePhases {
                update_end_to_paint_marker_us,
                paint_marker_to_next_root_input_us,
                next_root_input_to_update_entry_us,
                update_end_to_paint_marker_cpu,
                paint_marker_to_next_root_input_cpu,
                next_root_input_to_update_entry_cpu,
                next_update_entry_cpu: entry.cpu.ok()?,
            })
        })();
        if paired.is_none() {
            self.inconsistent = true;
        }
        paired
    }
}

pub(crate) fn current_thread_cpu_counters() -> Result<ThreadCpuCounters, ()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentThread, GetThreadTimes},
        };
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // Only the calling thread's pseudo-handle is used; it is not owned or closed.
        if unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(());
        }
        let ticks = |value: FILETIME| {
            (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
        };
        Ok(ThreadCpuCounters {
            kernel_100ns: ticks(kernel),
            user_100ns: ticks(user),
        })
    }
    #[cfg(not(windows))]
    {
        Err(())
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct MediaLabelFramePhases {
    pub(crate) frame_number: u64,
    pub(crate) update_us: u64,
    pub(crate) render_ui_us: u64,
    pub(crate) tile_labels_us: u64,
    pub(crate) viewer_labels_us: u64,
    pub(crate) render_chrome_us: u64,
    pub(crate) media_prepare_us: u64,
    pub(crate) media_library_us: u64,
    pub(crate) media_viewer_panel_us: u64,
    pub(crate) media_finish_us: u64,
    render_other_us: u64,
    update_thread_cpu_begin: Option<ThreadCpuCounters>,
    update_thread_cpu_end: Option<ThreadCpuCounters>,
    update_thread_cpu_delta: Option<ThreadCpuCounters>,
}

fn remaining_render_us(phases: &MediaLabelFramePhases) -> Option<u64> {
    if phases.tile_labels_us > phases.media_library_us
        || phases.viewer_labels_us > phases.media_viewer_panel_us
    {
        return None;
    }
    let sum = [
        phases.render_chrome_us,
        phases.media_prepare_us,
        phases.media_library_us,
        phases.media_viewer_panel_us,
        phases.media_finish_us,
    ]
    .into_iter()
    .try_fold(0u64, |sum, value| sum.checked_add(value))?;
    phases.render_ui_us.checked_sub(sum)
}

#[derive(Serialize)]
struct MediaLabelPhaseRecord {
    frame_end_timestamp_us: u64,
    native_cpu_us: u64,
    outside_app_update_cpu_us: Option<u64>,
    #[serde(flatten)]
    phases: MediaLabelFramePhases,
    between_updates: Option<BetweenUpdatePhases>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_buffers: Option<SwapTiming>,
}

pub(crate) struct MediaLabelPhaseProfile {
    pub(crate) current: MediaLabelFramePhases,
    previous: Option<MediaLabelFramePhases>,
    records: Vec<MediaLabelPhaseRecord>,
    failed: bool,
    thread_cpu_read_failed: bool,
    thread_cpu_regressed: bool,
    render_phase_inconsistent: bool,
    exported: bool,
    workspace: PathBuf,
    run_id: String,
    markers: Arc<Mutex<PhaseMarkers>>,
    frame_marker_inconsistent: bool,
    swap_graph_sha256: Option<String>,
    swap_profile_inconsistent: bool,
    swap_failure_code: u8,
    swap_frozen: bool,
}

impl MediaLabelPhaseProfile {
    pub(crate) fn new(workspace: &Path, capture_present: bool) -> Result<Option<Self>, String> {
        if swap_profile_requested()
            && (!phase_profile_requested() || !media_label_mode() || !capture_present)
        {
            return Err("swap profiling requires validated native phase capture".into());
        }
        if !phase_profile_requested() || !media_label_mode() || !capture_present {
            return Ok(None);
        }
        let workspace = fs::canonicalize(workspace)
            .map_err(|error| format!("profile workspace root is unavailable: {error}"))?;
        let path = env::var_os(CONFIG_ENV).ok_or("profile requires capture config")?;
        let bytes = read_bounded_regular_file(
            Path::new(&path),
            CONFIG_MAX_BYTES,
            "profile capture config",
        )?;
        let config: CaptureConfig =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        validate_config(&config)?;
        if !config.state.is_media_labels()
            || !config_binding_matches(MEDIA_LABEL_CONFIG_SHA256.get(), &bytes)
        {
            return Err("profile requires validated native Media capture".into());
        }
        let swap_graph_sha256 = prepare_swap_profile(&workspace)?;
        Ok(Some(Self {
            current: MediaLabelFramePhases::default(),
            previous: None,
            records: Vec::with_capacity(20_000),
            failed: false,
            thread_cpu_read_failed: false,
            thread_cpu_regressed: false,
            render_phase_inconsistent: false,
            exported: false,
            workspace,
            run_id: config.run_id,
            markers: Arc::new(Mutex::new(PhaseMarkers::default())),
            frame_marker_inconsistent: false,
            swap_graph_sha256,
            swap_profile_inconsistent: false,
            swap_failure_code: 0,
            swap_frozen: false,
        }))
    }

    pub(crate) fn observe(
        &mut self,
        sampled: SampleResult,
        timestamp: u64,
        cpu: Option<f32>,
        frame_number: u64,
        entry_at: Instant,
        entry_cpu: Result<ThreadCpuCounters, ()>,
    ) {
        let mut swap_buffers = None;
        let between_updates = match self.markers.lock() {
            Ok(mut markers) => {
                #[cfg(feature = "media-label-puffin-profile")]
                if self.swap_graph_sha256.is_some() && !self.swap_frozen && self.previous.is_some()
                {
                    let paired = swap_profiler::take(self.previous.unwrap().frame_number).and_then(
                        |sample| match (markers.paint, markers.input) {
                            (Some(paint), Some(input)) if input.frame == frame_number => {
                                swap_profiler::bound_to_markers(sample, paint, input)
                            }
                            _ => Err(11),
                        },
                    );
                    match paired {
                        Ok(sample) => swap_buffers = Some(sample.timing),
                        Err(code) => {
                            self.failed = true;
                            self.swap_profile_inconsistent = true;
                            if self.swap_failure_code == 0 {
                                self.swap_failure_code = code;
                            }
                        }
                    }
                }
                let phases = markers.take(
                    frame_number,
                    marker_point(entry_at, entry_cpu),
                    self.previous.is_none(),
                );
                if markers.inconsistent {
                    self.failed = true;
                    self.frame_marker_inconsistent = true;
                }
                phases
            }
            Err(_) => {
                self.failed = true;
                self.frame_marker_inconsistent = true;
                None
            }
        };
        if sampled == SampleResult::Recorded {
            if self.records.len() >= 20_000 {
                self.failed = true;
            } else if let (Some(phases), Some(cpu)) = (self.previous, cpu) {
                let native_cpu_us = (f64::from(cpu) * 1_000_000.0).round() as u64;
                if phases.frame_number.checked_add(1) != Some(frame_number)
                    || !cpu.is_finite()
                    || cpu < 0.0
                    || native_cpu_us < phases.update_us
                    || phases.render_ui_us > phases.update_us
                    || phases
                        .tile_labels_us
                        .saturating_add(phases.viewer_labels_us)
                        > phases.render_ui_us
                    || sample_phase(timestamp) != SamplePhase::Measure
                    || self
                        .records
                        .last()
                        .is_some_and(|last| timestamp <= last.frame_end_timestamp_us)
                {
                    self.failed = true;
                }
                self.records.push(MediaLabelPhaseRecord {
                    frame_end_timestamp_us: timestamp,
                    native_cpu_us,
                    outside_app_update_cpu_us: native_cpu_us.checked_sub(phases.update_us),
                    phases,
                    between_updates,
                    swap_buffers,
                });
            } else {
                self.failed = true;
            }
        } else if sampled == SampleResult::Invalidated {
            self.failed = true;
        }
        if self.swap_graph_sha256.is_some() && sampled == SampleResult::OutsideMeasurement {
            self.swap_frozen = true;
            #[cfg(feature = "media-label-puffin-profile")]
            swap_profiler::stop();
        }
        self.current = MediaLabelFramePhases {
            frame_number,
            ..Default::default()
        };
    }

    pub(crate) fn finish_frame(
        &mut self,
        update_us: u64,
        cpu_begin: Result<ThreadCpuCounters, ()>,
        cpu_end: Result<ThreadCpuCounters, ()>,
        end_at: Instant,
    ) {
        self.current.update_us = update_us;
        if let Some(other) = remaining_render_us(&self.current) {
            self.current.render_other_us = other;
        } else {
            self.failed = true;
            self.render_phase_inconsistent = true;
        }
        self.current.update_thread_cpu_begin = cpu_begin.ok();
        self.current.update_thread_cpu_end = cpu_end.ok();
        self.current.update_thread_cpu_delta = self
            .current
            .update_thread_cpu_begin
            .zip(self.current.update_thread_cpu_end)
            .and_then(|(begin, end)| thread_cpu_delta(begin, end));
        if self.current.update_thread_cpu_delta.is_none() {
            self.failed = true;
            if self.current.update_thread_cpu_begin.is_none()
                || self.current.update_thread_cpu_end.is_none()
            {
                self.thread_cpu_read_failed = true;
            } else {
                self.thread_cpu_regressed = true;
            }
        }
        self.previous = Some(self.current);
        match self.markers.lock() {
            Ok(mut markers) => {
                markers.finish(self.current.frame_number, marker_point(end_at, cpu_end))
            }
            Err(_) => {
                self.failed = true;
                self.frame_marker_inconsistent = true;
            }
        }
    }

    pub(crate) fn root_input_marker(&mut self, frame: u64) {
        #[cfg(feature = "media-label-puffin-profile")]
        if self.swap_graph_sha256.is_some() && !self.swap_frozen {
            swap_profiler::set_frame(frame);
        }
        let at = Instant::now();
        let point = marker_point(at, current_thread_cpu_counters());
        match self.markers.lock() {
            Ok(mut markers) => markers.input(frame, point),
            Err(_) => {
                self.failed = true;
                self.frame_marker_inconsistent = true;
            }
        }
    }

    pub(crate) fn queue_paint_marker(&mut self, ctx: &eframe::egui::Context) {
        if ctx.viewport_id() != eframe::egui::ViewportId::ROOT {
            self.failed = true;
            self.frame_marker_inconsistent = true;
            return;
        }
        let markers = Arc::clone(&self.markers);
        let frame = self.current.frame_number;
        let callback = eframe::egui_glow::CallbackFn::new(move |_info, _painter| {
            let at = Instant::now();
            let point = marker_point(at, current_thread_cpu_counters());
            if let Ok(mut markers) = markers.lock() {
                markers.paint(frame, point);
            }
            // A poisoned mutex is observed and invalidated at next update, without UI panic.
        });
        ctx.layer_painter(eframe::egui::LayerId::new(
            eframe::egui::Order::Debug,
            eframe::egui::Id::new("media_phase_paint_marker"),
        ))
        .add(eframe::egui::PaintCallback {
            rect: ctx.screen_rect(),
            callback: Arc::new(callback),
        });
    }

    /// All file reads/serialization happen only after the collector interval ends.
    pub(crate) fn export_after_terminal(
        &mut self,
        capture: &MatchBenchmarkCapture,
    ) -> Result<(), String> {
        if self.exported || !capture.finished() || !capture.terminal_sealed() {
            return Ok(());
        }
        let raw_path = create_output_file(&self.workspace, &self.run_id)?;
        reject_reparse_components(&self.workspace, &raw_path)?;
        let raw = read_bounded_regular_file(&raw_path, MAX_JSONL_BYTES, "profile raw capture")?;
        let mut lines = raw
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        let header: serde_json::Value =
            serde_json::from_slice(lines.next().ok_or("profile raw header missing")?)
                .map_err(|error| error.to_string())?;
        let Some(last) = lines.last() else {
            return Ok(());
        };
        let terminal: serde_json::Value = match serde_json::from_slice(last) {
            Ok(value) => value,
            Err(_) => return Ok(()),
        };
        if terminal["record_type"] != "end" {
            return Ok(());
        }
        let complete = !self.failed
            && terminal["outcome"] == "completed"
            && header["run_id"] == self.run_id
            && header["diagnostic_only"] == true
            && terminal["sample_count"].as_u64() == Some(self.records.len() as u64)
            && self.records.len() >= 7_200;
        let path = raw_path
            .parent()
            .ok_or("profile output parent missing")?
            .join("media-label-phase-profile.json");
        reject_reparse_components(
            &self.workspace,
            path.parent().ok_or("profile output parent missing")?,
        )?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        reject_reparse_components(&self.workspace, &path)?;
        let byte_limit = if self.swap_graph_sha256.is_some() {
            MAX_SWAP_PHASE_PROFILE_BYTES
        } else {
            MAX_PHASE_PROFILE_BYTES
        };
        let document = serde_json::json!({ "schema_version": 1, "diagnostic_only": true,
            "acceptance_verdict": "not_canonical_acceptance_evidence",
            "outcome": if complete { "diagnostic_complete" } else { "incomplete_overflow_or_capture_error" },
            "source_identity": header, "raw_sha256": sha256_bytes(&raw), "terminal": terminal,
            "record_limit": 20_000, "record_count": self.records.len(),
            "thread_cpu_read_failed": self.thread_cpu_read_failed,
            "thread_cpu_regressed": self.thread_cpu_regressed,
            "render_phase_inconsistent": self.render_phase_inconsistent,
            "frame_marker_inconsistent": self.frame_marker_inconsistent,
            "swap_profile_inconsistent": self.swap_profile_inconsistent,
            "swap_failure_code": self.swap_failure_code,
            "swap_failure_code_legend": "0:none,1:clock,2:CPU_read,3:CPU_regression,4:thread,5:endpoint_limit,6:metadata_identity,7:stream_bound,8:parser,9:missing_or_duplicate_swap,10:frame_pair,11:outside_interval",
            "byte_limit": byte_limit,
            "swap_graph_sha256": self.swap_graph_sha256,
            "swap_scope": "supported_root_Glow_swap_buffers_wall_and_coarse_own_thread_CPU_only_not_GPU_time_or_whole_between_update_gap;all_enabled_scope_CPU_reads_perturb_diagnostic",
            "paint_marker_scope": "Debug_layer_CPU_paint_primitives_marker_not_guaranteed_last_among_Debug_layers_not_GPU_completion;callback_restores_backend_state_and_perturbs_timing",
            "residual_scope": "native_cpu_minus_paired_app_update_includes_eframe_egui_backend_os_not_exact_gl",
            "viewer_scope": "label_definition_assignment_clones_and_visible_chip_widgets",
            "timer_note": "opt_in_timers_can_affect_timing",
            "thread_cpu_note": "calling_thread_update_scope_cumulative_kernel_user_100ns_coarse_resolution_aggregate_only_not_per_frame_wall_attribution",
            "records": self.records });
        let bytes = serde_json::to_vec(&document).map_err(|error| error.to_string())?;
        if bytes.len() > byte_limit {
            return Err("phase profile exceeds diagnostic sidecar byte bound".into());
        }
        output
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
        self.exported = true;
        Ok(())
    }
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
    pub(crate) fn recorded_timestamp_us(&self) -> u64 {
        self.last_timestamp_us.load(Ordering::Acquire)
    }
    pub(crate) fn terminal_sealed(&self) -> bool {
        self.admission.load(Ordering::Acquire) == ADMISSION_SEALED
    }
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
            media_label_input_invalid(input.pointer.hover_pos().is_some(), &input.events)
        }) {
            INPUT_INVALID.store(true, Ordering::Release);
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
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_only: Option<bool>,
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
        diagnostic_only: (config.state.is_media_labels() && media_label_mode() && phase_profile_requested()).then_some(true),
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
                "input_valid": !INPUT_INVALID.load(Ordering::Acquire),
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
    #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
    fn wp087_phase_profile_puffin_swap_feature_graph_binding_is_fail_closed() {
        let names = ["facial", "puffin", "eframe", "egui", "epaint"];
        let mut graph = serde_json::json!({"version": 1, "packages": names.iter().map(|name| serde_json::json!({
            "id": name, "name": name, "version": match *name { "facial" => env!("CARGO_PKG_VERSION"), "puffin" => "0.19.1", _ => "0.27.2" }
        })).collect::<Vec<_>>(), "resolve": { "nodes": names.iter().map(|name| serde_json::json!({
            "id": name, "deps": if *name == "facial" { serde_json::json!([{"pkg":"puffin"}]) } else { serde_json::json!([]) },
            "features": match *name { "facial" => serde_json::json!(["media-label-puffin-profile"]), "puffin" => serde_json::json!(["default"]),
                "eframe" => serde_json::json!(["puffin", "glow"]), _ => serde_json::json!(["puffin"]) }
        })).collect::<Vec<_>>() }});
        let receipt = |bytes: &[u8]| {
            serde_json::json!({"schema_version":1,"metadata_sha256":sha256_bytes(bytes),
            "cargo_manifest_sha256":sha256_bytes(include_bytes!("../Cargo.toml")),"cargo_lock_sha256":sha256_bytes(CARGO_LOCK_BYTES),
            "build_ui_sha256":sha256_bytes(include_bytes!("ui.rs")),"build_lib_sha256":sha256_bytes(include_bytes!("lib.rs")),
            "build_collector_sha256":sha256_bytes(include_bytes!("match_benchmark.rs")),"host_triple":"x86_64-pc-windows-msvc",
            "command_args":["metadata","--format-version","1","--locked","--features","media-label-puffin-profile","--filter-platform","x86_64-pc-windows-msvc"]})
        };
        let bytes = serde_json::to_vec(&graph).unwrap();
        let valid = receipt(&bytes);
        assert!(validate_puffin_graph(&bytes, &serde_json::to_vec(&valid).unwrap()).is_ok());
        for key in [
            "metadata_sha256",
            "cargo_manifest_sha256",
            "cargo_lock_sha256",
            "host_triple",
        ] {
            let mut invalid = valid.clone();
            invalid[key] = serde_json::json!("wrong");
            assert!(validate_puffin_graph(&bytes, &serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        for (index, feature) in [(1, "packing"), (4, "rayon")] {
            let mut invalid = graph.clone();
            invalid["resolve"]["nodes"][index]["features"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(feature));
            let bytes = serde_json::to_vec(&invalid).unwrap();
            assert!(
                validate_puffin_graph(&bytes, &serde_json::to_vec(&receipt(&bytes)).unwrap())
                    .is_err()
            );
        }
        graph["resolve"]["nodes"][2]["id"] = serde_json::json!("unresolved");
        let bytes = serde_json::to_vec(&graph).unwrap();
        assert!(
            validate_puffin_graph(&bytes, &serde_json::to_vec(&receipt(&bytes)).unwrap()).is_err()
        );
    }

    fn phase_profile_fixture() -> MediaLabelPhaseProfile {
        MediaLabelPhaseProfile {
            current: MediaLabelFramePhases::default(),
            previous: None,
            records: Vec::with_capacity(20_000),
            failed: false,
            thread_cpu_read_failed: false,
            thread_cpu_regressed: false,
            render_phase_inconsistent: false,
            exported: false,
            workspace: env::temp_dir()
                .join(format!("facial-phase-profile-{}", uuid::Uuid::new_v4())),
            run_id: "phase-test".into(),
            markers: Arc::new(Mutex::new(PhaseMarkers::default())),
            frame_marker_inconsistent: false,
            swap_graph_sha256: None,
            swap_profile_inconsistent: false,
            swap_failure_code: 0,
            swap_frozen: false,
        }
    }

    fn finish_profile_frame(profile: &mut MediaLabelPhaseProfile) {
        profile.current.render_ui_us = 300;
        profile.current.render_chrome_us = 40;
        profile.current.media_prepare_us = 30;
        profile.current.media_library_us = 100;
        profile.current.media_viewer_panel_us = 50;
        profile.current.media_finish_us = 20;
        profile.finish_frame(
            400,
            Ok(ThreadCpuCounters {
                kernel_100ns: 100,
                user_100ns: 200,
            }),
            Ok(ThreadCpuCounters {
                kernel_100ns: 300,
                user_100ns: 500,
            }),
            Instant::now(),
        );
    }

    fn observe_profile_frame(profile: &mut MediaLabelPhaseProfile, timestamp: u64, frame: u64) {
        let entry = {
            let mut markers = profile.markers.lock().unwrap();
            let end = markers.update_end.unwrap();
            let cpu = end.point.cpu;
            let thread_id = end.point.thread_id;
            markers.paint(
                end.frame,
                MarkerPoint {
                    at: end.point.at + Duration::from_micros(1),
                    cpu,
                    thread_id,
                },
            );
            markers.input(
                frame,
                MarkerPoint {
                    at: end.point.at + Duration::from_micros(2),
                    cpu,
                    thread_id,
                },
            );
            end.point.at + Duration::from_micros(3)
        };
        profile.observe(
            SampleResult::Recorded,
            timestamp,
            Some(0.001),
            frame,
            entry,
            Ok(ThreadCpuCounters {
                kernel_100ns: 300,
                user_100ns: 500,
            }),
        );
    }

    #[test]
    fn wp087_phase_profile_pairs_previous_frame_and_bounds_records() {
        let mut profile = phase_profile_fixture();
        profile.current = MediaLabelFramePhases {
            frame_number: 0,
            update_us: 400,
            render_ui_us: 300,
            tile_labels_us: 40,
            viewer_labels_us: 20,
            ..Default::default()
        };
        finish_profile_frame(&mut profile);
        for frame in 1..=20_001 {
            observe_profile_frame(&mut profile, 30_000_000 + frame, frame);
            profile.current.render_ui_us = 300;
            profile.current.tile_labels_us = 40;
            profile.current.viewer_labels_us = 20;
            finish_profile_frame(&mut profile);
        }
        assert_eq!(profile.records.len(), 20_000);
        assert!(profile.failed);
        assert_eq!(profile.records[0].phases.frame_number, 0);
        assert_eq!(profile.records[0].native_cpu_us, 1_000);
        assert_eq!(profile.records[0].outside_app_update_cpu_us, Some(600));
        assert_eq!(profile.records[0].frame_end_timestamp_us, 30_000_001);
        assert_eq!(profile.records[0].phases.render_other_us, 60);
        assert_eq!(
            profile.records[0]
                .phases
                .update_thread_cpu_delta
                .unwrap()
                .kernel_100ns,
            200
        );
        assert_eq!(
            profile.records[0]
                .phases
                .update_thread_cpu_delta
                .unwrap()
                .user_100ns,
            300
        );
    }

    #[test]
    fn wp087_phase_profile_rejects_mismatched_frame_and_is_exact_opt_in() {
        assert!(!phase_profile_opt_in(None));
        assert!(!phase_profile_opt_in(Some(std::ffi::OsStr::new("true"))));
        assert!(!phase_profile_opt_in(Some(std::ffi::OsStr::new("01"))));
        assert!(phase_profile_opt_in(Some(std::ffi::OsStr::new("1"))));
        let mut profile = phase_profile_fixture();
        finish_profile_frame(&mut profile);
        observe_profile_frame(&mut profile, 30_000_001, 2);
        assert!(profile.failed);
    }

    #[test]
    fn wp087_phase_profile_marker_triplets_pair_and_fail_closed() {
        let at = Instant::now();
        let point = |offset, ticks| MarkerPoint {
            at: at + Duration::from_micros(offset),
            thread_id: 42,
            cpu: Ok(ThreadCpuCounters {
                kernel_100ns: ticks,
                user_100ns: ticks,
            }),
        };
        let valid = || {
            let mut markers = PhaseMarkers::default();
            markers.finish(8, point(0, 100));
            markers.paint(8, point(1, 200));
            markers.input(9, point(3, 300));
            markers
        };
        let mut markers = valid();
        let paired = markers.take(9, point(6, 400), false).unwrap();
        assert_eq!(paired.update_end_to_paint_marker_us, 1);
        assert_eq!(paired.paint_marker_to_next_root_input_us, 2);
        assert_eq!(paired.next_root_input_to_update_entry_us, 3);
        assert_eq!(paired.next_update_entry_cpu.kernel_100ns, 400);
        assert_eq!(paired.next_update_entry_cpu.user_100ns, 400);
        assert_eq!(paired.paint_marker_to_next_root_input_cpu.kernel_100ns, 100);
        assert!(!markers.inconsistent);
        assert!(markers.update_end.is_none() && markers.paint.is_none() && markers.input.is_none());
        assert!(markers.take(10, point(8, 500), false).is_none());
        assert!(markers.inconsistent);
        for mode in 0..9 {
            let mut markers = valid();
            match mode {
                0 => markers.paint = None,
                1 => markers.paint(8, point(2, 200)),
                2 => markers.input(9, point(4, 300)),
                3 => markers.paint.as_mut().unwrap().frame = 7,
                4 => markers.input.as_mut().unwrap().point.thread_id = 43,
                5 => markers.paint.as_mut().unwrap().point.cpu = Err(()),
                6 => markers.paint.as_mut().unwrap().point.cpu = Ok(ThreadCpuCounters::default()),
                7 => markers.input.as_mut().unwrap().point.at = at,
                _ => markers.update_end.as_mut().unwrap().frame = u64::MAX,
            }
            markers.take(9, point(6, 400), false);
            assert!(markers.inconsistent);
        }
        for mode in 0..4 {
            let mut markers = PhaseMarkers::default();
            if mode != 0 {
                markers.input(0, point(1, 100));
            }
            if mode == 2 {
                markers.finish(0, point(0, 100));
            }
            if mode == 3 {
                markers.paint = Some(FrameMarker {
                    frame: 0,
                    point: point(0, 100),
                });
            }
            assert!(markers.take(0, point(2, 100), true).is_none());
            assert_eq!(markers.inconsistent, mode != 1);
        }
        let mut profile = phase_profile_fixture();
        let markers = Arc::clone(&profile.markers);
        assert!(std::panic::catch_unwind(move || {
            let _guard = markers.lock().unwrap();
            panic!("owned test poisons diagnostic marker state");
        })
        .is_err());
        profile.observe(SampleResult::Warmup, 0, None, 0, at, Err(()));
        assert!(profile.failed && profile.frame_marker_inconsistent);
    }

    #[test]
    fn wp087_phase_profile_rejects_overlapping_phases_and_cpu_counter_failures() {
        let begin = ThreadCpuCounters {
            kernel_100ns: 100,
            user_100ns: 200,
        };
        let end = ThreadCpuCounters {
            kernel_100ns: 300,
            user_100ns: 500,
        };
        assert!(thread_cpu_delta(end, begin).is_none());
        assert!(thread_cpu_delta(
            begin,
            ThreadCpuCounters {
                kernel_100ns: 99,
                ..end
            }
        )
        .is_none());
        assert!(thread_cpu_delta(
            begin,
            ThreadCpuCounters {
                user_100ns: 199,
                ..end
            }
        )
        .is_none());
        let mut profile = phase_profile_fixture();
        finish_profile_frame(&mut profile);
        assert!(!profile.failed);
        profile.current.media_prepare_us = 301;
        profile.finish_frame(400, Ok(begin), Ok(end), Instant::now());
        assert!(profile.failed);
        assert!(profile.render_phase_inconsistent);
        let mut profile = phase_profile_fixture();
        finish_profile_frame(&mut profile);
        profile.current.tile_labels_us = 101;
        profile.finish_frame(400, Ok(begin), Ok(end), Instant::now());
        assert!(profile.failed);
        assert!(profile.render_phase_inconsistent);
        let mut profile = phase_profile_fixture();
        finish_profile_frame(&mut profile);
        profile.current.viewer_labels_us = 51;
        profile.finish_frame(400, Ok(begin), Ok(end), Instant::now());
        assert!(profile.failed);
        assert!(profile.render_phase_inconsistent);
        for (start, finish) in [
            (Err(()), Ok(end)),
            (Ok(begin), Err(())),
            (Ok(end), Ok(begin)),
        ] {
            let mut profile = phase_profile_fixture();
            finish_profile_frame(&mut profile);
            profile.finish_frame(400, start, finish, Instant::now());
            assert!(profile.failed);
            assert!(profile.current.update_thread_cpu_delta.is_none());
            assert_eq!(
                profile.thread_cpu_read_failed,
                start.is_err() || finish.is_err()
            );
            assert_eq!(
                profile.thread_cpu_regressed,
                start.is_ok() && finish.is_ok()
            );
            assert!(!profile.render_phase_inconsistent);
        }
        // Serialize the actual record fields with every number at its widest u64 value.
        fn widest_numbers(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Number(_) => *value = u64::MAX.into(),
                serde_json::Value::Object(fields) => {
                    for field in fields.values_mut() {
                        widest_numbers(field);
                    }
                }
                _ => {}
            }
        }
        let mut profile = phase_profile_fixture();
        finish_profile_frame(&mut profile);
        let mut record = serde_json::to_value(MediaLabelPhaseRecord {
            frame_end_timestamp_us: 1,
            native_cpu_us: 1,
            outside_app_update_cpu_us: Some(1),
            phases: profile.current,
            swap_buffers: Some(SwapTiming {
                frame_number: 0,
                start_ns: 0,
                end_ns: 1,
                paint_marker_ns: 0,
                next_root_input_ns: 1,
                wall_us: 1,
                cpu_begin: begin,
                cpu_end: begin,
                cpu_delta: begin,
            }),
            between_updates: Some(BetweenUpdatePhases {
                update_end_to_paint_marker_us: 1,
                paint_marker_to_next_root_input_us: 1,
                next_root_input_to_update_entry_us: 1,
                update_end_to_paint_marker_cpu: begin,
                paint_marker_to_next_root_input_cpu: begin,
                next_root_input_to_update_entry_cpu: begin,
                next_update_entry_cpu: begin,
            }),
        })
        .unwrap();
        widest_numbers(&mut record);
        let max_record_bytes = serde_json::to_vec(&record).unwrap().len() + 1;
        // 128 KiB reserves the bound raw-header/end source identity and document envelope.
        assert!(max_record_bytes * 20_000 + 128 * 1024 < MAX_SWAP_PHASE_PROFILE_BYTES);
        record.as_object_mut().unwrap().remove("swap_buffers");
        assert!(
            (serde_json::to_vec(&record).unwrap().len() + 1) * 20_000 + 128 * 1024
                < MAX_PHASE_PROFILE_BYTES
        );
    }

    #[test]
    fn wp087_phase_profile_export_requires_interval_end_and_sealed_capture() {
        let mut profile = phase_profile_fixture();
        let (sender, _receiver) = mpsc::sync_channel(1);
        let mut capture = MatchBenchmarkCapture {
            origin: Instant::now(),
            sender,
            invalidated: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(AtomicU64::new(ADMISSION_SEALED)),
            last_timestamp_us: AtomicU64::new(0),
        };
        profile.export_after_terminal(&capture).unwrap();
        assert!(!profile.workspace.exists());
        capture.origin = Instant::now() - Duration::from_secs(SESSION_SECONDS + 1);
        capture.admission.store(0, Ordering::Release);
        profile.export_after_terminal(&capture).unwrap();
        assert!(!profile.workspace.exists());
        assert!(!profile.exported);
    }

    #[test]
    fn wp087_phase_profile_exports_sealed_raw_to_fresh_confined_leaf_once() {
        let mut profile = phase_profile_fixture();
        let root = profile.workspace.clone();
        fs::create_dir(&root).unwrap();
        profile.workspace = fs::canonicalize(&root).unwrap();
        let raw_path = create_output_file(&profile.workspace, &profile.run_id).unwrap();
        let output_path = raw_path
            .parent()
            .unwrap()
            .join("media-label-phase-profile.json");
        assert!(!output_path.exists());
        let header = serde_json::json!({
            "record_type": "header", "run_id": profile.run_id, "diagnostic_only": true
        });
        let mut raw = serde_json::to_vec(&header).unwrap();
        raw.push(b'\n');
        profile.current.render_ui_us = 300;
        profile.current.tile_labels_us = 40;
        profile.current.viewer_labels_us = 20;
        finish_profile_frame(&mut profile);
        for frame in 1..=7_200 {
            let timestamp = 30_000_000 + frame;
            observe_profile_frame(&mut profile, timestamp, frame);
            profile.current.render_ui_us = 300;
            profile.current.tile_labels_us = 40;
            profile.current.viewer_labels_us = 20;
            finish_profile_frame(&mut profile);
            raw.extend(
                serde_json::to_vec(&serde_json::json!({
                    "record_type": "frame", "frame_end_timestamp_us": timestamp,
                    "frame_duration_us": 1_000
                }))
                .unwrap(),
            );
            raw.push(b'\n');
        }
        raw.extend(
            serde_json::to_vec(&serde_json::json!({
                "record_type": "end", "outcome": "completed", "sample_count": 7_200
            }))
            .unwrap(),
        );
        raw.push(b'\n');
        fs::write(&raw_path, &raw).unwrap();
        let (sender, _receiver) = mpsc::sync_channel(1);
        let capture = MatchBenchmarkCapture {
            origin: Instant::now() - Duration::from_secs(SESSION_SECONDS + 1),
            sender,
            invalidated: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(AtomicU64::new(ADMISSION_SEALED)),
            last_timestamp_us: AtomicU64::new(0),
        };
        profile.export_after_terminal(&capture).unwrap();
        let exported = fs::read(&output_path).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&exported).unwrap();
        assert!(profile.exported);
        assert_eq!(document["outcome"], "diagnostic_complete");
        assert_eq!(document["diagnostic_only"], true);
        assert_eq!(document["thread_cpu_read_failed"], false);
        assert_eq!(document["thread_cpu_regressed"], false);
        assert_eq!(document["render_phase_inconsistent"], false);
        assert_eq!(document["frame_marker_inconsistent"], false);
        assert_eq!(
            document["raw_sha256"],
            format!("{:x}", Sha256::digest(&raw))
        );
        assert_eq!(document["source_identity"], header);
        assert_eq!(document["terminal"]["sample_count"], 7_200);
        assert_eq!(document["record_count"], 7_200);
        let records = document["records"].as_array().unwrap();
        assert_eq!(records.len(), 7_200);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["frame_number"].as_u64(), Some(index as u64));
            assert_eq!(
                record["frame_end_timestamp_us"].as_u64(),
                Some(30_000_001 + index as u64)
            );
            assert_eq!(record["native_cpu_us"], 1_000);
            assert_eq!(record["update_us"], 400);
            assert_eq!(record["outside_app_update_cpu_us"], 600);
            assert_eq!(record["render_chrome_us"], 40);
            assert_eq!(record["media_prepare_us"], 30);
            assert_eq!(record["media_library_us"], 100);
            assert_eq!(record["media_viewer_panel_us"], 50);
            assert_eq!(record["media_finish_us"], 20);
            assert_eq!(record["render_other_us"], 60);
            assert_eq!(record["update_thread_cpu_begin"]["kernel_100ns"], 100);
            assert_eq!(record["update_thread_cpu_end"]["user_100ns"], 500);
            assert_eq!(record["update_thread_cpu_delta"]["kernel_100ns"], 200);
            assert_eq!(record["update_thread_cpu_delta"]["user_100ns"], 300);
            assert_eq!(
                record["between_updates"]["next_update_entry_cpu"]["kernel_100ns"],
                300
            );
            assert_eq!(
                record["between_updates"]["next_update_entry_cpu"]["user_100ns"],
                500
            );
            assert_eq!(
                record["between_updates"]["update_end_to_paint_marker_us"],
                1
            );
            assert_eq!(
                record["between_updates"]["paint_marker_to_next_root_input_us"],
                1
            );
            assert_eq!(
                record["between_updates"]["next_root_input_to_update_entry_us"],
                1
            );
        }
        profile.export_after_terminal(&capture).unwrap();
        assert_eq!(fs::read(&output_path).unwrap(), exported);
        // A different exporter cannot overwrite the existing confined leaf.
        profile.exported = false;
        let existing_error = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .unwrap_err();
        assert_eq!(existing_error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            profile.export_after_terminal(&capture).unwrap_err(),
            existing_error.to_string()
        );
        assert_eq!(fs::read(&output_path).unwrap(), exported);
        assert_eq!(fs::read(&raw_path).unwrap(), raw);
        assert_eq!(output_path.parent(), raw_path.parent());
        assert!(output_path.starts_with(&profile.workspace));
        fs::remove_dir_all(root).unwrap();
    }

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
    fn wp087_media_label_input_rejects_stationary_hover_and_pointer_transitions() {
        use eframe::egui::{Event, Pos2};
        assert!(!media_label_input_invalid(false, &[]));
        assert!(media_label_input_invalid(true, &[]));
        assert!(media_label_input_invalid(
            false,
            &[Event::PointerMoved(Pos2::ZERO)]
        ));
        assert!(media_label_input_invalid(false, &[Event::PointerGone]));
        assert!(media_label_input_invalid(
            false,
            &[Event::Text("mutate".into())]
        ));
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
