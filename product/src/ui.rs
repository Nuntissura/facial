use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::Path,
    path::PathBuf,
    process::Command as StdCommand,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

use eframe::egui::{
    self, Align, ColorImage, ScrollArea, Sense, TextEdit, TextureHandle, TextureOptions,
};
use egui_phosphor::regular as icons;
use gilrs::{EventType, GamepadId, Gilrs};

use crate::{
    api::{self, ApiPaths, Command as ApiCommand, CommandKind},
    folder_picker::{FolderPicker, PickerEvent},
    media_db::MediaDb,
    media_explorer::PreparedFolderEntries,
    media_tabs::{
        MediaTabFilter, MediaTabQueryMode, MediaTabSort, MediaTabViewMode, MediaTabsState,
        MEDIA_TABS_RECOVERY_SETTING_KEY, MEDIA_TABS_SETTING_KEY,
    },
    models::RunSummary,
    service::FacialService,
    theme,
};

/// Disk-first manual is loaded at runtime; this is the compiled-in fallback used
/// only when product/docs/MANUAL.md cannot be read.
const EMBEDDED_MANUAL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/MANUAL.md"));

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Media,
    Project,
    QualityIq,
    Identity,
    Duplicates,
    RunDebug,
    Manual,
    Compare,
    Options,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::Media,
        Tab::Project,
        Tab::QualityIq,
        Tab::Identity,
        Tab::Duplicates,
        Tab::RunDebug,
        Tab::Compare,
        Tab::Manual,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Media => "Media",
            Tab::Project => "Project",
            Tab::QualityIq => "Quality & IQ",
            Tab::Identity => "Identity",
            Tab::Duplicates => "Duplicates",
            Tab::RunDebug => "Run",
            Tab::Manual => "Manual",
            Tab::Compare => "Compare",
            Tab::Options => "Settings",
        }
    }

    /// Phosphor icon shown next to the tab label in the header strip.
    pub fn icon(self) -> &'static str {
        match self {
            Tab::Media => icons::FOLDERS,
            Tab::Project => icons::FOLDERS,
            Tab::QualityIq => icons::GAUGE,
            Tab::Identity => icons::USER_FOCUS,
            Tab::Duplicates => icons::COPY,
            Tab::RunDebug => icons::PLAY,
            Tab::Manual => icons::BOOK_OPEN,
            Tab::Compare => icons::COLUMNS,
            Tab::Options => icons::GEAR,
        }
    }

    pub fn vocab(self) -> &'static str {
        match self {
            Tab::Media => "media",
            Tab::Project => "project",
            Tab::QualityIq => "quality_iq",
            Tab::Identity => "identity",
            Tab::Duplicates => "duplicates",
            Tab::RunDebug => "run_debug",
            Tab::Manual => "manual",
            Tab::Compare => "compare",
            Tab::Options => "options",
        }
    }

    pub fn from_vocab(s: &str) -> Option<Tab> {
        match s {
            "media" => Some(Tab::Media),
            "project" => Some(Tab::Project),
            "quality_iq" => Some(Tab::QualityIq),
            "identity" => Some(Tab::Identity),
            "duplicates" => Some(Tab::Duplicates),
            "run_debug" => Some(Tab::RunDebug),
            "manual" => Some(Tab::Manual),
            "compare" | "lanes" => Some(Tab::Compare),
            "options" => Some(Tab::Options),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct CompareLane {
    id: usize,
    name: String,
    folder: String,
    files: Arc<Vec<String>>,
    /// Last complete persisted inventory generation loaded or committed for
    /// this lane. This is deliberately distinct from the per-batch UI content
    /// revision used to invalidate display/search work.
    inventory_generation: Option<u64>,
    index: usize,
    scanning: bool,
    loading_image: bool,
    loading_image_inflight: bool,
    recursive: bool,
    scan_id: u64,
    load_id: u64,
    image_error: String,
    scan_error: String,
    texture: Option<TextureHandle>,
    texture_size: Option<[usize; 2]>,
    image_path: String,
    pending_jump: String,
    pending_image_index: Option<usize>,
    selected_files: HashSet<usize>,
    selection_anchor: Option<usize>,
    action_message: String,
    media_filter: MediaFilterMode,
    /// A complete persisted generation is currently visible while this lane
    /// reconciles. Progressive batches stay off-screen until the scan proves
    /// complete, so an outage can never replace cached rows with a partial set.
    scan_using_cached_inventory: bool,
}

#[derive(Clone)]
struct MediaTabRuntimeInventory {
    files: Arc<Vec<String>>,
    inventory_generation: Option<u64>,
    /// Last display order published for this tab, as source indices into
    /// `files`. Re-published on activation so the grid paints in the same frame
    /// instead of showing an empty viewport until the display worker finishes
    /// its debounce, index build, and sort/rank round trip (WP-064).
    display: Arc<Vec<usize>>,
}

#[derive(Clone, Debug)]
struct PendingInlineVideoTarget {
    tab_id: String,
    path: String,
    path_key: String,
    source_index: usize,
    requested_scan_id: u64,
    checked_display_key: Option<MediaDisplayCacheKey>,
}

/// Bound on the per-tile canonical key cache (WP-069). Cleared wholesale rather
/// than evicted: it is a pure derived value, so rebuilding is cheap and correct.
const MAX_MEDIA_TILE_KEY_CACHE: usize = 200_000;
const MAX_MEDIA_TAB_RUNTIME_INVENTORIES: usize = 8;
const MAX_MEDIA_TAB_RUNTIME_ITEMS: usize = 1_000_000;

/// Interactions collected while a lane card renders, applied after the borrow
/// of the lane ends. Relative moves (`nav_delta`) broadcast to all lanes when
/// Sync is on; absolute jumps (`target_index`) always stay per-lane.
#[derive(Default)]
struct CompareLaneRenderRequest {
    browse: bool,
    scan: bool,
    nav_delta: Option<isize>,
    target_index: Option<usize>,
    open_index: Option<usize>,
    open_index_in_system: Option<usize>,
    open_path_in_system: Option<String>,
    open_file: bool,
    copy_selected: bool,
    copy_absolute_path: bool,
    copy_portable_path: bool,
    paste: bool,
    delete_selected: bool,
    open_location: bool,
    open_location_index: Option<usize>,
    /// Explicit folder-navigator request for the Media tab owner. The tab
    /// implementation consumes this path by creating/selecting a tab without
    /// mutating the currently active tab's viewport state.
    open_folder_in_new_tab: Option<String>,
    select_all: bool,
    select_none: bool,
    invert_selection: bool,
    open_selected: bool,
    // Media explorer extras (WP-045); never set from Compare surfaces.
    cut_selected: bool,
    rename_selected: bool,
    new_folder: bool,
    refresh: bool,
    /// (sort setting vocab, descending) from the context sort submenu.
    sort_to: Option<(crate::media_explorer::MediaSort, bool)>,
    /// Multi-selection label mutations apply explicitly to every selected file.
    add_label: Option<String>,
    remove_label: Option<String>,
    clear_labels: bool,
    toggle_favorite: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaFilterMode {
    ImagesOnly,
    VideosOnly,
    All,
}

impl MediaFilterMode {
    fn label(self) -> &'static str {
        match self {
            Self::ImagesOnly => "Images",
            Self::VideosOnly => "Videos",
            Self::All => "All",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::ImagesOnly => "img",
            Self::VideosOnly => "vid",
            Self::All => "all",
        }
    }
}

impl CompareLane {
    fn new(id: usize) -> Self {
        Self {
            id,
            name: String::new(),
            folder: String::new(),
            files: Arc::new(Vec::new()),
            inventory_generation: None,
            index: 0,
            scanning: false,
            loading_image: false,
            loading_image_inflight: false,
            recursive: true,
            scan_id: 0,
            load_id: 0,
            image_error: String::new(),
            scan_error: String::new(),
            texture: None,
            texture_size: None,
            image_path: String::new(),
            pending_jump: String::new(),
            pending_image_index: None,
            selected_files: HashSet::new(),
            selection_anchor: None,
            action_message: String::new(),
            // Facial is a media browser first: a selected folder exposes all
            // supported images and videos by default (WP-050).
            media_filter: MediaFilterMode::All,
            scan_using_cached_inventory: false,
        }
    }

    fn total(&self) -> usize {
        self.files.len()
    }
}

enum CompareWorkEvent {
    /// Last complete generation loaded off-thread before reconciliation.
    ScanCacheReady {
        lane_id: usize,
        scan_id: u64,
        inventory: crate::media_db::MediaInventory,
        display_order: Arc<Vec<usize>>,
        load_ms: u64,
    },
    ScanRootReady {
        lane_id: usize,
        scan_id: u64,
        identity: crate::media_io::RootIdentity,
    },
    /// Progressive scan batch: lets the Media front paint thumbnails before
    /// a huge recursive walk and final sort have completed (WP-050).
    ScanBatch {
        lane_id: usize,
        scan_id: u64,
        files: Vec<String>,
    },
    ScanDone {
        lane_id: usize,
        scan_id: u64,
        files: Vec<String>,
        display_order: Arc<Vec<usize>>,
        dir_errors: usize,
        elapsed_ms: u64,
        first_batch_ms: Option<u64>,
        inventory_generation: Option<u64>,
        inventory_write_ms: Option<u64>,
        inventory_error: Option<String>,
    },
    ScanError {
        lane_id: usize,
        scan_id: u64,
        error: String,
        elapsed_ms: u64,
        inventory_error: Option<String>,
    },
    ImageDone {
        lane_id: usize,
        load_id: u64,
        path: String,
        width: usize,
        height: usize,
        pixels: Vec<u8>,
    },
    ImageError {
        lane_id: usize,
        load_id: u64,
        path: String,
        error: String,
    },
    /// Anchor strip thumbnails decoded off-thread (WP-017): (file name, w, h, rgba).
    AnchorsLoaded {
        items: Vec<(String, usize, usize, Vec<u8>)>,
        error: Option<String>,
    },
    /// Per-file stat sweep for Modified/Size sorting (WP-044), keyed by the
    /// folder it was computed for so stale sweeps are dropped.
    MediaStatsDone {
        key: MediaStatRequestKey,
        stats: Arc<std::collections::HashMap<String, crate::media_explorer::FileStat>>,
        elapsed_ms: u64,
        failures: usize,
    },
    /// Immutable normalized search index built away from the render thread.
    MediaSearchIndexReady {
        key: MediaSearchIndexKey,
        index: Arc<crate::media_search::MediaSearchIndex>,
        elapsed_ms: u64,
    },
    /// Autocomplete candidates ranked off-thread against the immutable index.
    MediaSuggestionsDone {
        key: MediaSuggestionRequestKey,
        suggestions: Arc<Vec<crate::media_search::Suggestion>>,
        cancelled: bool,
    },
    /// Query/sort result for one exact UI request generation.
    MediaDisplayDone {
        key: MediaDisplayCacheKey,
        request_key: crate::media_search::SearchRequestKey,
        indices: Arc<Vec<usize>>,
        elapsed_ms: u64,
        scanned_rows: usize,
        matched_rows: usize,
        cancelled: bool,
    },
    /// Immediate child directories are enumerated away from the render
    /// thread. Arc keeps a 10k-folder result cheap to share across frames.
    MediaChildFoldersDone {
        key: MediaChildFolderRequestKey,
        folders: Arc<Vec<String>>,
        prepared: Arc<PreparedFolderEntries>,
        error: Option<String>,
    },
    /// CLIP engine finished loading off-thread (WP-047).
    ClipReady(Result<std::sync::Arc<crate::media_clip::ClipEngine>, String>),
    /// CLIP embedding index build progress/completion for a folder.
    ClipIndexProgress {
        key: ClipIndexRequestKey,
        done: usize,
        total: usize,
    },
    ClipIndexDone {
        key: ClipIndexRequestKey,
        indexed: usize,
        failed: usize,
        /// False when the build could not even open the index (lock held by
        /// another process) — the folder must NOT be marked indexed then
        /// (review round 3, finding 2).
        ok: bool,
    },
    /// Semantic query resolved: cosine-ranked paths for (query, folder).
    ClipQueryDone {
        key: ClipQueryRequestKey,
        indices: Vec<usize>,
        missing: usize,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaDisplayCacheKey {
    lane_id: usize,
    scan_id: u64,
    content_generation: u64,
    stats_generation: u64,
    semantic_generation: u64,
    meta_generation: u64,
    sort: crate::media_explorer::MediaSort,
    sort_desc: bool,
    query: String,
    search_mode: usize,
    /// WP-066: folder-only search scope participates in the display identity so
    /// toggling it invalidates only this tab's cached order.
    search_folder_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaSearchIndexKey {
    lane_id: usize,
    scan_id: u64,
    content_generation: u64,
    inventory_generation: Option<u64>,
    meta_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaSuggestionRequestKey {
    index_key: MediaSearchIndexKey,
    folder: String,
    query: String,
}

fn media_suggestion_result_is_current(
    key: &MediaSuggestionRequestKey,
    current_index_key: Option<&MediaSearchIndexKey>,
    loaded_index_key: Option<&MediaSearchIndexKey>,
    current_query: &str,
    current_folder: &str,
    cancelled: bool,
) -> bool {
    !cancelled
        && current_index_key == Some(&key.index_key)
        && loaded_index_key == Some(&key.index_key)
        && current_query == key.query
        && current_folder == key.folder
}

fn media_background_index_work_allowed(scanning: bool, scan_using_cached_inventory: bool) -> bool {
    !scanning || scan_using_cached_inventory
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaStatRequestKey {
    lane_id: usize,
    scan_id: u64,
    content_generation: u64,
    inventory_generation: Option<u64>,
    folder: String,
}

/// Bound how long a metadata sweep can retain a remote bulk-I/O lane before
/// re-entering the shared coordinator. A count bound remains deterministic
/// even when individual NAS metadata calls have variable latency.
const MEDIA_BACKGROUND_IO_SLICE: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MediaChildFolderRequestKey {
    lane_id: usize,
    scan_id: u64,
    root_identity: Option<crate::media_io::RootIdentity>,
    folder: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClipIndexRequestKey {
    lane_id: usize,
    scan_id: u64,
    content_generation: u64,
    folder: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClipQueryRequestKey {
    lane_id: usize,
    scan_id: u64,
    content_generation: u64,
    folder: String,
    query: String,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct MediaScanDiagnostics {
    lane_id: usize,
    scan_id: u64,
    status: String,
    root_key: Option<String>,
    root_kind: Option<crate::media_io::RootKind>,
    cached_items: usize,
    final_items: usize,
    first_batch_ms: Option<u64>,
    elapsed_ms: Option<u64>,
    dir_errors: usize,
    inventory_generation: Option<u64>,
    inventory_write_ms: Option<u64>,
    inventory_error: Option<String>,
    source_error: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct MediaQueryDiagnostics {
    status: String,
    index_rows: usize,
    index_elapsed_ms: u64,
    scanned_rows: usize,
    matched_rows: usize,
    query_elapsed_ms: u64,
    sort_elapsed_ms: u64,
    stat_elapsed_ms: u64,
    stat_failures: usize,
    queue_depth: usize,
    cancellations: u64,
    stale_drops: u64,
}

/// Single-source feature->tab mapping (auditable coverage of all 23 features).
/// "deepface:*"->Identity; "imagededup:*"->Duplicates;
/// "facet:duplicate_pass"/"facet:burst_blink_pass"->Duplicates;
/// "facet:diagnostics_pass"->RunDebug;
/// other "facet:*" + "python-ofiq:*" + "ediffiqa:*"->QualityIq;
/// UNKNOWN prefix -> RunDebug (fallback so nothing is hidden).
fn tab_for_feature(key: &str) -> Tab {
    let (plugin, feature) = match key.split_once(':') {
        Some((plugin, feature)) => (plugin, feature),
        None => return Tab::RunDebug,
    };
    match plugin {
        "deepface" => Tab::Identity,
        "imagededup" => Tab::Duplicates,
        "facet" => match feature {
            "duplicate_pass" | "burst_blink_pass" => Tab::Duplicates,
            "diagnostics_pass" => Tab::RunDebug,
            _ => Tab::QualityIq,
        },
        "python-ofiq" => Tab::QualityIq,
        "ediffiqa" => Tab::QualityIq,
        _ => Tab::RunDebug,
    }
}

#[derive(Clone)]
struct FeatureRow {
    key: String,
    display: String,
}

enum AppEvent {
    PipelineDone(Result<RunSummary, String>),
}

struct PendingModelSnapshot {
    command: ApiCommand,
    path: PathBuf,
    requested_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplicitVideoOwner {
    Preserve,
    Library,
    Viewer,
}

fn explicit_video_owner(action: &str) -> ExplicitVideoOwner {
    match action {
        "play_library" => ExplicitVideoOwner::Library,
        "play" => ExplicitVideoOwner::Viewer,
        _ => ExplicitVideoOwner::Preserve,
    }
}

fn video_surface_owner(active: Option<&str>, inline: Option<&str>) -> Option<&'static str> {
    active.map(|active| {
        if inline == Some(active) {
            "library"
        } else {
            "viewer"
        }
    })
}

/// A screenshot reply carries no request ID, so exactly one pending capture may
/// claim it. Modal backdrop captures (Settings or the folder navigator) take
/// precedence because they run earlier in the frame; the model snapshot only
/// owns the reply when no modal capture is in flight (WP-064 extends this to
/// the folder navigator, which was previously unrepresented and could silently
/// steal a receipt-backed capture).
/// The folder navigator is logically active from the moment it is requested,
/// including the pre-open backdrop-capture window during which its visible flag
/// is deliberately false. Commands must be accepted across that whole window so
/// navigator behavior never depends on screenshot-reply latency (WP-064).
fn folder_navigator_is_active(show_folder_navigator: bool, capture_pending: bool) -> bool {
    show_folder_navigator || capture_pending
}

/// Whether `path` lies within `folder`, comparing case-insensitively with
/// normalized separators. Used on folder change to decide whether an actively
/// playing video still belongs to the incoming inventory (WP-065). A recursive
/// scan keeps subfolder media, so this is a prefix test rather than a parent
/// equality test.
fn path_is_inside_folder(folder: &str, path: &str) -> bool {
    fn normalize(value: &str) -> String {
        value.replace('\\', "/").trim_end_matches('/').to_lowercase()
    }
    let folder = normalize(folder);
    if folder.is_empty() {
        return false;
    }
    let path = normalize(path);
    path.strip_prefix(&folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn model_snapshot_owns_screenshot(
    request_started: bool,
    settings_capture_pending: bool,
    folder_navigator_capture_pending: bool,
) -> bool {
    request_started && !settings_capture_pending && !folder_navigator_capture_pending
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceCaptureRegion {
    /// Complete native child geometry, including any portion outside the
    /// renderer framebuffer.
    full: [i32; 4],
    /// Intersection with the renderer framebuffer: left, top, width, height.
    visible: [u32; 4],
    /// Offset into a full-size LibVLC snapshot for the visible intersection.
    source_offset: [u32; 2],
}

fn current_surface_capture_region(
    surface: &crate::video_player::NativeSurfaceDiagnostics,
    framebuffer: [u32; 2],
) -> Result<Option<SurfaceCaptureRegion>, String> {
    if !surface.child_visible {
        return Ok(None);
    }
    if !surface.parent_valid || !surface.child_valid || !surface.child_parent_matches {
        return Err("visible native video surface failed parent/child validation".to_string());
    }
    if surface.libvlc_hwnd_matches != Some(true) {
        return Err(
            "visible native video surface is not attached to the LibVLC player".to_string(),
        );
    }
    let target = surface
        .target_bounds_px
        .ok_or_else(|| "visible native video surface has no requested bounds".to_string())?;
    let observed = surface
        .child_bounds_px
        .ok_or_else(|| "visible native video surface has no observed bounds".to_string())?;
    if target != observed {
        return Err(format!(
            "native video bounds mismatch: requested={target:?} observed={observed:?}"
        ));
    }
    let [x, y, width, height] = observed;
    if width <= 0 || height <= 0 {
        return Err(format!(
            "native video surface has invalid bounds {observed:?}"
        ));
    }

    let left = i64::from(x).clamp(0, i64::from(framebuffer[0]));
    let top = i64::from(y).clamp(0, i64::from(framebuffer[1]));
    let right = (i64::from(x) + i64::from(width)).clamp(0, i64::from(framebuffer[0]));
    let bottom = (i64::from(y) + i64::from(height)).clamp(0, i64::from(framebuffer[1]));
    if right <= left || bottom <= top {
        return Ok(None);
    }
    Ok(Some(SurfaceCaptureRegion {
        full: observed,
        visible: [
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        ],
        source_offset: [(left - i64::from(x)) as u32, (top - i64::from(y)) as u32],
    }))
}

pub struct FacialApp {
    service: Arc<Mutex<FacialService>>,
    config: crate::config::AppConfig,
    api_paths: ApiPaths,
    active_tab: Tab,
    manual_text: String,
    last_applied_action: Option<String>,
    last_receipt: Option<String>,
    project_name: String,
    worktree_path: String,
    in_place: bool,
    models: Vec<String>,
    worktree_view: BTreeMap<String, Vec<String>>,
    feature_rows: Vec<FeatureRow>,
    selected_features: HashSet<String>,
    last_import_images: Vec<String>,
    import_paths_input: String,
    import_summary: String,
    pipeline_status: String,
    workspace_status: String,
    run_output: String,
    run_summary: String,
    debug_lines: String,
    running_pipeline: bool,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    model_name: String,
    model_description: String,
    show_manual: bool,
    font_size_pt: f32,
    manual_scroll_target: Option<usize>,
    // Options sub-tab: false = Preferences, true = Advanced / Debug (WP-026).
    options_advanced: bool,
    // Manual TOC: index of the section last jumped to (highlighted in the sidebar). (WP-026)
    manual_current_section: usize,
    workspace_root: String,
    copy_location: String,
    sort_run_id: String,
    sort_in_parent: bool,
    sort_keep_dir: String,
    sort_review_dir: String,
    sort_cull_dir: String,
    sort_status: String,
    identity_model_path: String,
    identity_detector_path: String,
    identity_engine_status: String,
    compare_clipboard: Vec<String>,
    /// Text waiting to be forwarded through eframe's normal clipboard output.
    pending_system_clipboard: Option<String>,
    compare_action_message: String,
    compare_lanes: Vec<CompareLane>,
    compare_next_lane_id: usize,
    compare_sync: bool,
    compare_anchors_on: bool,
    compare_anchor_thumbs: Vec<(String, TextureHandle)>,
    compare_anchors_loading: bool,
    compare_anchor_error: String,
    compare_work_tx: Sender<CompareWorkEvent>,
    compare_work_rx: Receiver<CompareWorkEvent>,
    /// Per-lane cooperative cancellation tokens. Starting a newer scan stops
    /// the old NAS walk instead of merely dropping its eventual UI event.
    compare_scan_cancellations: HashMap<usize, Arc<AtomicBool>>,
    folder_picker: FolderPicker,
    media_search_query: String,
    media_search_mode: usize,
    /// Cached display order. Sorting/ranking 50k paths every immediate-mode
    /// frame made scrolling O(total files); this only rebuilds on data/query
    /// generations (WP-050).
    media_display_cache_key: Option<MediaDisplayCacheKey>,
    media_display_cache: Arc<Vec<usize>>,
    media_display_desired_key: Option<MediaDisplayCacheKey>,
    media_display_pending_since: Option<std::time::Instant>,
    media_display_inflight: Option<crate::media_search::SearchRequestKey>,
    media_search_index_key: Option<MediaSearchIndexKey>,
    media_search_index: Option<Arc<crate::media_search::MediaSearchIndex>>,
    media_search_index_inflight: Option<MediaSearchIndexKey>,
    media_search_index_cancel: Option<Arc<AtomicBool>>,
    media_suggestion_key: Option<MediaSuggestionRequestKey>,
    media_suggestions: Arc<Vec<crate::media_search::Suggestion>>,
    media_suggestion_inflight: Option<MediaSuggestionRequestKey>,
    media_suggestion_cancel: Option<Arc<AtomicBool>>,
    media_search_requests: crate::media_search::LatestSearchRequests,
    media_search_status: String,
    media_scan_diagnostics: MediaScanDiagnostics,
    media_query_diagnostics: MediaQueryDiagnostics,
    media_ui_frame_last_us: u64,
    media_ui_frame_max_us: u64,
    media_content_generation: u64,
    media_stats_generation: u64,
    media_semantic_generation: u64,
    media_stat_request: Option<MediaStatRequestKey>,
    media_stat_complete_key: Option<MediaStatRequestKey>,
    media_stat_cancel: Option<Arc<AtomicBool>>,
    /// Current-folder child list cache: folder enumeration must not run on
    /// every scroll frame.
    media_child_folder_cache: HashMap<String, Arc<Vec<String>>>,
    media_folder_entry_cache: HashMap<String, Arc<PreparedFolderEntries>>,
    media_child_folder_inflight: HashSet<MediaChildFolderRequestKey>,
    media_child_folder_cancel: HashMap<MediaChildFolderRequestKey, Arc<AtomicBool>>,
    /// Library / Viewer surface state (WP-044); layout persisted via `media_db`.
    media_explorer: crate::media_explorer::MediaExplorerState,
    /// Ordered, durable document tabs. Only the active tab is materialized in
    /// the single Media lane; scanners, caches, DB, and video remain shared.
    media_tabs: MediaTabsState,
    /// Empty after a valid load; otherwise exposes the rejected session record
    /// while the app safely operates a fresh default tab.
    media_tabs_load_status: String,
    /// Prevents a fallback default from overwriting an unrecoverable rejected
    /// primary value when the separate recovery write itself failed.
    media_tabs_persistence_blocked: bool,
    /// Canonical selection keys restored only after the active folder scan has
    /// published an inventory, so stale numeric indices can never retarget.
    media_tab_pending_selection_keys: Vec<String>,
    media_tab_pending_cursor_key: Option<String>,
    /// Bounded, session-local inventories let an already-open tab paint on the
    /// first frame after activation. Durable recovery still comes from the
    /// shared MediaDb inventory; the background scan reconciles this snapshot.
    media_tab_runtime_inventories: HashMap<String, MediaTabRuntimeInventory>,
    media_tab_runtime_inventory_lru: VecDeque<String>,
    /// Async thumbnail engine (WP-043); recreated on workspace switch.
    thumb_engine: Option<crate::media_thumbs::ThumbnailEngine>,
    /// Shared root-aware admission budget for scanner/stat/thumbnail work.
    media_io: Arc<crate::media_io::MediaIoCoordinator>,
    /// Resolved once by the active scan worker, never per thumbnail/frame.
    media_root_identity: Option<crate::media_io::RootIdentity>,
    media_root_source: Option<String>,
    /// Playback activity signal throttles app-owned background NAS work.
    media_playback_lease: Option<crate::media_io::PlaybackLease>,
    /// Uploaded thumbnail textures, count-capped LRU.
    thumb_textures: crate::media_thumbs::TextureLru<TextureHandle>,
    /// Optional LibVLC runtime. It stays unloaded until Play is pressed on a
    /// selected video, so image browsing and folder scans pay no VLC cost.
    video_player: crate::video_player::VideoPlayer,
    /// When set, the single shared LibVLC child is hosted by this visible grid
    /// tile instead of the Viewer panel. There are never per-tile decoders.
    media_inline_video_path: Option<String>,
    /// Whether the Library tile that owns the native video child actually
    /// rendered during the current frame. The Library panel is drawn before the
    /// Viewer, so the Viewer reads this to decide whether the Library really
    /// owns the surface this frame or has silently abandoned it (WP-065).
    media_inline_video_seen: bool,
    /// WP-066: per-tab "search this folder only" scope. Independent of the
    /// scan's recursive flag so the recursive inventory is retained.
    media_search_folder_only: bool,
    /// A search result activated into a new tab: selected once that tab's scan
    /// publishes an inventory containing it (WP-066).
    media_pending_result_selection: Option<String>,
    /// Structured outcome of the last search-scope change: (scan_unchanged,
    /// inventory_unchanged). Reported in media_tabs receipts so proving "scope
    /// never rescans" does not require parsing free text (audit finding I).
    media_last_scope_change: Option<(bool, bool)>,
    /// Path -> canonical DB key, cached so the render path does not recompute
    /// and allocate a key for every visible tile on every frame (WP-069).
    media_tile_key_cache: HashMap<String, String>,
    /// Briefly preserves a model/controller-requested Library placement while
    /// the virtual grid scrolls its target tile into the rendered range.
    media_inline_video_requested_at: Option<std::time::Instant>,
    /// Exact canonical target waiting for the asynchronously built display
    /// order. Numeric indices are deliberately not retained across inventory
    /// generations because they can identify a different file after rescan.
    media_inline_video_pending_target: Option<PendingInlineVideoTarget>,
    /// Resolved at startup/global refresh, never by the Media render loop.
    video_player_available: bool,
    /// In-memory cache over `media_db`, keyed by CANONICAL DB KEYS
    /// (`media_db.key_for`) so scan-path separator/casing variants can never
    /// split or clobber rows (WP-042 hardening).
    media_notes: Arc<BTreeMap<String, String>>,
    media_tags: Arc<BTreeMap<String, String>>,
    media_color_labels: Arc<BTreeMap<String, Vec<String>>>,
    /// Stable label IDs with operator-editable display names and backend hex.
    media_label_definitions: Vec<crate::media_db::ColorLabelDefinition>,
    /// Preparsed label colors refreshed only when catalog definitions change;
    /// visible badge/chip paint never scans the catalog or reparses hex.
    media_label_colors: Arc<HashMap<String, egui::Color32>>,
    media_label_usage_counts: BTreeMap<String, usize>,
    /// Shared create-label draft. `Some(key)` opens the inline creator in the
    /// Viewer panel and atomically assigns the new label to that asset.
    media_label_create_for_key: Option<String>,
    media_label_create_name: String,
    media_label_create_rgb: [u8; 3],
    /// Two-step guard for deleting a label that may be assigned to many files.
    media_label_delete_confirm: Option<String>,
    /// Favorites as (canonical_key, display_path), sorted by key.
    media_favorites: Vec<(String, String)>,
    /// Canonical keys of favorites for O(1) membership checks.
    media_favorite_keys: HashSet<String>,
    /// Canonical keys with unflushed note/tag/label edits (debounced
    /// write-through; failed writes are re-queued, never dropped).
    media_dirty_meta: HashSet<String>,
    media_meta_last_edit: Option<std::time::Instant>,
    /// redb-backed store (WP-042); favorites write through immediately.
    media_db: MediaDb,
    /// Deterministic paper-grain tile (WP-048); painted under every panel.
    grain: TextureHandle,
    /// Inspector-only paint aid; false in every live app construction.
    debug_preview_fixture: bool,
    /// Inspector-only count of visible-tile label-cache lookups. Live
    /// construction leaves the probe disabled (`None`), so production frames
    /// pay only one predictable option check.
    debug_label_paint_probe: Option<u64>,
    /// One-shot screenshot of the unobscured viewport, downsampled and
    /// Gaussian-blurred before Settings opens.
    settings_backdrop: Option<TextureHandle>,
    /// Settings remains closed while the renderer prepares the screenshot.
    settings_backdrop_requested_at: Option<std::time::Instant>,
    /// Folder navigator uses the exact same capture/downsample/blur pipeline
    /// as Settings, but owns a separate texture so closing one modal cannot
    /// invalidate the other surface's lifecycle.
    folder_navigator_backdrop: Option<TextureHandle>,
    folder_navigator_backdrop_requested_at: Option<std::time::Instant>,
    /// True when the clipboard holds a CUT (paste moves + clears sources).
    compare_clipboard_cut: bool,
    /// Inline rename editor: (source PATH, edit buffer). Keyed by path, not
    /// index — a rescan while the modal is open must not retarget it
    /// (review round 3, finding 3).
    media_rename: Option<(String, String)>,
    /// Inline new-folder editor buffer.
    media_new_folder: Option<String>,
    /// False only for the device-neutral visual inspector. Live GUI instances
    /// keep both gilrs/WGI and WinMM fallback acquisition enabled.
    controller_input_enabled: bool,
    controller_gilrs: Option<Gilrs>,
    controller_active: Option<GamepadId>,
    controller_legacy: crate::media_input::LegacyController,
    controller_legacy_active: bool,
    controller_input_source: String,
    controller_input_device_id: String,
    controller_input_device_name: String,
    /// Remappable action bindings (WP-046), persisted via the media DB.
    media_bindings: crate::media_input::BindingTable,
    /// Held-input repeat timing for controller navigation.
    media_repeat: crate::media_input::RepeatClock,
    /// Armed rebind capture (settings panel), if any.
    media_capture: Option<crate::media_input::Capture>,
    /// Fractional stick-scroll row accumulator.
    media_stick_accum: f32,
    /// Explicit couch pointer mode: right stick moves, A/B click.
    controller_pointer_mode: bool,
    controller_pointer_accum: [f32; 2],
    controller_pointer_left_down: bool,
    controller_pointer_right_down: bool,
    /// Rising-edge guard for the reserved Start/Menu app-switch action.
    controller_start_down: bool,
    /// Millisecond clock basis for repeat/capture timing.
    input_epoch: std::time::Instant,
    /// When set, the grid scrolls the cursor tile into view this frame.
    media_scroll_to_cursor: bool,
    /// Last controller poll instant (analog scroll integration dt).
    media_last_poll: Option<std::time::Instant>,
    /// One-shot: focus the media search box next frame (FocusSearch action).
    media_focus_search: bool,
    // ---- semantic search runtime (WP-047) ----
    /// Loaded CLIP engine (None while loading/absent; status explains).
    clip_engine: Option<std::sync::Arc<crate::media_clip::ClipEngine>>,
    /// One-line semantic status for the toolbar/settings (fallback reason,
    /// load/index progress, ready state).
    clip_status: String,
    clip_loading: bool,
    clip_indexing: bool,
    clip_index_request: Option<ClipIndexRequestKey>,
    clip_index_cancel: Option<Arc<AtomicBool>>,
    /// Folder whose index build last completed (triggers re-query).
    clip_indexed_folder: Option<String>,
    /// Resolved semantic ranking: (folder, query, lane file indices).
    media_semantic: Option<(String, String, Arc<Vec<usize>>)>,
    /// Query currently being embedded/ranked off-thread.
    media_semantic_inflight: Option<String>,
    clip_query_request: Option<ClipQueryRequestKey>,
    clip_query_cancel: Option<Arc<AtomicBool>>,
    /// Bumped on metadata edits so search caches invalidate.
    media_meta_generation: u64,
    /// Last semantic-query failure; spawns back off for a short window
    /// instead of retrying every frame (review round 3, finding 9).
    clip_query_backoff: Option<std::time::Instant>,
    /// Receipt-backed exact live-frame capture requested through `ui_snapshot`.
    /// The intent remains pending until the renderer returns its screenshot.
    pending_model_snapshot: Option<PendingModelSnapshot>,
}

impl FacialApp {
    pub fn new(cc: &eframe::CreationContext<'_>, service: FacialService) -> Self {
        let mut app = Self::new_with_ctx(&cc.egui_ctx, service);
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

            match cc.window_handle().map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => {
                    if let Err(error) = app.video_player.set_parent_window_handle(handle.hwnd.get())
                    {
                        eprintln!("native video parent binding failed: {error}");
                    }
                }
                Ok(_) => eprintln!("native video parent binding failed: non-Win32 window handle"),
                Err(error) => eprintln!("native video parent binding failed: {error}"),
            }
        }
        app
    }

    /// Construct against a bare `egui::Context` (no eframe window). Used by the
    /// live `new()` and by the headless GUI inspector (`ui_inspect`).
    pub fn new_with_ctx(ctx: &egui::Context, service: FacialService) -> Self {
        Self::new_with_ctx_and_media_db_root(ctx, service, None, true)
    }

    /// Inspector-only construction seam: keep the visible/runtime config intact
    /// while placing redb's exclusive-lock file in the snapshot workspace. This
    /// lets inspection run beside a live GUI without producing a false lock
    /// warning or contending with operator metadata.
    pub(crate) fn new_with_ctx_for_inspector(
        ctx: &egui::Context,
        service: FacialService,
        media_db_root: &Path,
    ) -> Self {
        // Visual inspection is intentionally device-neutral. Controller
        // acquisition has its own structured `controller-probe`; touching WGI
        // here can contend with an operator controller and has caused Windows
        // runtime aborts before the first headless frame.
        Self::new_with_ctx_and_media_db_root(ctx, service, Some(media_db_root), false)
    }

    fn new_with_ctx_and_media_db_root(
        ctx: &egui::Context,
        service: FacialService,
        media_db_root: Option<&Path>,
        initialize_controller: bool,
    ) -> Self {
        let in_place = service.ingest_in_place_default();
        let config = service.config().clone();
        // Identity stack (WP-015): Inter + icon font first, then the palette
        // for the configured mode, then text styles at the configured size.
        crate::theme::install_fonts(ctx);
        crate::theme::set_mode(crate::theme::mode_from_str(&config.theme_mode));
        crate::theme::install_style(ctx);
        crate::theme::apply_text_styles(ctx, config.font_size_pt);
        let api_paths = ApiPaths::from_config(&config);
        let _ = api_paths.ensure_dirs();
        let manual_text = Self::load_manual(&config.repo_root);
        let (tx, rx) = mpsc::channel();
        let (compare_work_tx, compare_work_rx) = mpsc::channel();
        let service_handle = Arc::new(Mutex::new(service));

        let show_manual = false;
        let config_font_size = config.font_size_pt;
        let config_copy_location = config
            .copy_location
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let config_workspace_root = config.workspace_root.to_string_lossy().to_string();
        let config_identity_model = config
            .identity_model_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let config_identity_detector = config
            .identity_detector_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut controller_legacy = crate::media_input::LegacyController::default();
        let initial_legacy_snapshot = initialize_controller
            .then(|| controller_legacy.poll())
            .flatten();
        let mut controller_gilrs = None;
        let mut controller_active = None;
        // A directly enumerated WinMM joystick is already a complete,
        // Steam-independent acquisition route. Do not enter WGI for that
        // device: affected HID stacks can block or abort inside WGI before a
        // fallback can run. WGI remains the route for WGI-only controllers.
        if crate::media_input::should_initialize_gilrs(
            initialize_controller,
            initial_legacy_snapshot.is_some(),
        ) {
            if let Ok(gilrs) = crate::media_input::new_controller_backend() {
                controller_active = gilrs.gamepads().next().map(|(id, _)| id);
                controller_gilrs = Some(gilrs);
            }
        }
        let controller_legacy_active = initial_legacy_snapshot.is_some();
        let controller_input_source = initial_legacy_snapshot
            .as_ref()
            .map(|snapshot| snapshot.source.clone())
            .unwrap_or_default();
        let controller_input_device_id = initial_legacy_snapshot
            .as_ref()
            .map(|snapshot| snapshot.device_id.clone())
            .unwrap_or_default();
        let controller_input_device_name = initial_legacy_snapshot
            .as_ref()
            .map(|snapshot| snapshot.device_name.clone())
            .unwrap_or_default();
        let media_db = MediaDb::open(media_db_root.unwrap_or(&config.workspace_root));
        let media_label_definitions = media_db.color_label_definitions();
        let media_label_colors = build_media_label_color_cache(&media_label_definitions);
        let grain = theme::grain_texture(ctx);
        let media_explorer = crate::media_explorer::MediaExplorerState::load(&media_db);
        let (mut media_tabs, media_tabs_load_status, media_tabs_persistence_blocked) =
            load_media_tabs_with_recovery(&media_db);
        if media_tabs.active().viewport.folder_key.is_empty() {
            let viewport = &mut media_tabs.active_mut().viewport;
            viewport.view_mode = match media_explorer.view_mode {
                crate::media_explorer::MediaViewMode::TwoPanel => MediaTabViewMode::LibraryViewer,
                crate::media_explorer::MediaViewMode::FullGrid => MediaTabViewMode::FullGrid,
            };
            viewport.split_ratio = media_explorer.split_ratio;
            viewport.tile_edge = media_explorer.tile_edge;
            viewport.show_names = media_explorer.show_names;
            viewport.strip_height = media_explorer.strip_height;
            viewport.sort = match media_explorer.sort {
                crate::media_explorer::MediaSort::Name => MediaTabSort::Name,
                crate::media_explorer::MediaSort::Modified => MediaTabSort::Modified,
                crate::media_explorer::MediaSort::Size => MediaTabSort::Size,
                crate::media_explorer::MediaSort::Created => MediaTabSort::Created,
            };
            viewport.sort_descending = media_explorer.sort_desc;
        }
        let video_player_available = crate::video_player::VideoPlayer::available();
        let media_io = Arc::new(crate::media_io::MediaIoCoordinator::new());
        let repaint_ctx = ctx.clone();
        let thumb_engine = Some(crate::media_thumbs::ThumbnailEngine::new_with_cache_cap(
            &config.workspace_root,
            config.media_thumb_cache_mb,
            Box::new(move || repaint_ctx.request_repaint()),
        ));
        let mut app = Self {
            service: Arc::clone(&service_handle),
            config,
            api_paths,
            active_tab: if show_manual { Tab::Manual } else { Tab::Media },
            manual_text,
            last_applied_action: None,
            last_receipt: None,
            project_name: "default-project".to_string(),
            worktree_path: "no worktree yet".to_string(),
            in_place,
            models: Vec::new(),
            worktree_view: BTreeMap::new(),
            feature_rows: Vec::new(),
            selected_features: HashSet::new(),
            last_import_images: Vec::new(),
            import_paths_input: String::new(),
            import_summary: "No import yet".to_string(),
            pipeline_status: "No feature run yet".to_string(),
            workspace_status: String::new(),
            run_output: "no run yet".to_string(),
            run_summary: String::new(),
            debug_lines: String::new(),
            running_pipeline: false,
            tx,
            rx,
            model_name: String::new(),
            model_description: String::new(),
            show_manual,
            font_size_pt: config_font_size,
            manual_scroll_target: None,
            options_advanced: false,
            manual_current_section: 0,
            compare_clipboard: Vec::new(),
            pending_system_clipboard: None,
            compare_action_message: String::new(),
            compare_lanes: vec![CompareLane::new(0), CompareLane::new(1)],
            compare_next_lane_id: 2,
            compare_sync: false,
            compare_anchors_on: false,
            compare_anchor_thumbs: Vec::new(),
            compare_anchors_loading: false,
            compare_anchor_error: String::new(),
            compare_work_tx,
            compare_work_rx,
            compare_scan_cancellations: HashMap::new(),
            folder_picker: FolderPicker::default(),
            media_search_query: String::new(),
            media_search_mode: 0,
            media_display_cache_key: None,
            media_display_cache: Arc::new(Vec::new()),
            media_display_desired_key: None,
            media_display_pending_since: None,
            media_display_inflight: None,
            media_search_index_key: None,
            media_search_index: None,
            media_search_index_inflight: None,
            media_search_index_cancel: None,
            media_suggestion_key: None,
            media_suggestions: Arc::new(Vec::new()),
            media_suggestion_inflight: None,
            media_suggestion_cancel: None,
            media_search_requests: crate::media_search::LatestSearchRequests::default(),
            media_search_status: String::new(),
            media_scan_diagnostics: MediaScanDiagnostics::default(),
            media_query_diagnostics: MediaQueryDiagnostics::default(),
            media_ui_frame_last_us: 0,
            media_ui_frame_max_us: 0,
            media_content_generation: 0,
            media_stats_generation: 0,
            media_semantic_generation: 0,
            media_stat_request: None,
            media_stat_complete_key: None,
            media_stat_cancel: None,
            media_child_folder_cache: HashMap::new(),
            media_folder_entry_cache: HashMap::new(),
            media_child_folder_inflight: HashSet::new(),
            media_child_folder_cancel: HashMap::new(),
            media_explorer,
            media_tabs,
            media_tabs_load_status,
            media_tabs_persistence_blocked,
            media_tab_pending_selection_keys: Vec::new(),
            media_tab_pending_cursor_key: None,
            media_tab_runtime_inventories: HashMap::new(),
            media_tab_runtime_inventory_lru: VecDeque::new(),
            thumb_engine,
            media_io,
            media_root_identity: None,
            media_root_source: None,
            media_playback_lease: None,
            thumb_textures: crate::media_thumbs::TextureLru::new(512),
            video_player: crate::video_player::VideoPlayer::default(),
            media_inline_video_path: None,
            media_inline_video_seen: false,
            media_search_folder_only: false,
            media_pending_result_selection: None,
            media_last_scope_change: None,
            media_tile_key_cache: HashMap::new(),
            media_inline_video_requested_at: None,
            media_inline_video_pending_target: None,
            video_player_available,
            media_notes: Arc::new(BTreeMap::new()),
            media_tags: Arc::new(BTreeMap::new()),
            media_color_labels: Arc::new(BTreeMap::new()),
            media_label_definitions,
            media_label_colors,
            media_label_usage_counts: BTreeMap::new(),
            media_label_create_for_key: None,
            media_label_create_name: String::new(),
            media_label_create_rgb: [70, 130, 196],
            media_label_delete_confirm: None,
            media_favorites: Vec::new(),
            media_favorite_keys: HashSet::new(),
            media_dirty_meta: HashSet::new(),
            media_meta_last_edit: None,
            media_db,
            grain,
            debug_preview_fixture: false,
            debug_label_paint_probe: None,
            settings_backdrop: None,
            settings_backdrop_requested_at: None,
            folder_navigator_backdrop: None,
            folder_navigator_backdrop_requested_at: None,
            compare_clipboard_cut: false,
            media_rename: None,
            media_new_folder: None,
            controller_input_enabled: initialize_controller,
            controller_gilrs,
            controller_active,
            controller_legacy,
            controller_legacy_active,
            controller_input_source,
            controller_input_device_id,
            controller_input_device_name,
            media_bindings: crate::media_input::BindingTable::default(),
            media_repeat: crate::media_input::RepeatClock::default(),
            media_capture: None,
            media_stick_accum: 0.0,
            controller_pointer_mode: false,
            controller_pointer_accum: [0.0, 0.0],
            controller_pointer_left_down: false,
            controller_pointer_right_down: false,
            controller_start_down: false,
            input_epoch: std::time::Instant::now(),
            media_scroll_to_cursor: false,
            media_last_poll: None,
            media_focus_search: false,
            clip_engine: None,
            clip_status: String::new(),
            clip_loading: false,
            clip_indexing: false,
            clip_index_request: None,
            clip_index_cancel: None,
            clip_indexed_folder: None,
            media_semantic: None,
            media_semantic_inflight: None,
            clip_query_request: None,
            clip_query_cancel: None,
            media_meta_generation: 0,
            clip_query_backoff: None,
            pending_model_snapshot: None,
            workspace_root: config_workspace_root,
            copy_location: config_copy_location,
            sort_run_id: String::new(),
            sort_in_parent: false,
            sort_keep_dir: String::new(),
            sort_review_dir: String::new(),
            sort_cull_dir: String::new(),
            sort_status: "No sort yet".to_string(),
            identity_model_path: config_identity_model,
            identity_detector_path: config_identity_detector,
            identity_engine_status: String::new(),
        };
        app.load_media_metadata();
        app.load_media_bindings();
        app.materialize_active_media_tab();
        let _ = app.video_player.set_loop(app.media_explorer.video_loop);
        app.start_clip_engine_load();

        if let Ok(mut svc) = service_handle.lock() {
            svc.refresh_plugins();
            app.models = Self::load_models(&mut svc);
            app.worktree_view = Self::load_worktrees(&mut svc);
            app.feature_rows = Self::load_features(&mut svc);
            app.selected_features.clear();
            app.debug_lines = format!("initialized {}\n", chrono::Utc::now());
        } else {
            app.debug_lines = "initialized (service lock unavailable)\n".to_string();
        }
        app
    }

    /// Disk-first manual loader with compiled-in fallback.
    fn load_manual(repo_root: &std::path::Path) -> String {
        std::fs::read_to_string(repo_root.join("product/docs/MANUAL.md"))
            .unwrap_or_else(|_| EMBEDDED_MANUAL.to_string())
    }

    fn load_models(service: &mut FacialService) -> Vec<String> {
        service
            .list_models()
            .into_iter()
            .map(|model| format!("{} [{}] - {}", model.name, model.id, model.status))
            .collect()
    }

    fn load_worktrees(service: &mut FacialService) -> BTreeMap<String, Vec<String>> {
        let mut worktree_view = BTreeMap::new();
        for (project, runs) in service.list_worktrees() {
            worktree_view.insert(
                project,
                runs.into_iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect(),
            );
        }
        worktree_view
    }

    fn load_features(service: &mut FacialService) -> Vec<FeatureRow> {
        let mut feature_rows = Vec::new();
        for plugin in service.list_plugins() {
            let plugin_id = plugin
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let plugin_name = plugin
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or(&plugin_id)
                .to_string();
            if let Some(features) = plugin.get("features").and_then(|value| value.as_array()) {
                for feature in features {
                    let feature_id = feature
                        .get("id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if feature_id.is_empty() {
                        continue;
                    }
                    let feature_name = feature
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or(feature_id);
                    feature_rows.push(FeatureRow {
                        key: format!("{plugin_id}:{feature_id}"),
                        display: format!("{plugin_name} :: {feature_name} [{feature_id}]"),
                    });
                }
            }
        }
        feature_rows.sort_by(|a, b| a.display.cmp(&b.display));
        feature_rows
    }

    fn compare_lane_position(&self, lane_id: usize) -> Option<usize> {
        self.compare_lanes
            .iter()
            .position(|lane| lane.id == lane_id)
    }

    fn add_compare_lane(&mut self) {
        if self.compare_lanes.len() >= 16 {
            return;
        }
        self.compare_lanes
            .push(CompareLane::new(self.compare_next_lane_id));
        self.compare_next_lane_id += 1;
    }

    fn remove_compare_lane(&mut self) {
        if self.compare_lanes.len() > 1 {
            if let Some(lane) = self.compare_lanes.pop() {
                if let Some(cancel) = self.compare_scan_cancellations.remove(&lane.id) {
                    cancel.store(true, Ordering::Release);
                }
            }
        }
    }

    fn set_compare_lane_count(&mut self, target: usize) {
        let target = target.clamp(1, 16);
        while self.compare_lanes.len() < target {
            self.add_compare_lane();
        }
        while self.compare_lanes.len() > target {
            self.remove_compare_lane();
        }
    }

    fn clone_last_compare_lane_setup(&mut self) {
        if self.compare_lanes.len() >= 16 {
            return;
        }
        let Some(source) = self.compare_lanes.last().cloned() else {
            self.add_compare_lane();
            return;
        };
        let mut lane = CompareLane::new(self.compare_next_lane_id);
        let base_name = if source.name.trim().is_empty() {
            format!("Pane {}", source.id + 1)
        } else {
            source.name
        };
        lane.name = format!("{base_name} copy");
        lane.folder = source.folder;
        lane.recursive = source.recursive;
        self.compare_lanes.push(lane);
        self.compare_next_lane_id += 1;
    }

    fn start_compare_scan(&mut self, lane_id: usize) {
        self.start_compare_scan_internal(lane_id, false);
    }

    fn start_compare_scan_internal(&mut self, lane_id: usize, preserve_cached_inventory: bool) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let (folder, recursive, media_filter, using_runtime_inventory) = {
            let lane = &mut self.compare_lanes[pos];
            let trimmed = sanitize_folder_input(&lane.folder);
            if trimmed.is_empty() {
                lane.scan_error = "Set a folder path first.".to_string();
                return;
            }
            // Reflect the cleaned path back so the field shows what was scanned
            // (drops the surrounding quotes Windows "Copy as path" adds).
            lane.folder = trimmed.clone();
            // Auto-label an unnamed lane with the folder's leaf so multiple lanes
            // stay distinguishable; the operator can still rename it.
            if lane.name.trim().is_empty() {
                if let Some(leaf) = Path::new(&trimmed).file_name().and_then(|s| s.to_str()) {
                    lane.name = leaf.to_string();
                }
            }
            lane.scan_id = lane.scan_id.saturating_add(1);
            lane.scanning = true;
            lane.scan_error.clear();
            // WP-064: a restored viewport is worth keeping visible even when its
            // scan never committed an inventory generation (interrupted scan, or
            // any folder with one unreadable subdirectory). Reconciliation still
            // runs; this only decides whether the operator stares at a blank
            // grid while it does.
            let using_runtime_inventory = preserve_cached_inventory && !lane.files.is_empty();
            if !using_runtime_inventory {
                lane.loading_image = false;
                lane.loading_image_inflight = false;
                lane.pending_image_index = None;
                lane.image_error.clear();
                lane.image_path.clear();
                lane.selected_files.clear();
                lane.selection_anchor = None;
            }
            lane.action_message.clear();
            if !using_runtime_inventory {
                // Swap instead of Arc::make_mut: an obsolete cancellable worker
                // may still own the prior generation briefly, and cloning 141k
                // rows here would stall the UI before that worker observes cancel.
                lane.files = Arc::new(Vec::new());
                lane.inventory_generation = None;
                lane.index = 0;
                lane.texture = None;
                lane.texture_size = None;
            }
            (
                trimmed,
                lane.recursive,
                lane.media_filter,
                using_runtime_inventory,
            )
        };
        self.compare_lanes[pos].scan_using_cached_inventory = using_runtime_inventory;
        // WP-065: a folder change must make an explicit decision about active
        // playback. This path previously never touched the player or the inline
        // placement state, so a video kept decoding while its owning tile was
        // discarded with the old inventory and the pending-placement target was
        // dropped on the scan-id bump. Nothing then placed the surface: audio
        // continued with no picture, or the last frame stayed on screen. Video
        // that belongs to the outgoing folder is stopped; a video that still
        // exists in the incoming inventory is re-placed by the normal owners.
        if !using_runtime_inventory {
            let active_video = self.video_player.active_path().map(|path| path.to_string());
            if let Some(active_video) = active_video {
                let still_inside = path_is_inside_folder(&folder, &active_video);
                if !still_inside {
                    self.video_player.stop();
                    self.media_inline_video_path = None;
                    self.media_inline_video_seen = false;
                    self.media_inline_video_requested_at = None;
                    self.media_inline_video_pending_target = None;
                    self.media_playback_lease = None;
                    crate::video_player::playback_trace_phase(
                        "ui.folder_change.stop_playback",
                        &format!("released {active_video} leaving {folder}"),
                    );
                }
            }
        }
        self.media_child_folder_cache.clear();
        self.media_folder_entry_cache.clear();
        for cancelled in self.media_child_folder_cancel.values() {
            cancelled.store(true, Ordering::Release);
        }
        self.media_child_folder_cancel.clear();
        self.media_child_folder_inflight.clear();
        self.media_content_generation = self.media_content_generation.wrapping_add(1);
        // WP-069: keep an already-published order visible while a cached
        // inventory reconciles. Blanking it unconditionally is what made a tab
        // switch or refresh flash an empty grid even though the rows were
        // already in memory. A genuinely new folder has no cached inventory, so
        // its stale order is still cleared.
        if !using_runtime_inventory {
            self.media_display_cache = Arc::new(Vec::new());
        }
        self.media_display_cache_key = None;
        self.media_display_desired_key = None;
        if self.media_display_inflight.is_some() {
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        self.media_search_requests.cancel_current();
        if let Some(cancel) = self.media_search_index_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        if let Some(cancel) = self.media_suggestion_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        self.media_search_index = None;
        self.media_search_index_key = None;
        self.media_search_index_inflight = None;
        self.media_suggestion_key = None;
        self.media_suggestions = Arc::new(Vec::new());
        self.media_suggestion_inflight = None;
        if let Some(cancel) = self.media_stat_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        self.media_stat_request = None;
        self.media_stat_complete_key = None;
        self.media_explorer.stats_loading = false;
        self.media_explorer.stats = Arc::new(HashMap::new());
        self.media_stats_generation = self.media_stats_generation.wrapping_add(1);
        if let Some(cancel) = self.clip_index_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = self.clip_query_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.clip_indexing = false;
        self.clip_index_request = None;
        self.clip_query_request = None;
        self.media_semantic_inflight = None;
        let tx = self.compare_work_tx.clone();
        let scan_id = self.compare_lanes[pos].scan_id;
        let lane_label = lane_id;
        self.media_scan_diagnostics = MediaScanDiagnostics {
            lane_id,
            scan_id,
            status: "scanning".to_string(),
            ..MediaScanDiagnostics::default()
        };
        if let Some(previous) = self.compare_scan_cancellations.remove(&lane_id) {
            previous.store(true, Ordering::Release);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.compare_scan_cancellations
            .insert(lane_id, Arc::clone(&cancelled));
        let (inventory_store, mut inventory_issue) = match self.media_db.inventory_store() {
            Ok(store) => (Some(store), None),
            Err(error) => (None, Some(error)),
        };
        let media_io = Arc::clone(&self.media_io);
        thread::spawn(move || {
            let root = Path::new(&folder);
            let root_identity = media_io_root_identity_for_path(&folder, scan_id);
            let _ = tx.send(CompareWorkEvent::ScanRootReady {
                lane_id: lane_label,
                scan_id,
                identity: root_identity.clone(),
            });
            let progress_tx = tx.clone();
            let cache_started = std::time::Instant::now();
            if !using_runtime_inventory {
                if let Some(store) = inventory_store.as_ref() {
                    match store.load_with_identity(
                        root,
                        &root_identity.key,
                        recursive,
                        media_filter.short_label(),
                    ) {
                        Ok(Some(inventory)) => {
                            if cancelled.load(Ordering::Acquire) {
                                return;
                            }
                            let display_order = Arc::new((0..inventory.files.len()).collect());
                            let _ = tx.send(CompareWorkEvent::ScanCacheReady {
                                lane_id: lane_label,
                                scan_id,
                                inventory,
                                display_order,
                                load_ms: cache_started.elapsed().as_millis() as u64,
                            });
                        }
                        Ok(None) => {}
                        Err(error) => inventory_issue = Some(error),
                    }
                }
            }
            let io_request =
                media_io.enqueue(root_identity.clone(), crate::media_io::WorkClass::Scan);
            let io_permit = loop {
                if cancelled.load(Ordering::Acquire) {
                    io_request.cancel();
                    return;
                }
                match io_request.try_acquire() {
                    Ok(Some(permit)) => break permit,
                    Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            let started = std::time::Instant::now();
            let mut first_batch_ms = None;
            match collect_media_paths_for_compare_cancellable(
                root,
                recursive,
                media_filter,
                || cancelled.load(Ordering::Acquire),
                |files| {
                    first_batch_ms.get_or_insert(started.elapsed().as_millis() as u64);
                    let _ = progress_tx.send(CompareWorkEvent::ScanBatch {
                        lane_id: lane_label,
                        scan_id,
                        files,
                    });
                },
            ) {
                Ok((mut files, dir_errors)) => {
                    media_io.record_filesystem_duration(
                        &root_identity,
                        crate::media_io::WorkClass::Scan,
                        started.elapsed(),
                    );
                    io_permit.finish(if dir_errors == 0 {
                        crate::media_io::PermitOutcome::Success
                    } else {
                        crate::media_io::PermitOutcome::Error
                    });
                    files.sort();
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let mut inventory_generation = None;
                    let mut inventory_write_ms = None;
                    let mut inventory_error = inventory_issue;
                    let mut committed = None;
                    if dir_errors == 0 && !cancelled.load(Ordering::Acquire) {
                        let write_started = std::time::Instant::now();
                        match inventory_store.as_ref() {
                            Some(store) => match store.replace_cancellable_with_identity(
                                root,
                                &root_identity.key,
                                recursive,
                                media_filter.short_label(),
                                &files,
                                elapsed_ms,
                                first_batch_ms,
                                || cancelled.load(Ordering::Acquire),
                            ) {
                                Ok(Some(commit)) => {
                                    inventory_generation = Some(commit.generation);
                                    committed = Some(commit);
                                }
                                Ok(None) => return,
                                Err(error) => inventory_error = Some(error),
                            },
                            None => {}
                        }
                        inventory_write_ms = Some(write_started.elapsed().as_millis() as u64);
                    }
                    let display_order = Arc::new((0..files.len()).collect());
                    let _ = tx.send(CompareWorkEvent::ScanDone {
                        lane_id: lane_label,
                        scan_id,
                        files,
                        display_order,
                        dir_errors,
                        elapsed_ms,
                        first_batch_ms,
                        inventory_generation,
                        inventory_write_ms,
                        inventory_error,
                    });
                    if let (Some(store), Some(commit)) =
                        (inventory_store.as_ref(), committed.as_ref())
                    {
                        store.cleanup_superseded(commit);
                    }
                }
                Err(MediaScanFailure::Cancelled) => {
                    io_permit.finish(crate::media_io::PermitOutcome::Cancelled);
                }
                Err(MediaScanFailure::Failed(error)) => {
                    media_io.record_filesystem_duration(
                        &root_identity,
                        crate::media_io::WorkClass::Scan,
                        started.elapsed(),
                    );
                    io_permit.finish(crate::media_io::PermitOutcome::Error);
                    let _ = tx.send(CompareWorkEvent::ScanError {
                        lane_id: lane_label,
                        scan_id,
                        error,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        inventory_error: inventory_issue,
                    });
                }
            }
        });
    }

    fn request_compare_image(&mut self, lane_id: usize, index: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let start_now = {
            let lane = &mut self.compare_lanes[pos];
            if lane.files.is_empty() {
                false
            } else {
                let total = lane.files.len();
                let target = index.min(total.saturating_sub(1));
                lane.pending_image_index = Some(target);
                lane.index = target;
                lane.loading_image = true;
                lane.image_path = lane.files[target].clone();
                lane.image_error.clear();
                // Keep the previous texture visible while the next decodes:
                // flipping through a folder must not flash blank frames.
                !lane.loading_image_inflight
            }
        };
        if start_now {
            self.start_compare_image_load(lane_id);
        }
    }

    fn compare_lane_open_selected_with_system(
        &mut self,
        lane_id: usize,
    ) -> Result<(usize, usize), String> {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return Err("media lane missing".to_string());
        };
        let (indices, paths): (Vec<usize>, Vec<String>) = {
            let lane = &self.compare_lanes[pos];
            if lane.total() == 0 {
                self.set_compare_lane_message(lane_id, "No files found".to_string());
                return Err("no files found".to_string());
            }
            if lane.selected_files.is_empty() {
                let index = lane.index.min(lane.total() - 1);
                let path = lane.files.get(index).cloned().unwrap_or_default();
                (vec![index], vec![path])
            } else {
                let mut selected: Vec<usize> = lane.selected_files.iter().copied().collect();
                selected.sort_unstable();
                let selected_paths = selected
                    .iter()
                    .filter_map(|index| lane.files.get(*index).cloned())
                    .collect::<Vec<_>>();
                (selected, selected_paths)
            }
        };

        if paths.is_empty() {
            self.set_compare_lane_message(lane_id, "No valid selection".to_string());
            return Err("no valid selection".to_string());
        }

        let mut opened = 0usize;
        let mut failed = 0usize;
        for path in &paths {
            let target_path = Path::new(path);
            match self.open_path_with_system_app(target_path) {
                Ok(_) => opened += 1,
                Err(_) => failed += 1,
            }
        }
        if let Some(index) = indices.first().copied() {
            self.request_compare_image(lane_id, index);
        }

        if paths.len() == 1 {
            let first = Path::new(&paths[0]);
            let file_name = first
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("file");
            if opened == 1 {
                self.set_compare_lane_message(lane_id, format!("Opened {file_name}"));
            } else {
                self.set_compare_lane_message(lane_id, format!("Open failed for {file_name}"));
            }
            return Ok((opened, failed));
        }

        match (opened, failed) {
            (0, failed_count) => {
                self.set_compare_lane_message(
                    lane_id,
                    format!(
                        "Open failed for {failed_count} file{}",
                        if failed_count == 1 { "" } else { "s" }
                    ),
                );
            }
            (opened_count, 0) => {
                self.set_compare_lane_message(lane_id, format!("Opened {opened_count} files"));
            }
            (opened_count, failed_count) => {
                self.set_compare_lane_message(
                    lane_id,
                    format!("Opened {opened_count} files, {failed_count} failed"),
                );
            }
        }
        Ok((opened, failed))
    }

    fn compare_lane_open_selected_index_with_system(&mut self, lane_id: usize, index: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let target = {
            let lane = &self.compare_lanes[pos];
            lane.files.get(index).cloned()
        };
        let Some(target) = target else {
            return self
                .set_compare_lane_message(lane_id, "Selected file index unavailable".to_string());
        };
        let target_path = Path::new(&target);
        let target_name = target_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file");

        if let Err(error) = self.open_path_with_system_app(target_path) {
            return self.set_compare_lane_message(
                lane_id,
                format!("Open failed for {target_name}: {error}"),
            );
        }

        self.request_compare_image(lane_id, index);
        self.set_compare_lane_message(lane_id, format!("Opened {target_name}"));
    }

    fn compare_lane_open_location_with_system(
        &mut self,
        lane_id: usize,
        open_index: Option<usize>,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };

        let target = {
            let lane = &self.compare_lanes[pos];
            if let Some(index) = open_index {
                lane.files.get(index).cloned()
            } else if !lane.selected_files.is_empty() {
                lane.selected_files
                    .iter()
                    .min()
                    .and_then(|index| lane.files.get(*index).cloned())
            } else if lane.total() != 0 {
                Some(lane.files[lane.index].clone())
            } else if !lane.folder.trim().is_empty() {
                Some(lane.folder.clone())
            } else {
                None
            }
        };

        let Some(target) = target else {
            return self.set_compare_lane_message(
                lane_id,
                "No file or folder selected to open".to_string(),
            );
        };

        let target_path = Path::new(&target);
        if !target_path.exists() {
            return self.set_compare_lane_message(
                lane_id,
                "Selected file or folder no longer exists".to_string(),
            );
        }

        if let Err(error) =
            self.open_in_file_manager_with_system_app(target_path, target_path.is_file())
        {
            return self.set_compare_lane_message(
                lane_id,
                format!(
                    "Open location failed for {}: {error}",
                    target_path.to_string_lossy()
                ),
            );
        }

        self.set_compare_lane_message(
            lane_id,
            format!(
                "Opened location for {}",
                target_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file")
            ),
        );
    }

    fn draw_explorer_context_actions(
        ui: &mut egui::Ui,
        request: &mut CompareLaneRenderRequest,
        can_open: bool,
        has_selection: bool,
        has_files: bool,
        has_folder: bool,
        can_paste: bool,
        open_index: Option<usize>,
    ) {
        Self::draw_explorer_action_buttons(
            ui,
            request,
            can_open,
            has_selection,
            has_files,
            has_folder,
            can_paste,
            open_index,
            true,
        );
    }

    /// Full Explorer-parity context menu for MEDIA surfaces (WP-045): the
    /// shared open/copy/paste/delete/selection verbs plus cut, rename, new
    /// folder, refresh, sort, color labels, and favorites. Everything flows
    /// through the request so Compare surfaces stay untouched.
    #[allow(clippy::too_many_arguments)]
    fn draw_media_context_menu(
        ui: &mut egui::Ui,
        request: &mut CompareLaneRenderRequest,
        label_definitions: &[crate::media_db::ColorLabelDefinition],
        can_open: bool,
        has_selection: bool,
        has_files: bool,
        has_folder: bool,
        can_paste: bool,
        open_index: Option<usize>,
        current_sort: (crate::media_explorer::MediaSort, bool),
    ) {
        Self::draw_explorer_action_buttons(
            ui,
            request,
            can_open,
            has_selection,
            has_files,
            has_folder,
            can_paste,
            open_index,
            true,
        );
        ui.separator();
        ui.label(
            egui::RichText::new("Edit")
                .small()
                .strong()
                .color(theme::ink_faint()),
        );
        let action = |ui: &mut egui::Ui, text: &str, enabled: bool, hover: &str| -> bool {
            let button = egui::Button::new(egui::RichText::new(text).small().color(theme::ink()))
                .frame(false);
            if enabled {
                ui.add(button).on_hover_text(hover).clicked()
            } else {
                ui.add_enabled(false, button).on_hover_text(hover).clicked()
            }
        };
        if action(
            ui,
            "Cut",
            has_selection,
            "Cut selected file(s) — paste moves them (Ctrl/Cmd+X)",
        ) {
            request.cut_selected = true;
            ui.close_menu();
        }
        if action(ui, "Rename", has_selection, "Rename the selected file (F2)") {
            request.rename_selected = true;
            ui.close_menu();
        }
        if action(ui, "New folder", has_folder, "Create a folder here") {
            request.new_folder = true;
            ui.close_menu();
        }
        if action(ui, "Refresh", has_folder, "Rescan this folder (F5)") {
            request.refresh = true;
            ui.close_menu();
        }
        ui.separator();
        ui.label(
            egui::RichText::new("Path")
                .small()
                .strong()
                .color(theme::ink_faint()),
        );
        if action(
            ui,
            "Copy absolute path",
            has_selection || has_files || has_folder,
            "Copy the selected file path (or current folder) as an absolute path",
        ) {
            request.copy_absolute_path = true;
            ui.close_menu();
        }
        if action(
            ui,
            "Copy portable path",
            has_selection || has_files || has_folder,
            "Copy a path relative to the workspace, or to the selected folder for external media",
        ) {
            request.copy_portable_path = true;
            ui.close_menu();
        }
        ui.menu_button(egui::RichText::new("Sort by").small(), |ui| {
            for sort in [
                crate::media_explorer::MediaSort::Name,
                crate::media_explorer::MediaSort::Modified,
                crate::media_explorer::MediaSort::Size,
                crate::media_explorer::MediaSort::Created,
            ] {
                let active = current_sort.0 == sort;
                let arrow = if active {
                    if current_sort.1 {
                        " ↓"
                    } else {
                        " ↑"
                    }
                } else {
                    ""
                };
                if ui
                    .selectable_label(active, format!("{}{arrow}", sort.label()))
                    .clicked()
                {
                    // Same key toggles direction, new key starts ascending.
                    request.sort_to = Some(if active {
                        (sort, !current_sort.1)
                    } else {
                        (sort, false)
                    });
                    ui.close_menu();
                }
            }
        });
        ui.separator();
        ui.label(
            egui::RichText::new("Mark")
                .small()
                .strong()
                .color(theme::ink_faint()),
        );
        ui.menu_button(egui::RichText::new("Add label").small(), |ui| {
            egui::ScrollArea::vertical()
                .id_source("media-context-add-labels")
                .max_height(280.0)
                .show(ui, |ui| {
                    for label in label_definitions {
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(160.0, 18.0), Sense::click());
                        ui.painter().circle_filled(
                            egui::pos2(rect.min.x + 9.0, rect.center().y),
                            5.0,
                            media_label_color(label_definitions, &label.id),
                        );
                        ui.painter().text(
                            egui::pos2(rect.min.x + 20.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &label.name,
                            egui::TextStyle::Small.resolve(ui.style()),
                            theme::ink(),
                        );
                        if resp.clicked() {
                            request.add_label = Some(label.id.clone());
                            ui.close_menu();
                        }
                    }
                });
        });
        ui.menu_button(egui::RichText::new("Remove label").small(), |ui| {
            egui::ScrollArea::vertical()
                .id_source("media-context-remove-labels")
                .max_height(260.0)
                .show(ui, |ui| {
                    for label in label_definitions {
                        if ui.small_button(&label.name).clicked() {
                            request.remove_label = Some(label.id.clone());
                            ui.close_menu();
                        }
                    }
                });
            ui.separator();
            if ui.small_button("Remove all labels").clicked() {
                request.clear_labels = true;
                ui.close_menu();
            }
        });
        if action(
            ui,
            "Toggle favorite",
            has_selection || has_folder,
            "Star/unstar the selected file (or this folder when nothing is selected)",
        ) {
            request.toggle_favorite = true;
            ui.close_menu();
        }
    }

    fn draw_explorer_action_buttons(
        ui: &mut egui::Ui,
        request: &mut CompareLaneRenderRequest,
        can_open: bool,
        has_selection: bool,
        has_files: bool,
        has_folder: bool,
        can_paste: bool,
        open_index: Option<usize>,
        close_menu: bool,
    ) {
        let themed_action =
            |ui: &mut egui::Ui, text: &str, color: egui::Color32, enabled: bool, hover: &str| {
                let button =
                    egui::Button::new(egui::RichText::new(text).small().color(color)).frame(false);
                if enabled {
                    ui.add(button).on_hover_text(hover).clicked()
                } else {
                    ui.add_enabled(false, button).on_hover_text(hover).clicked()
                }
            };

        let has_open_target = can_open || open_index.is_some();
        let section = |ui: &mut egui::Ui, title: &str| {
            ui.separator();
            ui.label(
                egui::RichText::new(title)
                    .small()
                    .strong()
                    .color(theme::ink_faint()),
            );
        };

        if close_menu {
            ui.set_min_width(190.0);

            section(ui, "Open");
            if themed_action(
                ui,
                "Open file",
                theme::ink(),
                has_open_target,
                "Open selected file(s) with the system app (Enter or Ctrl/Cmd+O)",
            ) {
                request.open_file = true;
                request.open_index_in_system = open_index;
                ui.close_menu();
            }
            if themed_action(
                ui,
                "Open file location",
                theme::ink_soft(),
                has_files || has_folder,
                "Reveal selected file/folder in OS file browser",
            ) {
                request.open_location = true;
                request.open_location_index = open_index;
                ui.close_menu();
            }

            section(ui, "Clipboard");
            let copy_hover = if has_selection {
                "Copy selected file(s) (Ctrl/Cmd + C)"
            } else {
                "No files are selected"
            };
            if themed_action(ui, "Copy", theme::ink_faint(), has_selection, copy_hover) {
                request.copy_selected = true;
                ui.close_menu();
            }
            if themed_action(
                ui,
                "Paste",
                theme::ink_faint(),
                can_paste,
                "Paste copied file(s) into this folder (Ctrl/Cmd + V)",
            ) {
                request.paste = true;
                ui.close_menu();
            }
            if themed_action(
                ui,
                "Delete",
                theme::error_ink(),
                has_selection,
                "Delete selected file(s) (Delete / Backspace)",
            ) {
                request.delete_selected = true;
                ui.close_menu();
            }

            section(ui, "Selection");
            if themed_action(
                ui,
                "Select all",
                theme::ink_soft(),
                has_files,
                "Select all files in this view (Ctrl/Cmd + A)",
            ) {
                request.select_all = true;
                ui.close_menu();
            }
            if themed_action(
                ui,
                "Select none",
                theme::ink_soft(),
                has_selection,
                "Clear selection in this view (Ctrl/Cmd + Shift + A)",
            ) {
                request.select_none = true;
                ui.close_menu();
            }
            if themed_action(
                ui,
                "Invert selection",
                theme::ink_faint(),
                has_files,
                "Invert file selection in this view (Ctrl/Cmd + I)",
            ) {
                request.invert_selection = true;
                ui.close_menu();
            }
            return;
        }

        if themed_action(
            ui,
            "Open file",
            theme::ink(),
            has_open_target,
            "Open selected file(s) with the system app (Enter or Ctrl/Cmd+O)",
        ) {
            request.open_file = true;
            request.open_index_in_system = open_index;
        }

        ui.separator();
        if themed_action(
            ui,
            "Open file location",
            theme::ink_soft(),
            has_files || has_folder,
            "Reveal selected file/folder in OS file browser",
        ) {
            request.open_location = true;
            request.open_location_index = open_index;
        }

        ui.separator();
        let copy_hover = if has_selection {
            "Copy selected file(s) (Ctrl/Cmd + C)"
        } else {
            "No files are selected"
        };
        if themed_action(ui, "Copy", theme::ink_faint(), has_selection, copy_hover) {
            request.copy_selected = true;
        }
        if themed_action(
            ui,
            "Paste",
            theme::ink_faint(),
            can_paste,
            "Paste copied file(s) into this folder (Ctrl/Cmd + V)",
        ) {
            request.paste = true;
        }
        if themed_action(
            ui,
            "Delete",
            theme::error_ink(),
            has_selection,
            "Delete selected file(s) (Delete / Backspace)",
        ) {
            request.delete_selected = true;
        }

        ui.separator();
        if themed_action(
            ui,
            "Select all",
            theme::ink_soft(),
            has_files,
            "Select all files in this view (Ctrl/Cmd + A)",
        ) {
            request.select_all = true;
        }
        if themed_action(
            ui,
            "Select none",
            theme::ink_soft(),
            has_selection,
            "Clear selection in this view (Ctrl/Cmd + Shift + A)",
        ) {
            request.select_none = true;
        }
        if themed_action(
            ui,
            "Invert selection",
            theme::ink_faint(),
            has_files,
            "Invert file selection in this view (Ctrl/Cmd + I)",
        ) {
            request.invert_selection = true;
        }
    }

    fn open_path_with_system_app(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Err("File no longer exists".to_string());
        }
        if crate::media_explorer::is_video_path(&path.to_string_lossy()) {
            return crate::video_player::open_in_vlc(path)
                .or_else(|_| crate::video_player::open_with_dialog(path));
        }
        #[cfg(target_os = "windows")]
        {
            StdCommand::new("cmd")
                .args(["/C", "start", "", path.to_string_lossy().as_ref()])
                .spawn()
                .map_err(|error| format!("failed to launch default file app: {error}"))?;
        }
        #[cfg(target_os = "macos")]
        {
            StdCommand::new("open")
                .arg(path)
                .spawn()
                .map_err(|error| format!("failed to launch default file app: {error}"))?;
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            StdCommand::new("xdg-open")
                .arg(path)
                .spawn()
                .map_err(|error| format!("failed to launch default file app: {error}"))?;
        }
        Ok(())
    }

    fn open_in_file_manager_with_system_app(
        &self,
        path: &Path,
        reveal_file: bool,
    ) -> Result<(), String> {
        if !path.exists() {
            return Err("Location no longer exists".to_string());
        }

        #[cfg(target_os = "windows")]
        {
            let mut command = StdCommand::new("explorer");
            if reveal_file {
                command.args(["/select,", path.to_string_lossy().as_ref()]);
            } else {
                command.arg(path);
            }
            command
                .spawn()
                .map_err(|error| format!("failed to open file manager: {error}"))?;
        }
        #[cfg(target_os = "macos")]
        {
            if reveal_file {
                StdCommand::new("open")
                    .arg("-R")
                    .arg(path)
                    .spawn()
                    .map_err(|error| format!("failed to open file manager: {error}"))?;
            } else {
                StdCommand::new("open")
                    .arg(path)
                    .spawn()
                    .map_err(|error| format!("failed to open file manager: {error}"))?;
            }
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            let target = if reveal_file {
                path.parent().unwrap_or(path)
            } else {
                path
            };
            StdCommand::new("xdg-open")
                .arg(target)
                .spawn()
                .map_err(|error| format!("failed to open file manager: {error}"))?;
        }
        Ok(())
    }

    fn start_compare_image_load(&mut self, lane_id: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        if self.compare_lanes[pos].loading_image_inflight {
            return;
        }

        let (file_path, load_id) = {
            let lane = &mut self.compare_lanes[pos];
            let Some(target) = lane.pending_image_index.take() else {
                lane.loading_image = false;
                return;
            };
            if lane.files.is_empty() {
                lane.loading_image = false;
                return;
            }
            let total = lane.files.len();
            let target = target.min(total.saturating_sub(1));
            lane.index = target;
            lane.image_path = lane.files[target].clone();
            lane.load_id = lane.load_id.saturating_add(1);
            lane.loading_image_inflight = true;
            lane.image_error.clear();
            (lane.image_path.clone(), lane.load_id)
        };
        let tx = self.compare_work_tx.clone();
        let root_identity = self.media_root_identity.clone().unwrap_or_else(|| {
            crate::media_io::RootIdentity::new(
                file_path.clone(),
                0,
                crate::media_io::RootKind::Unknown,
            )
        });
        let media_io = Arc::clone(&self.media_io);
        thread::spawn(move || {
            let io_request =
                media_io.enqueue(root_identity.clone(), crate::media_io::WorkClass::Visible);
            let Ok(io_permit) = io_request.wait() else {
                return;
            };
            let started = std::time::Instant::now();
            let result = image::open(&file_path).and_then(|img| {
                let rgba = img.to_rgba8();
                Ok((
                    rgba.width() as usize,
                    rgba.height() as usize,
                    rgba.into_raw(),
                ))
            });
            media_io.record_filesystem_duration(
                &root_identity,
                crate::media_io::WorkClass::Visible,
                started.elapsed(),
            );
            let succeeded = result.is_ok();
            match result {
                Ok((width, height, pixels)) => {
                    let _ = tx.send(CompareWorkEvent::ImageDone {
                        lane_id,
                        load_id,
                        path: file_path,
                        width,
                        height,
                        pixels,
                    });
                }
                Err(error) => {
                    let _ = tx.send(CompareWorkEvent::ImageError {
                        lane_id,
                        load_id,
                        path: file_path,
                        error: format!("{error}"),
                    });
                }
            }
            io_permit.finish(if succeeded {
                crate::media_io::PermitOutcome::Success
            } else {
                crate::media_io::PermitOutcome::Error
            });
        });
    }

    /// Decode the identity reference set into strip thumbnails off-thread
    /// (WP-017). Capped at 24 anchors; results arrive as AnchorsLoaded.
    fn start_anchor_load(&mut self) {
        let Some(ref_dir) = self.config.identity_reference_dir.clone() else {
            return;
        };
        if self.compare_anchors_loading {
            return;
        }
        self.compare_anchors_loading = true;
        self.compare_anchor_error.clear();
        let tx = self.compare_work_tx.clone();
        thread::spawn(move || {
            let mut items: Vec<(String, usize, usize, Vec<u8>)> = Vec::new();
            let mut error = None;
            match std::fs::read_dir(&ref_dir) {
                Ok(entries) => {
                    let mut paths: Vec<PathBuf> = entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file() && is_supported_image_path(p))
                        .collect();
                    paths.sort();
                    for path in paths.into_iter().take(24) {
                        if let Ok(img) = image::open(&path) {
                            let thumb = img.thumbnail(160, 96).to_rgba8();
                            items.push((
                                path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("anchor")
                                    .to_string(),
                                thumb.width() as usize,
                                thumb.height() as usize,
                                thumb.into_raw(),
                            ));
                        }
                    }
                    if items.is_empty() {
                        error = Some("no decodable anchor images in the reference dir".to_string());
                    }
                }
                Err(e) => error = Some(format!("read reference dir: {e}")),
            }
            let _ = tx.send(CompareWorkEvent::AnchorsLoaded { items, error });
        });
    }

    fn handle_compare_events(&mut self, ctx: &egui::Context) {
        let event_started = std::time::Instant::now();
        let mut handled = 0usize;
        loop {
            // Large scans and thumbnail/index workers can complete in bursts.
            // Keep event draining inside one small frame budget and schedule
            // the remainder instead of monopolizing input/render handling.
            if handled >= 32 || event_started.elapsed() >= std::time::Duration::from_millis(4) {
                ctx.request_repaint();
                break;
            }
            let event = match self.compare_work_rx.try_recv() {
                Ok(event) => {
                    handled += 1;
                    event
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(_) => break,
            };
            match event {
                CompareWorkEvent::ClipReady(result) => {
                    self.clip_loading = false;
                    match result {
                        Ok(engine) => {
                            // Names the picked tensors so a wrong export is
                            // visible, never silent (round 3, finding 5).
                            self.clip_status = format!(
                                "semantic search: CLIP ready ({} dims; {})",
                                engine.dim, engine.picks
                            );
                            self.clip_engine = Some(engine);
                        }
                        Err(err) => {
                            self.clip_status = format!("semantic search: local fallback ({err})");
                        }
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ClipIndexProgress { key, done, total } => {
                    if self.clip_index_request.as_ref() == Some(&key) {
                        self.clip_status = format!("semantic search: indexing {done}/{total}…");
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ClipIndexDone {
                    key,
                    indexed,
                    failed,
                    ok,
                } => {
                    if self.clip_index_request.as_ref() != Some(&key) {
                        continue;
                    }
                    self.clip_indexing = false;
                    self.clip_index_request = None;
                    self.clip_index_cancel = None;
                    if ok {
                        self.clip_indexed_folder = Some(key.folder);
                        self.clip_status = if failed > 0 {
                            format!("semantic search: index ready ({indexed} new, {failed} failed)")
                        } else {
                            format!("semantic search: index ready ({indexed} new)")
                        };
                    } else {
                        // Open failed (index busy elsewhere): stay unindexed
                        // so a later frame retries instead of going silently
                        // empty for the rest of the session.
                        self.clip_status = "semantic search: index busy — will retry".to_string();
                    }
                    // Force a fresh ranked query against the new index.
                    self.media_semantic = None;
                    self.media_semantic_generation = self.media_semantic_generation.wrapping_add(1);
                    self.media_semantic_inflight = None;
                    ctx.request_repaint();
                }
                CompareWorkEvent::ClipQueryDone {
                    key,
                    indices,
                    missing,
                    error,
                } => {
                    if self.clip_query_request.as_ref() != Some(&key) {
                        continue;
                    }
                    self.clip_query_request = None;
                    self.clip_query_cancel = None;
                    self.media_semantic_inflight = None;
                    if let Some(err) = error {
                        self.clip_status = format!("semantic search: {err}");
                        self.clip_query_backoff = Some(std::time::Instant::now());
                    } else {
                        self.clip_query_backoff = None;
                        self.media_semantic = Some((key.folder, key.query, Arc::new(indices)));
                        self.media_semantic_generation =
                            self.media_semantic_generation.wrapping_add(1);
                        if missing > 0 {
                            self.clip_status =
                                format!("semantic search: ranked (skipped {missing} unindexed)");
                        }
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::MediaSearchIndexReady {
                    key,
                    index,
                    elapsed_ms,
                } => {
                    if self.media_search_index_inflight.as_ref() == Some(&key) {
                        self.media_search_index_inflight = None;
                    }
                    if self.media_search_index_key_for(key.lane_id).as_ref() == Some(&key) {
                        self.media_search_status = format!(
                            "search index · {} rows · {elapsed_ms} ms",
                            group_thousands(index.len())
                        );
                        self.media_query_diagnostics.status = "index_ready".to_string();
                        self.media_query_diagnostics.index_rows = index.len();
                        self.media_query_diagnostics.index_elapsed_ms = elapsed_ms;
                        self.media_search_index_key = Some(key);
                        self.media_search_index = Some(index);
                        self.media_display_pending_since = Some(std::time::Instant::now());
                    } else {
                        self.media_query_diagnostics.stale_drops =
                            self.media_query_diagnostics.stale_drops.saturating_add(1);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::MediaSuggestionsDone {
                    key,
                    suggestions,
                    cancelled,
                } => {
                    if self.media_suggestion_inflight.as_ref() == Some(&key) {
                        self.media_suggestion_inflight = None;
                        self.media_suggestion_cancel = None;
                    }
                    let current_index_key = self.media_search_index_key_for(key.index_key.lane_id);
                    let current_folder = self
                        .compare_lane_position(key.index_key.lane_id)
                        .map(|pos| sanitize_folder_input(&self.compare_lanes[pos].folder))
                        .unwrap_or_default();
                    let current = media_suggestion_result_is_current(
                        &key,
                        current_index_key.as_ref(),
                        self.media_search_index_key.as_ref(),
                        &self.media_search_query,
                        &current_folder,
                        cancelled,
                    );
                    if current {
                        self.media_suggestion_key = Some(key);
                        self.media_suggestions = suggestions;
                    } else if !cancelled {
                        self.media_query_diagnostics.stale_drops =
                            self.media_query_diagnostics.stale_drops.saturating_add(1);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::MediaDisplayDone {
                    key,
                    request_key,
                    indices,
                    elapsed_ms,
                    scanned_rows,
                    matched_rows,
                    cancelled,
                } => {
                    if self.media_display_inflight == Some(request_key) {
                        self.media_display_inflight = None;
                    }
                    if !cancelled
                        && self.media_search_requests.is_current(request_key)
                        && self.media_display_desired_key.as_ref() == Some(&key)
                    {
                        self.media_search_status = format!(
                            "display · {} rows · {elapsed_ms} ms",
                            group_thousands(indices.len())
                        );
                        self.media_query_diagnostics.status = "complete".to_string();
                        self.media_query_diagnostics.scanned_rows = scanned_rows;
                        self.media_query_diagnostics.matched_rows = matched_rows;
                        self.media_query_diagnostics.query_elapsed_ms = elapsed_ms;
                        if crate::media_search::parse_query(&key.query).is_empty() {
                            self.media_query_diagnostics.sort_elapsed_ms = elapsed_ms;
                        }
                        self.media_display_cache_key = Some(key);
                        self.media_display_cache = indices;
                    } else {
                        self.media_query_diagnostics.stale_drops =
                            self.media_query_diagnostics.stale_drops.saturating_add(1);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::MediaStatsDone {
                    key,
                    stats,
                    elapsed_ms,
                    failures,
                } => {
                    // Only the exact active immutable request may clear the
                    // loading state or publish. Folder text alone is
                    // insufficient after a rescan of the same NAS root.
                    let current_content =
                        self.compare_lane_position(key.lane_id).is_some_and(|pos| {
                            let lane = &self.compare_lanes[pos];
                            lane.scan_id == key.scan_id
                                && self.media_content_generation == key.content_generation
                                && lane.inventory_generation == key.inventory_generation
                                && sanitize_folder_input(&lane.folder) == key.folder
                        });
                    if current_content && self.media_stat_request.as_ref() == Some(&key) {
                        self.media_explorer.stats_loading = false;
                        self.media_stat_request = None;
                        self.media_stat_cancel = None;
                        self.media_stat_complete_key = Some(key);
                        self.media_explorer.stats = stats;
                        self.media_query_diagnostics.stat_elapsed_ms = elapsed_ms;
                        self.media_query_diagnostics.stat_failures = failures;
                        self.media_stats_generation = self.media_stats_generation.wrapping_add(1);
                    } else {
                        self.media_query_diagnostics.stale_drops =
                            self.media_query_diagnostics.stale_drops.saturating_add(1);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::MediaChildFoldersDone {
                    key,
                    folders,
                    prepared,
                    error,
                } => {
                    // The exact staged request key is the producer/consumer
                    // contract. Comparing it with `media_root_identity` is
                    // wrong because that identity belongs to the committed
                    // active folder, not the staged navigator location.
                    let was_inflight = self.media_child_folder_inflight.remove(&key);
                    self.media_child_folder_cancel.remove(&key);
                    let current = self.compare_lane_position(key.lane_id).is_some_and(|pos| {
                        was_inflight && self.compare_lanes[pos].scan_id == key.scan_id
                    });
                    if !current {
                        self.media_query_diagnostics.stale_drops =
                            self.media_query_diagnostics.stale_drops.saturating_add(1);
                        continue;
                    }
                    let folder = key.folder;
                    // A small bounded cache supports current-folder and
                    // parent/sibling navigation without growing forever.
                    if self.media_child_folder_cache.len() >= 64
                        && !self.media_child_folder_cache.contains_key(&folder)
                    {
                        self.media_child_folder_cache.clear();
                        self.media_folder_entry_cache.clear();
                    }
                    self.media_child_folder_cache
                        .insert(folder.clone(), folders);
                    self.media_folder_entry_cache
                        .insert(folder.clone(), prepared);
                    if let Some(cancel) = self.media_suggestion_cancel.take() {
                        cancel.store(true, Ordering::Release);
                        self.media_query_diagnostics.cancellations =
                            self.media_query_diagnostics.cancellations.saturating_add(1);
                    }
                    self.media_suggestion_key = None;
                    self.media_suggestions = Arc::new(Vec::new());
                    if let Some(error) = error {
                        let current = self
                            .compare_lanes
                            .first()
                            .map(|lane| sanitize_folder_input(&lane.folder))
                            .unwrap_or_default();
                        if current == folder {
                            self.compare_action_message = error;
                        }
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ScanRootReady {
                    lane_id,
                    scan_id,
                    identity,
                } => {
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        if self.compare_lanes[pos].scan_id == scan_id {
                            let source_root =
                                sanitize_folder_input(&self.compare_lanes[pos].folder);
                            let changed = self.media_root_identity.as_ref() != Some(&identity)
                                || self.media_root_source.as_deref() != Some(source_root.as_str());
                            if changed {
                                let repaint_ctx = ctx.clone();
                                self.thumb_engine = Some(
                                    crate::media_thumbs::ThumbnailEngine::new_with_cache_cap_and_io(
                                        &self.config.workspace_root,
                                        self.config.media_thumb_cache_mb,
                                        Arc::clone(&self.media_io),
                                        identity.clone(),
                                        Path::new(&source_root),
                                        Box::new(move || repaint_ctx.request_repaint()),
                                    ),
                                );
                                let _ = self.thumb_textures.clear();
                            }
                            self.media_scan_diagnostics.root_key = Some(identity.key.clone());
                            self.media_scan_diagnostics.root_kind = Some(identity.kind);
                            self.media_root_identity = Some(identity);
                            self.media_root_source = Some(source_root);
                        }
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ScanCacheReady {
                    lane_id,
                    scan_id,
                    inventory,
                    display_order,
                    load_ms,
                } => {
                    let mut start_preview = false;
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        let lane = &mut self.compare_lanes[pos];
                        if lane.scan_id != scan_id || !lane.scanning {
                            continue;
                        }
                        lane.scan_using_cached_inventory = true;
                        lane.inventory_generation = Some(inventory.generation);
                        self.media_scan_diagnostics.status = "cached_reconciling".to_string();
                        self.media_scan_diagnostics.cached_items = inventory.files.len();
                        self.media_scan_diagnostics.inventory_generation =
                            Some(inventory.generation);
                        lane.files = Arc::new(inventory.files);
                        // WP-069: publish an immediately renderable order for
                        // every batch, not only for the empty-query/Name-sort
                        // special case. Otherwise any active query or non-default
                        // sort turns a streaming folder open into a blank grid
                        // until a worker round trip finishes. The display cache
                        // KEY stays unset, so the authoritative ranked/sorted
                        // order still replaces this provisional one.
                        self.media_display_cache = display_order;
                        lane.action_message = format!(
                            "cached generation {} · {} items · loaded {load_ms} ms · checking source…",
                            inventory.generation,
                            group_thousands(lane.files.len())
                        );
                        if !lane.files.is_empty() {
                            lane.index = 0;
                            lane.loading_image = true;
                            lane.pending_image_index = Some(0);
                            lane.image_path = lane.files[0].clone();
                            start_preview = true;
                        }
                        self.media_content_generation =
                            self.media_content_generation.wrapping_add(1);
                    }
                    self.restore_media_tab_selection(lane_id);
                    if self
                        .compare_lane_position(lane_id)
                        .is_some_and(|pos| self.compare_lanes[pos].pending_image_index.is_some())
                    {
                        start_preview = true;
                    }
                    if start_preview {
                        self.start_compare_image_load(lane_id);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ScanBatch {
                    lane_id,
                    scan_id,
                    files,
                } => {
                    let mut start_preview = false;
                    let mut appended_range = None;
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        let lane = &mut self.compare_lanes[pos];
                        if lane.scan_id != scan_id || !lane.scanning {
                            continue;
                        }
                        if lane.scan_using_cached_inventory {
                            // The complete cached generation remains visible
                            // until reconciliation is itself complete.
                            continue;
                        }
                        let was_empty = lane.files.is_empty();
                        let first_new = lane.files.len();
                        Arc::make_mut(&mut lane.files).extend(files);
                        appended_range = Some(first_new..lane.files.len());
                        if was_empty && !lane.files.is_empty() {
                            lane.index = 0;
                            lane.loading_image = true;
                            lane.pending_image_index = Some(0);
                            start_preview = true;
                        }
                        self.media_content_generation =
                            self.media_content_generation.wrapping_add(1);
                    }
                    // WP-069: append every published batch so the grid keeps
                    // growing and stays scrollable during enumeration. Appending
                    // in traversal order is provisional; the settled order
                    // replaces it when the display worker completes.
                    if let Some(range) = appended_range {
                        Arc::make_mut(&mut self.media_display_cache).extend(range);
                    }
                    if start_preview {
                        self.start_compare_image_load(lane_id);
                    }
                    ctx.request_repaint();
                }
                CompareWorkEvent::ScanDone {
                    lane_id,
                    scan_id,
                    files,
                    display_order,
                    dir_errors,
                    elapsed_ms,
                    first_batch_ms,
                    inventory_generation,
                    inventory_write_ms,
                    inventory_error,
                } => {
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        if self.compare_lanes[pos].scan_id != scan_id {
                            continue;
                        }
                        // A cached-generation stat sweep may have been running
                        // during reconciliation. Final scan publication is a
                        // new content boundary even when the folder text is
                        // unchanged, so invalidate it before replacing rows.
                        if let Some(cancel) = self.media_stat_cancel.take() {
                            cancel.store(true, Ordering::Release);
                            self.media_query_diagnostics.cancellations =
                                self.media_query_diagnostics.cancellations.saturating_add(1);
                        }
                        self.media_stat_request = None;
                        self.media_stat_complete_key = None;
                        self.media_explorer.stats_loading = false;
                        self.media_explorer.stats = Arc::new(HashMap::new());
                        self.media_stats_generation = self.media_stats_generation.wrapping_add(1);
                        self.media_scan_diagnostics.status = if dir_errors == 0 {
                            "complete".to_string()
                        } else {
                            "incomplete".to_string()
                        };
                        self.media_scan_diagnostics.final_items = files.len();
                        self.media_scan_diagnostics.first_batch_ms = first_batch_ms;
                        self.media_scan_diagnostics.elapsed_ms = Some(elapsed_ms);
                        self.media_scan_diagnostics.dir_errors = dir_errors;
                        self.media_scan_diagnostics.inventory_generation = inventory_generation;
                        self.media_scan_diagnostics.inventory_write_ms = inventory_write_ms;
                        self.media_scan_diagnostics.inventory_error = inventory_error.clone();
                        self.compare_scan_cancellations.remove(&lane_id);
                        let kept_cached =
                            self.compare_lanes[pos].scan_using_cached_inventory && dir_errors > 0;
                        if kept_cached {
                            let lane = &mut self.compare_lanes[pos];
                            lane.scanning = false;
                            lane.scan_using_cached_inventory = false;
                            lane.scan_error = format!(
                                "Source scan incomplete: skipped {dir_errors} unreadable directories; last-good inventory retained"
                            );
                            lane.action_message = format!(
                                "stale inventory · {} items · reconcile {} ms",
                                group_thousands(lane.files.len()),
                                elapsed_ms
                            );
                            self.media_scan_diagnostics.status =
                                "incomplete_stale_retained".to_string();
                            ctx.request_repaint();
                            continue;
                        }

                        // Progressive batches are traversal-ordered, while
                        // the terminal inventory is sorted. Relocate an exact
                        // pending playback target once at publication so its
                        // numeric slot cannot become stale under the same scan
                        // id. The worker's sorted vector makes this O(log n),
                        // including 100k+ folders.
                        let pending_sorted_index = self
                            .media_inline_video_pending_target
                            .as_ref()
                            .filter(|pending| {
                                pending.requested_scan_id == scan_id
                                    && pending.tab_id == self.media_tabs.active_id().as_str()
                            })
                            .and_then(|pending| {
                                pending_path_index_in_sorted(
                                    &files,
                                    &pending.path,
                                    &pending.path_key,
                                    |candidate| self.media_db.key_for(candidate),
                                )
                            });
                        // Inline Library playback may already have become
                        // visible during a progressive batch, which clears the
                        // short-lived pending placement. Re-establish exact
                        // placement at the terminal reorder boundary so a
                        // playing tile follows its file instead of its old
                        // traversal-order number.
                        let inline_terminal_target =
                            self.media_inline_video_path.as_ref().and_then(|path| {
                                let path_key = self.media_db.key_for(path);
                                pending_path_index_in_sorted(&files, path, &path_key, |candidate| {
                                    self.media_db.key_for(candidate)
                                })
                                .map(|source_index| (path.clone(), path_key, source_index))
                            });

                        let lane = &mut self.compare_lanes[pos];
                        lane.scanning = false;
                        lane.scan_using_cached_inventory = false;
                        if dir_errors == 0 {
                            lane.inventory_generation = inventory_generation;
                        } else if !kept_cached {
                            lane.inventory_generation = None;
                        }
                        lane.loading_image_inflight = false;
                        lane.pending_image_index = None;
                        lane.scan_error.clear();
                        if dir_errors > 0 {
                            lane.scan_error = format!(
                                "Incomplete source scan: skipped {dir_errors} unreadable entries or directories; result not saved"
                            );
                        }
                        let selected_paths: HashSet<String> = lane
                            .selected_files
                            .iter()
                            .filter_map(|&index| lane.files.get(index).cloned())
                            .collect();
                        let prior_preview = lane.image_path.clone();
                        lane.files = Arc::new(files);
                        let mut clear_pending = false;
                        if let Some(pending) = self.media_inline_video_pending_target.as_mut() {
                            if pending.requested_scan_id == scan_id
                                && pending.tab_id == self.media_tabs.active_id().as_str()
                            {
                                if let Some(index) = pending_sorted_index {
                                    pending.source_index = index;
                                    pending.checked_display_key = None;
                                } else {
                                    // The exact requested path disappeared
                                    // during reconciliation; never retarget a
                                    // neighboring sorted row.
                                    clear_pending = true;
                                }
                            }
                        }
                        if clear_pending {
                            self.media_inline_video_pending_target = None;
                        }
                        if let Some((path, path_key, source_index)) = inline_terminal_target {
                            self.media_inline_video_pending_target =
                                Some(PendingInlineVideoTarget {
                                    tab_id: self.media_tabs.active_id().as_str().to_string(),
                                    path,
                                    path_key,
                                    source_index,
                                    requested_scan_id: scan_id,
                                    checked_display_key: None,
                                });
                            self.media_inline_video_requested_at = Some(std::time::Instant::now());
                        }
                        // WP-069: publish an immediately renderable order for
                        // every batch, not only for the empty-query/Name-sort
                        // special case. Otherwise any active query or non-default
                        // sort turns a streaming folder open into a blank grid
                        // until a worker round trip finishes. The display cache
                        // KEY stays unset, so the authoritative ranked/sorted
                        // order still replaces this provisional one.
                        self.media_display_cache = display_order;
                        lane.selected_files = lane
                            .files
                            .iter()
                            .enumerate()
                            .filter_map(|(index, path)| {
                                selected_paths.contains(path).then_some(index)
                            })
                            .collect();
                        lane.selection_anchor = None;
                        lane.action_message = if lane.files.is_empty() {
                            if dir_errors > 0 {
                                format!(
                                    "partial scan · no readable media · skipped {dir_errors} entries/directories · not committed"
                                )
                            } else {
                                match lane.media_filter {
                                    MediaFilterMode::ImagesOnly => {
                                        "No supported images in folder".to_string()
                                    }
                                    MediaFilterMode::VideosOnly => {
                                        "No supported videos in folder".to_string()
                                    }
                                    MediaFilterMode::All => {
                                        "No supported media in folder".to_string()
                                    }
                                }
                            }
                        } else {
                            let first = first_batch_ms
                                .map(|ms| format!("{ms} ms"))
                                .unwrap_or_else(|| "n/a".to_string());
                            match (inventory_generation, inventory_error) {
                                (Some(generation), _) => format!(
                                    "inventory generation {generation} · first batch {first} · scan {elapsed_ms} ms · inventory write {} ms",
                                    inventory_write_ms.unwrap_or_default()
                                ),
                                (_, Some(error)) => format!(
                                    "scan complete · first batch {first} · total {elapsed_ms} ms · inventory not saved: {error}"
                                ),
                                _ => format!(
                                    "partial scan · first batch {first} · total {elapsed_ms} ms · not committed"
                                ),
                            }
                        };
                        if lane.files.is_empty() {
                            lane.image_error = if dir_errors > 0 {
                                "Source scan incomplete; no readable media found".to_string()
                            } else {
                                match lane.media_filter {
                                    MediaFilterMode::ImagesOnly => {
                                        "No supported images in folder".to_string()
                                    }
                                    MediaFilterMode::VideosOnly => {
                                        "No supported videos in folder".to_string()
                                    }
                                    MediaFilterMode::All => {
                                        "No supported media in folder".to_string()
                                    }
                                }
                            };
                            lane.loading_image = false;
                            lane.image_path.clear();
                            lane.texture = None;
                            lane.texture_size = None;
                        } else if !prior_preview.is_empty()
                            && lane.files.iter().any(|path| path == &prior_preview)
                        {
                            lane.index = lane
                                .files
                                .iter()
                                .position(|path| path == &prior_preview)
                                .unwrap_or(0);
                            lane.loading_image = false;
                            lane.pending_image_index = None;
                        } else {
                            lane.image_error.clear();
                            lane.loading_image = true;
                            lane.pending_image_index = Some(0);
                            drop(lane);
                            self.start_compare_image_load(lane_id);
                        }
                        self.media_content_generation =
                            self.media_content_generation.wrapping_add(1);
                    }
                    self.restore_media_tab_selection(lane_id);
                    if self
                        .compare_lane_position(lane_id)
                        .is_some_and(|pos| self.compare_lanes[pos].pending_image_index.is_some())
                    {
                        self.start_compare_image_load(lane_id);
                    }
                }
                CompareWorkEvent::ScanError {
                    lane_id,
                    scan_id,
                    error,
                    elapsed_ms,
                    inventory_error,
                } => {
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        let lane = &mut self.compare_lanes[pos];
                        if lane.scan_id != scan_id {
                            continue;
                        }
                        self.media_scan_diagnostics.status = if lane.scan_using_cached_inventory {
                            "offline_stale_retained".to_string()
                        } else {
                            "error".to_string()
                        };
                        self.media_scan_diagnostics.elapsed_ms = Some(elapsed_ms);
                        self.media_scan_diagnostics.source_error = Some(error.clone());
                        self.media_scan_diagnostics.inventory_error = inventory_error.clone();
                        self.compare_scan_cancellations.remove(&lane_id);
                        lane.scanning = false;
                        let kept_cached = lane.scan_using_cached_inventory;
                        lane.scan_using_cached_inventory = false;
                        if kept_cached {
                            lane.scan_error = match inventory_error {
                                Some(inventory_error) => format!(
                                    "Source unavailable: {error}; visible cache retained, but inventory health also failed: {inventory_error}"
                                ),
                                None => format!(
                                    "Source unavailable: {error}; last-good inventory retained"
                                ),
                            };
                            lane.action_message = format!(
                                "offline/stale inventory · {} items · failed after {elapsed_ms} ms",
                                group_thousands(lane.files.len())
                            );
                        } else {
                            lane.loading_image_inflight = false;
                            lane.pending_image_index = None;
                            lane.scan_error = match inventory_error {
                                Some(inventory_error) => format!(
                                    "{error} ({elapsed_ms} ms); last-good inventory unavailable: {inventory_error}"
                                ),
                                None => format!("{error} ({elapsed_ms} ms)"),
                            };
                            lane.files = Arc::new(Vec::new());
                            lane.image_path.clear();
                            lane.loading_image = false;
                            lane.texture = None;
                            lane.texture_size = None;
                        }
                    }
                }
                CompareWorkEvent::ImageDone {
                    lane_id,
                    load_id,
                    path,
                    width,
                    height,
                    pixels,
                } => {
                    let Some(pos) = self.compare_lane_position(lane_id) else {
                        continue;
                    };
                    let has_pending = {
                        let lane = &mut self.compare_lanes[pos];
                        // `load_id` identifies the in-flight load we started; a mismatch
                        // means a newer load superseded this one — ignore it.
                        if lane.load_id != load_id {
                            continue;
                        }
                        // The in-flight load finished: free the slot unconditionally so a
                        // fast scroll that moved the target can never strand
                        // `loading_image_inflight` true (the freeze-on-scroll bug).
                        lane.loading_image_inflight = false;
                        // Apply only if this is still the lane's current target; otherwise
                        // drop the pixels and let the pending load fetch the new target.
                        if lane.image_path == path {
                            if width == 0 || height == 0 {
                                lane.image_error = "Empty image".to_string();
                                lane.texture = None;
                                lane.texture_size = None;
                            } else {
                                lane.image_error.clear();
                                let color_image =
                                    ColorImage::from_rgba_unmultiplied([width, height], &pixels);
                                lane.texture = Some(ctx.load_texture(
                                    format!("compare_lane_{}_{}", lane_id, load_id),
                                    color_image,
                                    TextureOptions::LINEAR,
                                ));
                                lane.texture_size = Some([width, height]);
                            }
                        }
                        let has_pending = lane.pending_image_index.is_some();
                        lane.loading_image = has_pending;
                        has_pending
                    };
                    if has_pending {
                        self.start_compare_image_load(lane_id);
                    }
                }
                CompareWorkEvent::ImageError {
                    lane_id,
                    load_id,
                    path,
                    error,
                } => {
                    self.handle_compare_events_image_error(lane_id, load_id, path, error);
                }
                CompareWorkEvent::AnchorsLoaded { items, error } => {
                    self.compare_anchors_loading = false;
                    self.compare_anchor_error = error.unwrap_or_default();
                    self.compare_anchor_thumbs = items
                        .into_iter()
                        .enumerate()
                        .map(|(i, (name, w, h, rgba))| {
                            let color = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                            let tex = ctx.load_texture(
                                format!("anchor_{i}_{name}"),
                                color,
                                TextureOptions::LINEAR,
                            );
                            (name, tex)
                        })
                        .collect();
                }
            }
        }
    }

    fn handle_compare_events_image_error(
        &mut self,
        lane_id: usize,
        load_id: u64,
        path: String,
        error: String,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let has_pending = {
            let lane = &mut self.compare_lanes[pos];
            // Mismatched load_id -> a newer load owns the slot; ignore this result.
            if lane.load_id != load_id {
                return;
            }
            lane.loading_image_inflight = false;
            // Only surface the error if it is still the current target; a superseded
            // failure is replaced by the pending load below.
            if lane.image_path == path {
                lane.image_error = error;
                lane.texture = None;
                lane.texture_size = None;
            }
            let has_pending = lane.pending_image_index.is_some();
            lane.loading_image = has_pending;
            has_pending
        };
        if has_pending {
            self.start_compare_image_load(lane_id);
        }
    }

    fn ingest_images(&mut self, paths: Vec<String>) {
        let image_list: Vec<String> = paths
            .into_iter()
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty())
            .collect();
        if image_list.is_empty() {
            self.import_summary = "No paths entered to import".to_string();
            return;
        }
        let result = if let Ok(mut svc) = self.service.lock() {
            svc.ingest_images(&self.project_name, &image_list, self.in_place)
        } else {
            Vec::new()
        };

        let successes = result.iter().filter(|entry| entry.ok).count();
        let failures = result.len().saturating_sub(successes);
        let mode_hint = result
            .first()
            .map(|item| item.mode.clone())
            .unwrap_or_else(|| "n/a".to_string());
        self.import_summary =
            format!("imported {successes} images, failed {failures}, mode {mode_hint}");
        self.last_import_images = result
            .into_iter()
            .filter(|item| item.ok)
            .map(|item| item.destination)
            .collect();
        self.pipeline_status = if successes > 0 {
            format!("{successes} images ready for pipeline")
        } else {
            "no valid images imported".to_string()
        };

        if let Some(first) = self.last_import_images.first() {
            if let Some(parent) = PathBuf::from(first)
                .parent()
                .and_then(|value| value.parent())
            {
                self.worktree_path = parent.to_string_lossy().to_string();
            }
        }

        match Arc::clone(&self.service).lock() {
            Ok(mut svc) => {
                self.worktree_view = Self::load_worktrees(&mut svc);
            }
            Err(_) => {}
        }
    }

    fn execute_pipeline(&mut self) {
        if self.running_pipeline || self.selected_features.is_empty() {
            self.pipeline_status = if self.selected_features.is_empty() {
                "No feature selected".to_string()
            } else {
                "pipeline already running".to_string()
            };
            return;
        }

        let mut feature_keys: Vec<String> = self.selected_features.iter().cloned().collect();
        feature_keys.sort();

        let mut images = self.last_import_images.clone();
        if images.is_empty() && self.worktree_path != "no worktree yet" {
            images = collect_image_paths(&PathBuf::from(&self.worktree_path));
            self.last_import_images = images.clone();
        }
        if images.is_empty() {
            self.pipeline_status = "No images available".to_string();
            return;
        }

        let project_name = self.project_name.clone();
        let worktree = self.worktree_path.clone();
        let in_place = self.in_place;
        let service = Arc::clone(&self.service);
        let sender = self.tx.clone();
        self.running_pipeline = true;
        self.pipeline_status = "pipeline started".to_string();

        thread::spawn(move || {
            // Panic boundary: a panic inside run_pipeline must never leave the UI
            // wedged. Convert any panic into a failed run result and ALWAYS send
            // PipelineDone so running_pipeline is reset on the UI thread.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(mut svc) = service.lock() {
                    svc.run_pipeline(
                        &project_name,
                        &images,
                        &feature_keys,
                        Some(worktree),
                        in_place,
                    )
                } else {
                    Err("service lock failure".to_string())
                }
            }));
            let result = match outcome {
                Ok(result) => result,
                Err(panic) => {
                    let detail = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    Err(format!("pipeline panicked: {detail}"))
                }
            };
            let _ = sender.send(AppEvent::PipelineDone(result));
        });
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        self.handle_compare_events(ctx);
        loop {
            let event = match self.rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The result channel is gone (worker dropped without sending);
                    // never leave the UI wedged with running_pipeline stuck true.
                    if self.running_pipeline {
                        self.running_pipeline = false;
                        self.pipeline_status = "pipeline result channel disconnected".to_string();
                    }
                    break;
                }
            };
            match event {
                AppEvent::PipelineDone(result) => {
                    self.running_pipeline = false;
                    match result {
                        Ok(summary) => {
                            self.pipeline_status = format!(
                                "run {} completed: status={} ok={} failed={}",
                                summary.run_id,
                                summary.status,
                                summary.totals.get("ok").copied().unwrap_or(0),
                                summary.totals.get("failed").copied().unwrap_or(0)
                            );
                            self.run_output = summary.output_path.clone();
                            self.worktree_path = summary.worktree;
                            let mut lines = vec![
                                format!("run_id={}", summary.run_id),
                                format!("status={}", summary.status),
                                format!("output={}", summary.output_path),
                                format!("features={}", summary.feature_keys.join(",")),
                                format!("images={}", summary.images.len()),
                            ];
                            for plugin in summary.plugin_results {
                                lines.push(format!(
                                    "{}::{} -> {} ({})",
                                    plugin.plugin_id,
                                    plugin.feature_id,
                                    plugin.status,
                                    plugin.message
                                ));
                                for artifact in &plugin.artifacts {
                                    lines.push(format!("    artifact: {artifact}"));
                                }
                            }
                            self.run_summary = lines.join("\n");
                        }
                        Err(err) => {
                            self.pipeline_status = format!("pipeline failed: {err}");
                        }
                    }
                }
            }
        }
        // Only rebuild the debug-event string when the Run & Debug tab (its sole
        // consumer) is showing. Doing this every frame on every tab locked the
        // service and formatted up to 800 events per frame -> idle CPU spin.
        if self.active_tab == Tab::RunDebug {
            if let Ok(mut svc) = self.service.lock() {
                let limit = svc.max_debug_events();
                let events = svc.get_recent_events(limit);
                let mut lines = String::new();
                for event in events {
                    lines.push_str(&format!(
                        "[{}] {} {} - {}\n",
                        event.ts, event.level, event.source, event.message
                    ));
                }
                self.debug_lines = lines;
            }
        }
    }

    /// Poll the controller into bound action fires (WP-046). Every pad
    /// binding is edge-triggered with hold-repeat for navigation actions;
    /// the left stick integrates into smooth row scrolling. Also services an
    /// armed pad rebind capture. Returns (fired actions, stick scroll rows).
    fn media_poll_controller(
        &mut self,
        app_focused: bool,
    ) -> (Vec<crate::media_input::MediaAction>, isize) {
        use crate::media_input::{
            stick_scroll_velocity, CaptureSlot, PadAxisCode, PadButtonCode, PadInput,
        };
        if !self.controller_input_enabled {
            return (Vec::new(), 0);
        }
        let mut snapshot = None;
        if let Some(gilrs) = self.controller_gilrs.as_mut() {
            while let Some(event) = gilrs.next_event() {
                match event.event {
                    EventType::Connected => {
                        self.controller_active = Some(event.id);
                        self.media_repeat.clear();
                    }
                    EventType::Disconnected => {
                        if self.controller_active == Some(event.id) {
                            self.controller_active = None;
                        }
                        self.media_repeat.clear();
                    }
                    _ => {}
                }
            }
            let gamepad_id = self
                .controller_active
                .or_else(|| gilrs.gamepads().next().map(|(id, _)| id));
            self.controller_active = gamepad_id;
            if let Some(gamepad_id) = gamepad_id {
                if let Some(gamepad) = gilrs.connected_gamepad(gamepad_id) {
                    snapshot = Some(crate::media_input::ControllerSnapshot::from_gilrs(
                        gamepad_id, &gamepad,
                    ));
                }
            }
        }
        if snapshot.is_none() {
            snapshot = self.controller_legacy.poll();
        }
        self.controller_legacy_active = snapshot
            .as_ref()
            .is_some_and(|state| state.source == "winmm-directinput-fallback");
        self.controller_input_source = snapshot
            .as_ref()
            .map(|state| state.source.clone())
            .unwrap_or_default();
        self.controller_input_device_id = snapshot
            .as_ref()
            .map(|state| state.device_id.clone())
            .unwrap_or_default();
        self.controller_input_device_name = snapshot
            .as_ref()
            .map(|state| state.device_name.clone())
            .unwrap_or_default();
        let Some(gamepad) = snapshot else {
            self.media_repeat.clear();
            return (Vec::new(), 0);
        };
        let now_ms = self.input_epoch.elapsed().as_millis() as u64;
        let bindings: Vec<(crate::media_input::MediaAction, PadInput)> = self
            .media_bindings
            .pad
            .iter()
            .map(|(action, input)| (*action, *input))
            .collect();

        // Latch Start even while Guide/background suppression owns the input,
        // so releasing Guide/focus with Start still held cannot manufacture a
        // delayed Facial edge.
        let start_down = gamepad.pressed(PadButtonCode::Start);
        let start_was_down = self.controller_start_down;
        self.controller_start_down = start_down;
        // Guide owns its chord layer (notably Guide+Start = Alt+Tab),
        // and background windows own no controller input. Suppress every held
        // Facial binding until release so releasing Guide/focus cannot create
        // a delayed false edge inside Facial.
        let guide_pressed = gamepad.guide_pressed;
        if crate::media_input::suppress_controller_actions(app_focused, guide_pressed) {
            // Focus loss is a hard handoff boundary: release buttons before
            // another app can observe a stale Facial drag/click.
            if !app_focused {
                if self.controller_pointer_left_down {
                    let _ = crate::platform_input::set_pointer_button(
                        crate::platform_input::PointerButton::Left,
                        false,
                    );
                }
                if self.controller_pointer_right_down {
                    let _ = crate::platform_input::set_pointer_button(
                        crate::platform_input::PointerButton::Right,
                        false,
                    );
                }
                self.controller_pointer_left_down = false;
                self.controller_pointer_right_down = false;
                self.controller_pointer_mode = false;
                self.controller_pointer_accum = [0.0, 0.0];
            }
            for (_, input) in &bindings {
                let is_down = match input {
                    PadInput::Button(code) => gamepad.pressed(*code),
                    PadInput::AxisPos(code) => gamepad.axis(*code) > 0.5,
                    PadInput::AxisNeg(code) => gamepad.axis(*code) < -0.5,
                };
                if is_down {
                    self.media_repeat.suppress(*input, now_ms);
                }
            }
            self.media_stick_accum = 0.0;
            self.media_last_poll = Some(std::time::Instant::now());
            return (Vec::new(), 0);
        }

        if crate::media_input::reserved_app_switch_edge_with_guide(
            app_focused,
            guide_pressed,
            start_down,
            start_was_down,
        ) {
            if self.controller_pointer_left_down {
                let _ = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Left,
                    false,
                );
            }
            if self.controller_pointer_right_down {
                let _ = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Right,
                    false,
                );
            }
            self.controller_pointer_left_down = false;
            self.controller_pointer_right_down = false;
            self.controller_pointer_mode = false;
            self.controller_pointer_accum = [0.0, 0.0];
            self.media_stick_accum = 0.0;
            self.media_repeat.clear();
            self.compare_action_message = match crate::platform_input::switch_apps() {
                Ok(()) => "Switched apps — controller handed off".to_string(),
                Err(err) => format!("App switch failed: {err}"),
            };
            self.media_last_poll = Some(std::time::Instant::now());
            return (Vec::new(), 0);
        }

        let now = std::time::Instant::now();
        let dt = self
            .media_last_poll
            .map(|t| now.duration_since(t).as_secs_f32().min(0.10))
            .unwrap_or(0.0);
        self.media_last_poll = Some(now);

        if self.controller_pointer_mode {
            let vx = crate::media_input::pointer_velocity(gamepad.axis(PadAxisCode::RightStickX));
            let vy = crate::media_input::pointer_velocity(-gamepad.axis(PadAxisCode::RightStickY));
            self.controller_pointer_accum[0] += vx * dt;
            self.controller_pointer_accum[1] += vy * dt;
            let dx = self.controller_pointer_accum[0].trunc() as i32;
            let dy = self.controller_pointer_accum[1].trunc() as i32;
            self.controller_pointer_accum[0] -= dx as f32;
            self.controller_pointer_accum[1] -= dy as f32;
            if let Err(err) = crate::platform_input::move_pointer(dx, dy) {
                self.compare_action_message = format!("Controller cursor failed: {err}");
                self.controller_pointer_mode = false;
            }

            let left_down = gamepad.pressed(PadButtonCode::South);
            if left_down != self.controller_pointer_left_down {
                if let Err(err) = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Left,
                    left_down,
                ) {
                    self.compare_action_message = format!("Controller click failed: {err}");
                }
                self.controller_pointer_left_down = left_down;
            }
            let right_down = gamepad.pressed(PadButtonCode::East);
            if right_down != self.controller_pointer_right_down {
                if let Err(err) = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Right,
                    right_down,
                ) {
                    self.compare_action_message = format!("Controller click failed: {err}");
                }
                self.controller_pointer_right_down = right_down;
            }
        }

        // Armed pad rebind: the first pressed button / deflected axis wins.
        if let Some(capture) = &self.media_capture {
            if let CaptureSlot::Pad(action) = capture.slot {
                if capture.expired(now_ms) {
                    self.media_capture = None;
                } else {
                    let mut captured: Option<PadInput> = None;
                    for code in PadButtonCode::ALL {
                        if gamepad.pressed(code) {
                            captured = Some(PadInput::Button(code));
                            break;
                        }
                    }
                    if captured.is_none() {
                        for code in [
                            PadAxisCode::LeftStickX,
                            PadAxisCode::LeftStickY,
                            PadAxisCode::RightStickX,
                            PadAxisCode::RightStickY,
                        ] {
                            let value = gamepad.axis(code);
                            if value > 0.6 {
                                captured = Some(PadInput::AxisPos(code));
                                break;
                            }
                            if value < -0.6 {
                                captured = Some(PadInput::AxisNeg(code));
                                break;
                            }
                        }
                    }
                    if let Some(input) = captured {
                        self.media_bindings.rebind_pad(action, input);
                        self.save_media_bindings();
                        self.media_capture = None;
                        self.media_repeat.clear();
                        // The capturing press is still physically held: mark
                        // it held-without-edge so the new binding does not
                        // fire instantly (round 3, finding 10).
                        self.media_repeat.suppress(input, now_ms);
                        self.compare_action_message =
                            format!("{} bound to {}", action.label(), input.display());
                        return (Vec::new(), 0); // swallow the captured press
                    }
                }
            }
        }

        let mut fired = Vec::new();
        for (action, input) in bindings {
            if self.controller_pointer_mode
                && matches!(
                    input,
                    PadInput::Button(PadButtonCode::South | PadButtonCode::East)
                )
            {
                self.media_repeat.suppress(input, now_ms);
                continue;
            }
            let is_down = match input {
                PadInput::Button(code) => gamepad.pressed(code),
                PadInput::AxisPos(code) => gamepad.axis(code) > 0.5,
                PadInput::AxisNeg(code) => gamepad.axis(code) < -0.5,
            };
            if self
                .media_repeat
                .should_fire(input, is_down, now_ms, action.repeats())
            {
                fired.push(action);
            }
        }

        // Analog scroll: left stick Y (gilrs up = +1, so invert for rows).
        let stick_y = gamepad.axis(PadAxisCode::LeftStickY);
        self.media_stick_accum += stick_scroll_velocity(-stick_y) * dt;
        let rows = self.media_stick_accum.trunc() as isize;
        self.media_stick_accum -= rows as f32;

        (fired, rows)
    }

    // -----------------------------------------------------------------------
    // Model-drivable control surface (file-based UI-intent protocol)
    // -----------------------------------------------------------------------

    /// Poll intents/, apply at most one ui-intent this frame, write its Receipt
    /// (Applied/Rejected) via api::mark_intent_applied, record a ModelAction event.
    /// Returns true if an intent was applied (for repaint coalescing).
    fn poll_and_apply_model_intent(&mut self, ctx: &egui::Context) -> bool {
        // Keep a live snapshot intent in the queue until the renderer returns
        // the requested framebuffer. This prevents a second frame from
        // re-applying the same still-pending intent.
        if self.pending_model_snapshot.is_some() {
            return false;
        }
        let cmd = match api::poll_pending_intent(&self.api_paths) {
            Some(cmd) => cmd,
            None => return false,
        };

        if let CommandKind::UiSnapshot { output } = &cmd.command {
            self.pending_model_snapshot = Some(PendingModelSnapshot {
                path: self.ui_snapshot_path(output.as_deref(), &cmd.action_id),
                command: cmd,
                requested_at: None,
            });
            return true;
        }

        let (mut applied, mut message) = self.apply_ui_intent(ctx, &cmd);
        // A receipt is written in the same frame the intent is applied, before
        // the next render refreshes the persisted tab viewports. Without this,
        // a mutation receipt embedded the PRE-change tab array — so a model that
        // read the receipt of the very command it just issued saw the old value
        // and could not tell a stale-but-succeeded receipt from an
        // accurate-but-failed one (no-context Manual audit, finding G).
        self.snapshot_active_media_tab();
        let intent_result = if let CommandKind::MediaVideoControl { action, output, .. } =
            &cmd.command
        {
            let fresh = self.video_player.snapshot_fresh();
            let mut value = match fresh {
                Ok(snapshot) => {
                    self.sync_media_playback_priority(snapshot.as_ref());
                    if applied {
                        if let Some(state) = snapshot.as_ref().filter(|state| state.confirmed) {
                            let contradicted = ((action == "play" || action == "play_library")
                                && !state.playing)
                                || (action == "pause" && state.playing);
                            if contradicted {
                                applied = false;
                                message = format!(
                                    "LibVLC confirmed {:?}, not requested {action}",
                                    state.status
                                );
                            }
                        }
                    }
                    if applied && snapshot.as_ref().is_some_and(|state| !state.confirmed) {
                        message = format!("{message}; pending LibVLC confirmation");
                    }
                    snapshot
                        .and_then(|state| serde_json::to_value(state).ok())
                        .unwrap_or_else(|| serde_json::json!({}))
                }
                Err(error) => {
                    applied = false;
                    message = error.clone();
                    serde_json::json!({"error": error})
                }
            };
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "surface_owner".to_string(),
                    self.media_video_surface_owner()
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
                if let Ok(diagnostics) = serde_json::to_value(self.video_player.diagnostics()) {
                    object.insert("playback_diagnostics".to_string(), diagnostics);
                }
                if let Some(engine) = self.thumb_engine.as_ref() {
                    if let Ok(diagnostics) = serde_json::to_value(engine.diagnostics()) {
                        object.insert("thumbnail_diagnostics".to_string(), diagnostics);
                    }
                }
                if let Ok(diagnostics) = serde_json::to_value(self.media_io.diagnostics()) {
                    object.insert("media_io_diagnostics".to_string(), diagnostics);
                }
                object.insert(
                    "search_status".to_string(),
                    serde_json::Value::String(self.media_search_status.clone()),
                );
                if let Ok(diagnostics) = serde_json::to_value(&self.media_scan_diagnostics) {
                    object.insert("scan_diagnostics".to_string(), diagnostics);
                }
                if let Ok(diagnostics) = serde_json::to_value(&self.media_query_diagnostics) {
                    object.insert("query_diagnostics".to_string(), diagnostics);
                }
                object.insert(
                    "ui_frame_diagnostics".to_string(),
                    serde_json::json!({
                        "last_us": self.media_ui_frame_last_us,
                        "max_us": self.media_ui_frame_max_us,
                    }),
                );
            }
            if action == "capture_frame" {
                let path = self.media_video_capture_path(output.as_deref(), &cmd.action_id);
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "capture_path".to_string(),
                        serde_json::Value::String(path.to_string_lossy().to_string()),
                    );
                    object.insert(
                        "capture_exists".to_string(),
                        serde_json::Value::Bool(
                            path.metadata().is_ok_and(|metadata| metadata.len() > 0),
                        ),
                    );
                }
            }
            value
        } else if let CommandKind::MediaLabelMutation { path, .. } = &cmd.command {
            serde_json::json!({
                "labels": &self.media_label_definitions,
                "usage": &self.media_label_usage_counts,
                "path": path,
                "assigned_labels": path.as_deref().map(|path| self.media_db.labels(path)),
            })
        } else if let CommandKind::MediaFolderNavigate { action } = &cmd.command {
            let lane = self.compare_lanes.first();
            serde_json::json!({
                "action": action,
                "staged_folder": sanitize_folder_input(&self.media_explorer.folder_navigator_location),
                "committed_folder": lane.map(|lane| sanitize_folder_input(&lane.folder)).unwrap_or_default(),
                "active_tab": self.media_tabs.active_id().as_str(),
                "active_lane": lane.map(|lane| lane.id),
                "scan_requested": applied && matches!(action.as_str(), "commit" | "open_new_tab"),
                "navigator_visible": self.media_explorer.show_folder_navigator,
            })
        } else if let CommandKind::MediaTabs {
            action,
            tab_id,
            path,
        } = &cmd.command
        {
            serde_json::json!({
                "action": action,
                "requested_tab_id": tab_id,
                "requested_path": path,
                "active_tab_id": self.media_tabs.active_id().as_str(),
                "tabs": self.media_tabs.tabs().iter().map(|tab| {
                    let resolved = if tab.viewport.folder_key.is_empty() { String::new() } else { self.media_db.path_for_key(&tab.viewport.folder_key) };
                    let collection = tab.viewport.kind == crate::media_tabs::MediaTabKind::Collection;
                    serde_json::json!({
                        "id": tab.id.as_str(),
                        // WP-067: tab kind and sub-view are model-visible so a
                        // collection tab is distinguishable from a folder tab.
                        "kind": if collection { "collection" } else { "folder" },
                        "collection_view": collection.then(|| format!("{:?}", tab.viewport.collection_view)),
                        "collection_label_id": tab.viewport.collection_label_id,
                        "title": if collection { "Favorites".to_string() } else { crate::media_tabs::folder_tab_title(&resolved) },
                        "folder_key": tab.viewport.folder_key,
                        "folder": resolved,
                        "search_query": tab.viewport.search_query,
                        // WP-066: per-tab search scope.
                        "search_folder_only": tab.viewport.search_folder_only,
                        // WP-068: per-tab ordering.
                        "sort": format!("{:?}", tab.viewport.sort),
                        "sort_descending": tab.viewport.sort_descending,
                    })
                }).collect::<Vec<_>>(),
                "selection_restore_pending": !self.media_tab_pending_selection_keys.is_empty(),
                // Structured label catalog: open_collection needs a stable ID,
                // and a model must not have to parse it out of the note text.
                "label_catalog": self.media_label_definitions.iter().map(|definition| {
                    serde_json::json!({
                        "id": definition.id,
                        "name": definition.name,
                        "hex": definition.hex,
                        "usage": self.media_label_usage_counts
                            .get(&definition.id)
                            .copied()
                            .unwrap_or(0),
                    })
                }).collect::<Vec<_>>(),
                "search_folder_only": self.media_search_folder_only,
                "last_scope_change": self.media_last_scope_change.map(|(scan, inventory)| {
                    serde_json::json!({
                        "scan_unchanged": scan,
                        "inventory_unchanged": inventory,
                    })
                }),
                "scan_generation": self.compare_lanes.first().map(|lane| lane.scan_id),
                "scan_active": self.compare_lanes.first().is_some_and(|lane| lane.scanning),
                "inventory_count": self.compare_lanes.first().map(|lane| lane.files.len()).unwrap_or(0),
                // WP-069: the grid renders the DISPLAY order, not the inventory,
                // so a blank grid with a full inventory is the exact defect this
                // packet fixes. Report what is actually renderable and whether it
                // is the settled order or a provisional one.
                "display_count": self.media_display_cache.len(),
                "display_provenance": if self.media_display_cache.is_empty() {
                    "empty"
                } else if self.media_display_cache_key.is_some() {
                    "settled"
                } else {
                    "provisional"
                },
                "scan_error": self.compare_lanes.first().map(|lane| lane.scan_error.as_str()).unwrap_or_default(),
                "scan_diagnostics": &self.media_scan_diagnostics,
                "ui_frame_diagnostics": {
                    "last_us": self.media_ui_frame_last_us,
                    "max_us": self.media_ui_frame_max_us,
                },
            })
        } else if matches!(cmd.command, CommandKind::MediaSearch { .. }) {
            // WP-066: make filtering provable without foregrounding — which
            // terms were additive, which were subtractive, the active scope, and
            // how many rows survived. An empty result caused by subtraction is
            // otherwise indistinguishable from a broken query.
            let parsed = crate::media_search::parse_query(&self.media_search_query);
            let inventory = self
                .compare_lanes
                .first()
                .map(|lane| lane.files.len())
                .unwrap_or(0);
            // The display worker has NOT re-ranked yet in this frame, so the
            // published order still belongs to the previous query. Reporting its
            // length as matched_count made every search receipt lag by exactly
            // one intent (no-context Manual audit, finding D). Report a count
            // only when the published order actually belongs to this query;
            // otherwise say so, rather than returning a confidently wrong number.
            let settled_for_this_query = self
                .media_display_cache_key
                .as_ref()
                .is_some_and(|key| {
                    key.query == self.media_search_query
                        && key.search_folder_only == self.media_search_folder_only
                });
            let matched = self.media_display_cache.len();
            serde_json::json!({
                "scan_diagnostics": &self.media_scan_diagnostics,
                // query_diagnostics and search_status describe the ranking that
                // has actually completed. Immediately after a query change that
                // is the PREVIOUS query, so emitting them beside a correct
                // "applied" status handed a model a plausible wrong number
                // (second no-context Manual audit). Withhold them until they
                // describe this query.
                "query_diagnostics": settled_for_this_query
                    .then(|| serde_json::to_value(&self.media_query_diagnostics).ok())
                    .flatten(),
                "search_status": settled_for_this_query
                    .then(|| self.media_search_status.clone()),
                "media_io_diagnostics": self.media_io.diagnostics(),
                "search_scope": if self.media_search_folder_only { "folder" } else { "tab" },
                "search_terms": {
                    "text": parsed.text,
                    "tags": parsed.tags,
                    "labels": parsed.labels,
                    "notes": parsed.notes_contain,
                    "kinds": parsed.kinds.iter().map(|kind| match kind {
                        crate::media_search::MediaKindFilter::Image => "image",
                        crate::media_search::MediaKindFilter::Video => "video",
                    }).collect::<Vec<_>>(),
                    "favorite": parsed.favorite,
                },
                "search_excluded": {
                    "tags": parsed.excluded.tags,
                    "labels": parsed.excluded.labels,
                    "notes": parsed.excluded.notes_contain,
                    "kinds": parsed.excluded.kinds.iter().map(|kind| match kind {
                        crate::media_search::MediaKindFilter::Image => "image",
                        crate::media_search::MediaKindFilter::Video => "video",
                    }).collect::<Vec<_>>(),
                    "words": parsed.excluded.words,
                },
                "matched_count": settled_for_this_query.then_some(matched),
                "excluded_count": settled_for_this_query
                    .then(|| inventory.saturating_sub(matched)),
                // When false, ranking for this query is still in flight; poll
                // media_tabs --action list and read display_count once
                // display_provenance reports "settled".
                "counts_settled": settled_for_this_query,
                "inventory_count": inventory,
                "ui_frame_diagnostics": {
                    "last_us": self.media_ui_frame_last_us,
                    "max_us": self.media_ui_frame_max_us,
                }
            })
        } else {
            serde_json::json!({
                "scan_diagnostics": &self.media_scan_diagnostics,
                "query_diagnostics": &self.media_query_diagnostics,
                "media_io_diagnostics": self.media_io.diagnostics(),
                "search_status": &self.media_search_status,
                "ui_frame_diagnostics": {
                    "last_us": self.media_ui_frame_last_us,
                    "max_us": self.media_ui_frame_max_us,
                }
            })
        };
        let snapshot = self.current_state_snapshot();
        let kind = cmd.command.id_str().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let status = if applied {
            api::ActionStatus::Applied
        } else {
            api::ActionStatus::Rejected
        };
        let receipt = api::Receipt {
            action_id: cmd.action_id.clone(),
            kind: kind.clone(),
            status,
            actor: cmd.actor.clone(),
            protocol_version: cmd.protocol_version,
            started_at: now.clone(),
            finished_at: now,
            result: intent_result,
            error: if applied { None } else { Some(message.clone()) },
            note: Some(message.clone()),
        };

        let snapshot_value = serde_json::to_value(&snapshot).unwrap_or(serde_json::Value::Null);

        let persistence_error = match self.service.lock() {
            Ok(mut svc) => {
                let result = api::mark_intent_applied(&mut svc, &self.api_paths, &receipt);
                svc.record_applied_action(&cmd.action_id, &kind, applied, &message, snapshot_value);
                result.err().map(|error| error.to_string())
            }
            Err(_) => Some("service lock unavailable while finalizing UI intent".to_string()),
        };

        self.last_applied_action = Some(match persistence_error.as_deref() {
            Some(error) => {
                eprintln!("UI intent {} finalization failed: {error}", cmd.action_id);
                format!(
                    "{} intent={} applied={} persistence_error={} :: {}",
                    cmd.action_id, kind, applied, error, message
                )
            }
            None => format!(
                "{} intent={} applied={} :: {}",
                cmd.action_id, kind, applied, message
            ),
        });
        self.last_receipt = serde_json::to_string_pretty(&receipt).ok();

        applied
    }

    /// Apply one decoded Command (ui-intent CommandKind) to UI/service state.
    /// Returns (applied: bool, message: String) used for the receipt + event.
    fn apply_ui_intent(&mut self, ctx: &egui::Context, cmd: &ApiCommand) -> (bool, String) {
        match &cmd.command {
            CommandKind::SetProject { project_name } => {
                self.project_name = project_name.clone();
                (true, format!("project set to {project_name}"))
            }
            CommandKind::SetWorktree { worktree_path } => {
                self.worktree_path = worktree_path.clone();
                (true, format!("worktree set to {worktree_path}"))
            }
            CommandKind::SelectTab { tab } => match Tab::from_vocab(tab) {
                Some(t) => {
                    self.active_tab = t;
                    if t == Tab::Manual {
                        self.show_manual = true;
                    }
                    (true, format!("tab set to {tab}"))
                }
                None => (false, format!("unknown tab vocab: {tab}")),
            },
            CommandKind::SetFeatures { feature_keys } => {
                let known: HashSet<&str> = self
                    .feature_rows
                    .iter()
                    .map(|row| row.key.as_str())
                    .collect();
                let mut accepted = Vec::new();
                let mut dropped = Vec::new();
                for key in feature_keys {
                    if known.contains(key.as_str()) {
                        accepted.push(key.clone());
                    } else {
                        dropped.push(key.clone());
                    }
                }
                self.selected_features = accepted.iter().cloned().collect();
                if dropped.is_empty() {
                    (true, format!("selected {} features", accepted.len()))
                } else {
                    (
                        true,
                        format!(
                            "selected {} features; dropped unknown: {}",
                            accepted.len(),
                            dropped.join(",")
                        ),
                    )
                }
            }
            CommandKind::SetInPlace { in_place } => {
                self.in_place = *in_place;
                (true, format!("in_place set to {in_place}"))
            }
            CommandKind::ImportPaths {
                project_name,
                paths,
                in_place,
            } => {
                self.project_name = project_name.clone();
                self.in_place = *in_place;
                self.ingest_images(paths.clone());
                (true, self.import_summary.clone())
            }
            CommandKind::StartRunUi => {
                if self.running_pipeline {
                    (false, "pipeline already running".to_string())
                } else if self.selected_features.is_empty() {
                    (false, "no features selected".to_string())
                } else {
                    self.execute_pipeline();
                    (true, self.pipeline_status.clone())
                }
            }
            CommandKind::UiSnapshot { .. } => (
                false,
                "ui_snapshot must be handled by the renderer capture path".to_string(),
            ),
            // media browser intents (WP-042): drive the front surface headlessly.
            CommandKind::MediaSetFolder { path } => {
                if self.compare_lanes.is_empty() {
                    self.compare_lanes = vec![CompareLane::new(0)];
                    self.compare_next_lane_id = 1;
                }
                let lane_id = self.compare_lanes[0].id;
                let folder = sanitize_folder_input(path);
                if folder.is_empty() {
                    (false, "media folder path is empty".to_string())
                } else if self.media_tabs.active().viewport.kind
                    == crate::media_tabs::MediaTabKind::Collection
                {
                    // The Favorites tab has no folder. Silently "succeeding"
                    // here reported "scanning" while nothing changed
                    // (no-context Manual audit, finding A).
                    (
                        false,
                        format!(
                            "active tab {} is the Favorites collection tab and has no folder; \
                             select a folder tab (media_tabs --action select --tab-id ID) or \
                             open one (media_tabs --action open --path {folder})",
                            self.media_tabs.active_id().as_str()
                        ),
                    )
                } else {
                    self.active_tab = Tab::Media;
                    if let Some(pos) = self.compare_lane_position(lane_id) {
                        self.compare_lanes[pos].folder = folder.clone();
                    }
                    self.start_compare_scan(lane_id);
                    (true, format!("media folder set to {folder}; scanning"))
                }
            }
            CommandKind::MediaSearch { query, mode } => {
                // Mode vocab maps onto the 3-mode ranker (WP-047): tags/notes
                // become filter chips so those intents keep working.
                let (mode_index, effective_query) = match mode.as_deref() {
                    None | Some("name") => (0, query.clone()),
                    Some("fuzzy") => (1, query.clone()),
                    Some("semantic") => (2, query.clone()),
                    Some("tags") => (
                        0,
                        format!(
                            "tag:{}",
                            crate::media_search::quote_chip_value(query.trim())
                        ),
                    ),
                    Some("notes") => (
                        0,
                        format!(
                            "note:{}",
                            crate::media_search::quote_chip_value(query.trim())
                        ),
                    ),
                    Some(other) => {
                        return (false, format!("unknown media search mode: {other}"));
                    }
                };
                self.active_tab = Tab::Media;
                self.media_search_query = effective_query.clone();
                self.media_search_mode = mode_index;
                (
                    true,
                    format!(
                        "media search set: query='{effective_query}' mode={}",
                        ["name", "fuzzy", "semantic"][mode_index]
                    ),
                )
            }
            CommandKind::MediaSelect { paths } => {
                if self.compare_lanes.is_empty() {
                    return (false, "media surface has no lane".to_string());
                }
                let lane_id = self.compare_lanes[0].id;
                if self.compare_lane_position(lane_id).is_none() {
                    return (false, "media lane missing".to_string());
                }
                // Separator + casing insensitive matching: models naturally
                // send forward-slash paths while scans produce native ones.
                let wanted: HashSet<String> = paths
                    .iter()
                    .map(|p| sanitize_folder_input(p).replace('\\', "/").to_lowercase())
                    .collect();
                let matched = self.media_select_paths(lane_id, paths);
                let missed = wanted.len().saturating_sub(matched);
                (
                    matched > 0 || wanted.is_empty(),
                    format!(
                        "selected {matched} of {} requested paths ({missed} not in current folder)",
                        wanted.len()
                    ),
                )
            }
            CommandKind::MediaOpenSelected => {
                if self.compare_lanes.is_empty() {
                    return (false, "media surface has no lane".to_string());
                }
                let lane_id = self.compare_lanes[0].id;
                match self.compare_lane_open_selected_with_system(lane_id) {
                    Ok((opened, failed)) => (
                        failed == 0 && opened > 0,
                        format!("external media handoff: opened={opened} failed={failed}"),
                    ),
                    Err(error) => (false, error),
                }
            }
            CommandKind::MediaTabs {
                action,
                tab_id,
                path,
            } => {
                self.active_tab = Tab::Media;
                let result = match action.as_str() {
                    "list" => Ok(format!(
                        "{} media tabs; active={}",
                        self.media_tabs.tabs().len(),
                        self.media_tabs.active_id().as_str()
                    )),
                    // The backend media_labels_list command cannot run while the
                    // GUI holds the exclusive media database lock, and there was
                    // no read-only ui-intent to list labels — so a model could
                    // not discover the label ID that open_collection needs
                    // without MUTATING the catalog (no-context Manual audit,
                    // finding B).
                    "labels" => Ok(format!(
                        // The catalog is ALSO emitted as a structured
                        // `label_catalog` array in the receipt result; a model
                        // must never have to regex an ID out of this sentence
                        // (second no-context Manual audit).
                        "{} labels: {}",
                        self.media_label_definitions.len(),
                        self.media_label_definitions
                            .iter()
                            .map(|definition| format!(
                                "{}='{}'({})",
                                definition.id,
                                definition.name,
                                self.media_label_usage_counts
                                    .get(&definition.id)
                                    .copied()
                                    .unwrap_or(0)
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    "select" => tab_id
                        .as_deref()
                        .ok_or_else(|| "media tab select requires tab_id".to_string())
                        .and_then(|id| self.activate_media_tab(id))
                        .map(|()| {
                            format!("active media tab={}", self.media_tabs.active_id().as_str())
                        }),
                    "open" => {
                        let target = path
                            .as_deref()
                            .map(sanitize_folder_input)
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| {
                                sanitize_folder_input(
                                    &self.media_explorer.folder_navigator_location,
                                )
                            });
                        self.open_media_folder_in_new_tab(&target)
                            .map(|id| format!("opened media tab={id}"))
                    }
                    "close" => tab_id
                        .as_deref()
                        .ok_or_else(|| "media tab close requires tab_id".to_string())
                        .and_then(|id| self.close_media_tab(id))
                        .map(|active| format!("closed media tab; active={active}")),
                    // WP-066/WP-068: per-tab search scope and per-tab ordering
                    // are new operator controls, so they need receipt-backed
                    // model equivalents (FACIAL-MODEL-001) — a model cannot
                    // click a toolbar toggle.
                    "set_scope" => {
                        let wanted = path.as_deref().unwrap_or("tab");
                        let folder_only = match wanted {
                            "folder" => true,
                            "tab" | "tree" => false,
                            other => {
                                return (
                                    false,
                                    format!("unknown search scope: {other} (folder|tab)"),
                                )
                            }
                        };
                        let before = self
                            .compare_lanes
                            .first()
                            .map(|lane| (lane.scan_id, lane.files.len()));
                        self.media_search_folder_only = folder_only;
                        self.touch_media_settings();
                        let after = self
                            .compare_lanes
                            .first()
                            .map(|lane| (lane.scan_id, lane.files.len()));
                        // The whole point of separating scope from the Tree flag
                        // is that scope never rescans. Prove it in the receipt —
                        // also as structured fields, so proving it does not
                        // require parsing this sentence (audit finding I).
                        self.media_last_scope_change = Some((
                            before.map(|b| b.0) == after.map(|a| a.0),
                            before.map(|b| b.1) == after.map(|a| a.1),
                        ));
                        Ok(format!(
                            "search scope={wanted} scan_unchanged={} inventory_unchanged={}",
                            before.map(|b| b.0) == after.map(|a| a.0),
                            before.map(|b| b.1) == after.map(|a| a.1)
                        ))
                    }
                    "set_sort" => {
                        let raw = path.as_deref().unwrap_or_default().to_ascii_lowercase();
                        let (key, descending) = match raw.split_once(':') {
                            Some((key, dir)) => (key.to_string(), dir == "desc"),
                            None => (raw.clone(), false),
                        };
                        let sort = match key.as_str() {
                            "name" => crate::media_explorer::MediaSort::Name,
                            "modified" => crate::media_explorer::MediaSort::Modified,
                            "size" => crate::media_explorer::MediaSort::Size,
                            "created" => crate::media_explorer::MediaSort::Created,
                            other => {
                                return (
                                    false,
                                    format!(
                                        "unknown sort key: {other} (name|modified|size|created, \
                                         optionally :asc or :desc)"
                                    ),
                                )
                            }
                        };
                        // A collection tab's rows come from the metadata cache
                        // and carry no stat sidecar, so a stat-dependent key
                        // would silently order by name while claiming otherwise.
                        // Reporting success and changing nothing is worse than
                        // refusing (no-context Manual audit, finding C).
                        let collection = self.media_tabs.active().viewport.kind
                            == crate::media_tabs::MediaTabKind::Collection;
                        if collection && sort.needs_stat() {
                            return (
                                false,
                                format!(
                                    "sort key {key} needs file metadata that the Favorites \
                                     collection tab does not collect; use name, or select a \
                                     folder tab"
                                ),
                            );
                        }
                        self.media_explorer.sort = sort;
                        self.media_explorer.sort_desc = descending;
                        self.media_tabs.active_mut().viewport.sort = match sort {
                            crate::media_explorer::MediaSort::Name => MediaTabSort::Name,
                            crate::media_explorer::MediaSort::Modified => MediaTabSort::Modified,
                            crate::media_explorer::MediaSort::Size => MediaTabSort::Size,
                            crate::media_explorer::MediaSort::Created => MediaTabSort::Created,
                        };
                        self.media_tabs.active_mut().viewport.sort_descending = descending;
                        self.touch_media_settings();
                        if collection {
                            let lane_id =
                                self.compare_lanes.first().map(|lane| lane.id).unwrap_or(0);
                            self.materialize_media_collection_tab(lane_id);
                        }
                        Ok(format!(
                            "sort={key} descending={descending} tab={}",
                            self.media_tabs.active_id().as_str()
                        ))
                    }
                    // WP-067: the favourites/labels collection tab needs a
                    // receipt-backed intent, not only a keyboard binding, so a
                    // model can reach and prove it (FACIAL-MODEL-001).
                    "open_collection" => {
                        let raw = path.as_deref().unwrap_or("fav_videos");
                        // `labels:<label-id>` selects a label in the same call,
                        // otherwise the labels view opens with nothing chosen
                        // and a model cannot reach a label's files (WP-067).
                        let (view_key, label_id) = match raw.split_once(':') {
                            Some((view, label)) => (view, Some(label.to_string())),
                            None => (raw, None),
                        };
                        let parsed = match view_key {
                            "fav_videos" | "" => {
                                Some(crate::media_tabs::MediaCollectionView::FavoriteVideos)
                            }
                            "fav_images" => {
                                Some(crate::media_tabs::MediaCollectionView::FavoriteImages)
                            }
                            "labels" => Some(crate::media_tabs::MediaCollectionView::Labels),
                            _ => None,
                        };
                        if let (Some(crate::media_tabs::MediaCollectionView::Labels), Some(id)) =
                            (parsed, label_id.as_deref())
                        {
                            if !self
                                .media_label_definitions
                                .iter()
                                .any(|definition| definition.id == id)
                            {
                                return (false, format!("unknown label id: {id}"));
                            }
                        }
                        match parsed {
                            Some(view) => self.open_media_collection_tab().map(|id| {
                                self.media_tabs.active_mut().viewport.collection_view = view;
                                if let Some(label) = label_id {
                                    self.media_tabs.active_mut().viewport.collection_label_id =
                                        label;
                                }
                                let lane_id =
                                    self.compare_lanes.first().map(|lane| lane.id).unwrap_or(0);
                                self.materialize_media_collection_tab(lane_id);
                                let count = self
                                    .compare_lanes
                                    .first()
                                    .map(|lane| lane.files.len())
                                    .unwrap_or(0);
                                format!("collection tab={id} view={view:?} items={count}")
                            }),
                            None => Err(format!(
                                "unknown collection view: {raw} (fav_videos|fav_images|labels[:LABEL_ID])"
                            )),
                        }
                    }
                    other => Err(format!("unknown media tabs action: {other}")),
                };
                match result {
                    Ok(message) => (true, message),
                    Err(error) => (false, error),
                }
            }
            CommandKind::MediaFolderNavigate { action } => {
                if self.compare_lanes.is_empty() {
                    self.compare_lanes = vec![CompareLane::new(0)];
                    self.compare_next_lane_id = 1;
                }
                self.active_tab = Tab::Media;
                let lane_id = self.compare_lanes[0].id;
                let requires_open = !matches!(action.as_str(), "open" | "toggle" | "close");
                // WP-064: the navigator is also "active" while its pre-open
                // backdrop capture is in flight, during which show_folder_navigator
                // is deliberately false. Rejecting actions in that window made
                // every navigator command a coin flip against capture latency.
                if requires_open && !self.media_folder_navigator_active() {
                    return (
                        false,
                        "folder navigator is closed; send action=open first".to_string(),
                    );
                }
                // A queued action implies the operator/model wants the navigator
                // usable now. Resolve any in-flight capture to an open navigator
                // before applying the action so staged state is well defined.
                if requires_open && !self.media_explorer.show_folder_navigator {
                    self.settle_media_folder_navigator_capture(lane_id);
                }
                let mut render_request = CompareLaneRenderRequest::default();
                let mut action_applied = true;
                match action.as_str() {
                    "open" => {
                        if !self.media_explorer.show_folder_navigator {
                            self.open_media_folder_navigator_without_capture(lane_id);
                        }
                    }
                    "close" => self.close_media_folder_navigator(),
                    "toggle" => self.media_toggle_folder_navigator(ctx, lane_id),
                    "up" => self.media_navigator_move(lane_id, -1),
                    "down" => self.media_navigator_move(lane_id, 1),
                    "page_up" => self.media_navigator_move(lane_id, -8),
                    "page_down" => self.media_navigator_move(lane_id, 8),
                    "home" => {
                        self.media_explorer.folder_cursor =
                            (!self.media_folder_entries(lane_id).is_empty()).then_some(0);
                        self.media_explorer.folder_scroll_to_cursor = true;
                    }
                    "end" => {
                        self.media_explorer.folder_cursor =
                            self.media_folder_entries(lane_id).len().checked_sub(1);
                        self.media_explorer.folder_scroll_to_cursor = true;
                    }
                    "enter" => self.media_navigator_enter(lane_id),
                    "parent" => self.media_navigator_parent_or_close(lane_id),
                    "refresh" => render_request.refresh = true,
                    "commit" => {
                        action_applied =
                            self.media_navigator_commit_current(lane_id, &mut render_request);
                    }
                    "open_new_tab" => {
                        let target =
                            sanitize_folder_input(&self.media_explorer.folder_navigator_location);
                        if target.is_empty() {
                            action_applied = false;
                        } else {
                            render_request.open_folder_in_new_tab = Some(target);
                        }
                    }
                    _ => return (false, format!("unknown folder navigator action: {action}")),
                }
                if render_request.scan || render_request.refresh {
                    self.start_compare_scan(lane_id);
                }
                if let Some(path) = render_request.open_folder_in_new_tab.as_deref() {
                    action_applied = match self.open_media_folder_in_new_tab(path) {
                        Ok(_) => {
                            self.close_media_folder_navigator();
                            true
                        }
                        Err(error) => {
                            // WP-064: a failed commit must still release the
                            // modal. Leaving it open behind its blurred backdrop
                            // strands the operator with no route out.
                            self.compare_action_message = error;
                            self.close_media_folder_navigator();
                            false
                        }
                    };
                }
                (
                    action_applied,
                    format!(
                        "folder navigator action={action} open={} cursor={} staged='{}' active='{}' scan_requested={}",
                        self.media_explorer.show_folder_navigator,
                        self.media_explorer
                            .folder_cursor
                            .map(|index| index.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                        self.media_explorer.folder_navigator_location,
                        self.compare_lane_position(lane_id)
                            .map(|pos| sanitize_folder_input(&self.compare_lanes[pos].folder))
                            .unwrap_or_default(),
                        render_request.scan,
                    ),
                )
            }
            CommandKind::MediaVideoControl {
                action,
                value,
                output,
            } => {
                if self.compare_lanes.is_empty() {
                    return (false, "media surface has no lane".to_string());
                }
                self.active_tab = Tab::Media;
                let lane_id = self.compare_lanes[0].id;
                let selected = self.media_selected_path(lane_id);
                let selected_video = selected
                    .as_deref()
                    .filter(|path| crate::media_explorer::is_video_path(path));
                let result: Result<String, String> = match action.as_str() {
                    "status" => self
                        .video_player
                        .active_path()
                        .map(|path| format!("video status for {path}"))
                        .ok_or_else(|| "no embedded video is loaded".to_string()),
                    "play_pause" => selected_video
                        .ok_or_else(|| "selected item is not a video".to_string())
                        .and_then(|path| {
                            if self.video_player.active_path() == Some(path) {
                                self.video_player.toggle_pause()?;
                            } else {
                                self.play_media_video(Path::new(path))?;
                                self.begin_media_playback_priority();
                            }
                            Ok("video play/pause applied".to_string())
                        }),
                    "play" => selected_video
                        .ok_or_else(|| "selected item is not a video".to_string())
                        .and_then(|path| {
                            if self.video_player.active_path() != Some(path) {
                                self.play_media_video(Path::new(path))?;
                                self.begin_media_playback_priority();
                            } else {
                                self.video_player.set_playing(true)?;
                            }
                            Ok("video playing".to_string())
                        }),
                    "play_library" => selected_video
                        .ok_or_else(|| "selected item is not a video".to_string())
                        .and_then(|path| {
                            if self.video_player.active_path() != Some(path) {
                                self.play_media_video(Path::new(path))?;
                            } else {
                                self.video_player.set_playing(true)?;
                            }
                            self.begin_media_playback_priority();
                            Ok("video playing in Library panel".to_string())
                        }),
                    "pause" => {
                        if self.video_player.active_path().is_none() {
                            Err("no embedded video is loaded".to_string())
                        } else {
                            self.video_player
                                .set_playing(false)
                                .map(|()| "video paused".to_string())
                        }
                    }
                    "stop" => {
                        self.video_player.stop();
                        self.media_inline_video_path = None;
                        self.media_inline_video_requested_at = None;
                        self.media_inline_video_pending_target = None;
                        self.media_playback_lease = None;
                        Ok("video stopped".to_string())
                    }
                    "seek_ms" => value
                        .ok_or_else(|| "seek_ms requires value".to_string())
                        .and_then(|milliseconds| {
                            if self.video_player.active_path().is_none() {
                                Err("no embedded video is loaded".to_string())
                            } else {
                                self.video_player.set_time(milliseconds)?;
                                Ok(format!("video seeked to {} ms", milliseconds.max(0)))
                            }
                        }),
                    "volume" => value
                        .ok_or_else(|| "volume requires value".to_string())
                        .and_then(|volume| {
                            if self.video_player.active_path().is_none() {
                                Err("no embedded video is loaded".to_string())
                            } else {
                                let volume = volume.clamp(0, 125) as i32;
                                self.video_player.set_volume(volume)?;
                                Ok(format!("video volume set to {volume}"))
                            }
                        }),
                    "audio_track" => value
                        .ok_or_else(|| "audio_track requires value".to_string())
                        .and_then(|track| {
                            if self.video_player.active_path().is_none() {
                                Err("no embedded video is loaded".to_string())
                            } else {
                                self.video_player.set_audio_track(track as i32)?;
                                Ok(format!("audio track set to {track}"))
                            }
                        }),
                    "subtitle_track" => value
                        .ok_or_else(|| "subtitle_track requires value".to_string())
                        .and_then(|track| {
                            if self.video_player.active_path().is_none() {
                                Err("no embedded video is loaded".to_string())
                            } else {
                                self.video_player.set_subtitle_track(track as i32)?;
                                Ok(format!("subtitle track set to {track}"))
                            }
                        }),
                    "loop" => value
                        .ok_or_else(|| "loop requires value 0 or 1".to_string())
                        .and_then(|enabled| {
                            if !matches!(enabled, 0 | 1) {
                                return Err("loop requires value 0 (off) or 1 (on)".to_string());
                            }
                            let enabled = enabled != 0;
                            self.video_player.set_loop(enabled)?;
                            self.media_explorer.video_loop = enabled;
                            self.touch_media_settings();
                            Ok(format!(
                                "video loop {}",
                                if enabled { "enabled" } else { "disabled" }
                            ))
                        }),
                    "capture_frame" => {
                        let path = self.media_video_capture_path(output.as_deref(), &cmd.action_id);
                        self.video_player
                            .capture_frame(&path)
                            .map(|()| format!("video frame captured: {}", path.to_string_lossy()))
                    }
                    _ => Err(format!("unknown video action: {action}")),
                };
                if result.is_ok()
                    && matches!(
                        action.as_str(),
                        "play_pause"
                            | "play"
                            | "play_library"
                            | "pause"
                            | "seek_ms"
                            | "volume"
                            | "audio_track"
                            | "subtitle_track"
                    )
                {
                    // A paused remote video still needs a short quiet period
                    // around direct interaction. The next paused-state sync
                    // drops this lease and starts coordinator hysteresis.
                    self.begin_media_playback_priority();
                }
                match result {
                    Ok(message) => {
                        match explicit_video_owner(action) {
                            ExplicitVideoOwner::Library => {
                                self.media_inline_video_path = selected_video.map(str::to_string);
                                self.media_inline_video_requested_at =
                                    Some(std::time::Instant::now());
                                if let Some(path) = selected_video {
                                    self.set_pending_inline_video_target(path);
                                } else {
                                    self.media_inline_video_pending_target = None;
                                }
                            }
                            ExplicitVideoOwner::Viewer => {
                                self.media_inline_video_path = None;
                                self.media_inline_video_requested_at = None;
                                self.media_inline_video_pending_target = None;
                            }
                            ExplicitVideoOwner::Preserve => {}
                        }
                        (true, message)
                    }
                    Err(error) => (false, error),
                }
            }
            CommandKind::MediaLabelMutation {
                action,
                path,
                id,
                name,
                hex,
                confirmed,
            } => {
                let result: Result<String, String> = match action.as_str() {
                    "create" => {
                        let name = name
                            .as_deref()
                            .ok_or_else(|| "label create requires name".to_string());
                        let hex = hex
                            .as_deref()
                            .ok_or_else(|| "label create requires hex".to_string());
                        name.and_then(|name| {
                            hex.and_then(|hex| {
                                if let Some(path) = path.as_deref() {
                                    self.media_db.create_color_label_and_assign(path, name, hex)
                                } else {
                                    self.media_db.create_color_label(name, hex)
                                }
                                .map(|definition| format!("label {} created", definition.id))
                            })
                        })
                    }
                    "update" => id
                        .as_deref()
                        .ok_or_else(|| "label update requires id".to_string())
                        .and_then(|id| {
                            self.media_db
                                .update_color_label(id, name.as_deref(), hex.as_deref())
                                .map(|definition| format!("label {} updated", definition.id))
                        }),
                    "delete" => id
                        .as_deref()
                        .ok_or_else(|| "label delete requires id".to_string())
                        .and_then(|id| {
                            self.media_db
                                .delete_color_label(id, *confirmed)
                                .map(|result| {
                                    format!(
                                        "label {} deleted; {} assignments removed",
                                        result.id, result.assignments_removed
                                    )
                                })
                        }),
                    "add" => path
                        .as_deref()
                        .ok_or_else(|| "label add requires path".to_string())
                        .and_then(|path| {
                            id.as_deref()
                                .ok_or_else(|| "label add requires id or name".to_string())
                                .and_then(|id| self.media_db.add_label(path, id))
                                .map(|labels| format!("labels assigned: {}", labels.join(", ")))
                        }),
                    "remove" => path
                        .as_deref()
                        .ok_or_else(|| "label remove requires path".to_string())
                        .and_then(|path| {
                            id.as_deref()
                                .ok_or_else(|| "label remove requires id or name".to_string())
                                .and_then(|id| self.media_db.remove_label(path, id))
                                .map(|labels| format!("labels assigned: {}", labels.join(", ")))
                        }),
                    "clear" => path
                        .as_deref()
                        .ok_or_else(|| "label clear requires path".to_string())
                        .and_then(|path| {
                            self.media_db
                                .clear_labels(path)
                                .map(|()| "labels cleared".to_string())
                        }),
                    other => Err(format!("unknown label mutation action: {other}")),
                };
                match result {
                    Ok(message) => {
                        self.load_media_metadata();
                        self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
                        (true, message)
                    }
                    Err(error) => (false, error),
                }
            }
            other => (
                false,
                format!("not a ui-intent (backend command): {}", other.id_str()),
            ),
        }
    }

    /// Build api::AppStateSnapshot from current FacialApp + service state.
    fn current_state_snapshot(&mut self) -> api::AppStateSnapshot {
        self.snapshot_active_media_tab();
        let (models, plugins, worktrees, lanes) = match self.service.lock() {
            Ok(mut svc) => {
                let models = svc.list_models();
                let plugins = svc
                    .list_plugins()
                    .into_iter()
                    .filter_map(|value| {
                        serde_json::from_value::<crate::plugin_host::PluginManifest>(value).ok()
                    })
                    .collect::<Vec<_>>();
                let mut worktrees = BTreeMap::new();
                for (project, runs) in svc.list_worktrees() {
                    worktrees.insert(
                        project,
                        runs.into_iter()
                            .map(|path| path.to_string_lossy().to_string())
                            .collect(),
                    );
                }
                let lanes = svc.list_lanes().unwrap_or_default();
                (models, plugins, worktrees, lanes)
            }
            Err(_) => (Vec::new(), Vec::new(), BTreeMap::new(), Vec::new()),
        };

        let mut selected: Vec<String> = self.selected_features.iter().cloned().collect();
        selected.sort();

        api::AppStateSnapshot {
            protocol_version: api::API_PROTOCOL_VERSION,
            captured_at: chrono::Utc::now().to_rfc3339(),
            repo_root: self.config.repo_root.to_string_lossy().to_string(),
            workspace_root: self.config.workspace_root.to_string_lossy().to_string(),
            worktrees_root: self.config.worktrees_root.to_string_lossy().to_string(),
            api_root: self.config.api_root.to_string_lossy().to_string(),
            ingest_in_place_default: self.config.ingest_in_place_default,
            models,
            plugins,
            worktrees,
            lanes,
            active_tab: self.active_tab.vocab().to_string(),
            project_name: self.project_name.clone(),
            worktree_path: self.worktree_path.clone(),
            in_place: self.in_place,
            selected_features: selected,
            running_pipeline: self.running_pipeline,
            run_output: self.run_output.clone(),
            media_tabs: serde_json::json!({
                "active_tab_id": self.media_tabs.active_id().as_str(),
                "tabs": self.media_tabs.tabs().iter().map(|tab| {
                    let path = if tab.viewport.folder_key.is_empty() {
                        String::new()
                    } else {
                        self.media_db.path_for_key(&tab.viewport.folder_key)
                    };
                    serde_json::json!({
                        "id": tab.id.as_str(),
                        "title": crate::media_tabs::folder_tab_title(&path),
                        "folder_key": tab.viewport.folder_key,
                        "folder": path,
                        "search_query": tab.viewport.search_query,
                    })
                }).collect::<Vec<_>>(),
                "selection_restore_pending": self.media_tab_pending_selection_keys,
                "cursor_restore_pending": self.media_tab_pending_cursor_key,
                "load_status": self.media_tabs_load_status,
                "persistence_blocked": self.media_tabs_persistence_blocked,
            }),
            media_folder_navigation: serde_json::json!({
                "open": self.media_explorer.show_folder_navigator,
                "staged_folder": self.media_explorer.folder_navigator_location,
                "committed_folder": self.compare_lanes.first().map(|lane| sanitize_folder_input(&lane.folder)).unwrap_or_default(),
            }),
            media_controller: serde_json::json!({
                "gilrs_initialized": self.controller_gilrs.is_some(),
                "input_enabled": self.controller_input_enabled,
                "active_gamepad": self.controller_active.map(|id| format!("{id:?}")),
                "legacy_active": self.controller_legacy_active,
                "input_source": self.controller_input_source,
                "input_device_id": self.controller_input_device_id,
                "input_device_name": self.controller_input_device_name,
                "gamepads": self.controller_gilrs.as_ref().map(|gilrs| gilrs.gamepads().map(|(id, pad)| serde_json::json!({
                    "id": format!("{id:?}"),
                    "name": pad.name(),
                    "connected": pad.is_connected(),
                })).collect::<Vec<_>>()).unwrap_or_default(),
                "pointer_mode": self.controller_pointer_mode,
            }),
            media_video: serde_json::json!({
                "available": self.video_player_available,
                "placement": if self.video_player.active_path().is_none() {
                    serde_json::Value::Null
                } else if self.media_inline_video_path.is_some() {
                    serde_json::Value::String("library".to_string())
                } else {
                    serde_json::Value::String("viewer".to_string())
                },
                "active_path": self.video_player.active_path(),
                "snapshot": self.video_player.cached_snapshot(),
                "diagnostics": self.video_player.diagnostics(),
                "last_error": self.video_player.last_error(),
            }),
        }
    }

    fn refresh_all(&mut self) {
        if let Ok(mut svc) = Arc::clone(&self.service).lock() {
            self.models = Self::load_models(&mut svc);
            self.worktree_view = Self::load_worktrees(&mut svc);
            self.feature_rows = Self::load_features(&mut svc);
            self.selected_features.clear();
        }
        self.manual_text = Self::load_manual(&self.config.repo_root);
        self.video_player_available = crate::video_player::VideoPlayer::available();
        // Give failed thumbnails another chance (files may have been fixed).
        if let Some(engine) = self.thumb_engine.as_mut() {
            engine.forget_failures();
        }
    }

    // -----------------------------------------------------------------------
    // Rendering (flat, hairline dividers, no cards)
    // -----------------------------------------------------------------------

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            theme::logomark(ui, 22.0);
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("facial")
                    .heading()
                    .strong()
                    .color(theme::ink()),
            );
            ui.add_space(14.0);
            for t in Tab::ALL {
                // Text-only tabs (WP-048): icon+label crowded the strip and
                // brutalist-minimal reads better as plain words.
                if theme::tab_item(ui, self.active_tab == t, t.label()) {
                    self.active_tab = t;
                    self.show_manual = t == Tab::Manual;
                }
                ui.add_space(10.0);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(format!("{} Refresh", icons::ARROW_CLOCKWISE))
                    .on_hover_text("Global refresh: reload models, worktrees, features, the manual, and failed thumbnail state (F5)")
                    .clicked()
                {
                    self.refresh_all();
                }
                if ui
                    .button(format!("{} Settings", icons::GEAR))
                    .on_hover_text("Unified Media and app settings")
                    .clicked()
                {
                    self.active_tab = Tab::Media;
                    self.request_media_settings(ui.ctx(), 3);
                }
            });
        });
    }

    /// Slim bottom status bar: workspace at left, live state at right.
    fn draw_status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} {}",
                        icons::FOLDER,
                        elide_middle(&self.workspace_root, 52)
                    ))
                    .small()
                    .color(theme::ink_faint()),
                )
                .wrap(false),
            )
            .on_hover_text(&self.workspace_root);
            ui.label(egui::RichText::new("·").small().color(theme::ink_faint()));
            if self.copy_location.trim().is_empty() {
                ui.label(
                    egui::RichText::new("output folder not set")
                        .small()
                        .color(theme::warn_ink()),
                );
            } else {
                ui.label(
                    egui::RichText::new("output ready")
                        .small()
                        .color(theme::ok_ink()),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.running_pipeline
                    || self
                        .compare_lanes
                        .iter()
                        .any(|lane| lane.scanning || lane.loading_image_inflight);
                let (status, color) = if busy {
                    ("working…".to_string(), theme::accent())
                } else {
                    (elide_middle(&self.pipeline_status, 64), theme::ink_faint())
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(status).small().color(color)).wrap(false),
                );
            });
        });
    }

    fn draw_project_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Project");
        ui.label(
            "Workspace root and copy-output folder are in Settings → App → Workspace settings.",
        );

        egui::CollapsingHeader::new("Project & worktrees")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Project name:");
                    ui.text_edit_singleline(&mut self.project_name);
                });
                ui.checkbox(&mut self.in_place, "Work in place / source parent");
                if self.in_place {
                    ui.label("Uses original image paths; run output goes to <source parent>/.facial/runs.");
                } else {
                    ui.label("Copies images to <copy output folder>/images; run output goes to <copy output folder>/runs.");
                }
                ui.horizontal(|ui| {
                    if ui.button("New worktree").clicked() {
                        if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                            if let Ok(path) = svc.create_project_worktree(&self.project_name) {
                                self.worktree_path = path.to_string_lossy().to_string();
                                self.worktree_view = Self::load_worktrees(&mut svc);
                            }
                        }
                    }
                });
                ui.label(format!("Current worktree: {}", self.worktree_path));
                ui.label("Worktrees (per project):");
                ScrollArea::vertical()
                    .id_source("project_worktrees")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (project, runs) in &self.worktree_view {
                            ui.collapsing(project, |ui| {
                                for run in runs {
                                    if ui
                                        .selectable_label(self.worktree_path == *run, run)
                                        .clicked()
                                    {
                                        self.worktree_path = run.clone();
                                    }
                                }
                            });
                        }
                    });
            });

        egui::CollapsingHeader::new("Import images")
            .default_open(true)
            .show(ui, |ui| {
                ui.label("Paste file or directory paths, one per line:");
                ui.add(
                    TextEdit::multiline(&mut self.import_paths_input)
                        .desired_rows(3)
                        .hint_text("C:/path/to/image.jpg\nC:/path/to/folder"),
                );
                if theme::primary_button(ui, &format!("{} Import images", icons::DOWNLOAD_SIMPLE))
                    .clicked()
                {
                    let paths: Vec<String> = self
                        .import_paths_input
                        .lines()
                        .map(|line| line.to_string())
                        .collect();
                    self.ingest_images(paths);
                }
                if !self.import_summary.trim().is_empty() {
                    ui.label(&self.import_summary);
                }
                if !self.pipeline_status.trim().is_empty() {
                    ui.label(&self.pipeline_status);
                }
            });

        egui::CollapsingHeader::new("Models")
            .default_open(true)
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .id_source("project_models")
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for entry in &self.models {
                            ui.label(entry);
                        }
                    });
                ui.label("New model:");
                ui.text_edit_singleline(&mut self.model_name);
                ui.add(TextEdit::multiline(&mut self.model_description).desired_rows(2));
                if ui.button("Add model").clicked() {
                    let name = self.model_name.trim().to_string();
                    if !name.is_empty() {
                        if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                            if svc.add_model(&name, &name, &self.model_description).is_ok() {
                                self.models = Self::load_models(&mut svc);
                            }
                        }
                    }
                }
            });
    }

    fn draw_options_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "App settings");

        // Sub-tabs: Preferences (operator settings) | Advanced / Debug (the moved
        // visual-debugger surface). Defaults to Preferences so the debug panels stay
        // out of a non-technical operator's way (WP-026).
        ui.horizontal(|ui| {
            if ui
                .selectable_label(!self.options_advanced, "Preferences")
                .clicked()
            {
                self.options_advanced = false;
            }
            if ui
                .selectable_label(self.options_advanced, "Advanced / Debug")
                .clicked()
            {
                self.options_advanced = true;
            }
        });
        theme::hairline(ui);
        if self.options_advanced {
            self.draw_advanced_debug(ui);
            return;
        }

        theme::kicker(ui, "Workspace");
        ui.label("Workspace root:");
        ui.horizontal(|ui| {
            let button_w = 150.0 + ui.spacing().item_spacing.x;
            let field_w = (ui.available_width() - button_w).max(140.0);
            ui.add_sized(
                [field_w, ui.spacing().interact_size.y],
                TextEdit::singleline(&mut self.workspace_root).clip_text(true),
            );
            if ui.button("Set workspace").clicked() {
                self.snapshot_active_media_tab();
                match self.write_media_tabs() {
                    Err(error) => {
                        self.workspace_status =
                            format!("Workspace unchanged; Media tabs save failed: {error}");
                    }
                    Ok(()) => match self.flush_media_metadata(true) {
                        Err(error) => {
                            self.workspace_status =
                                format!("Workspace unchanged; Media metadata save failed: {error}");
                        }
                        Ok(()) => match Arc::clone(&self.service).lock() {
                            Ok(mut svc) => match svc.set_workspace_root(&self.workspace_root) {
                                Ok(resolved) => {
                                    self.workspace_root = resolved;
                                    self.config = svc.config().clone();
                                    self.api_paths = ApiPaths::from_config(&self.config);
                                    let _ = self.api_paths.ensure_dirs();
                                    self.worktree_view = Self::load_worktrees(&mut svc);
                                    let ctx = ui.ctx().clone();
                                    self.reopen_media_db(&ctx);
                                    self.workspace_status = "Workspace root set".to_string();
                                }
                                Err(err) => {
                                    self.workspace_status = format!("Workspace root error: {err}");
                                }
                            },
                            Err(error) => {
                                self.workspace_status =
                                    format!("Workspace unchanged; service unavailable: {error}");
                            }
                        },
                    },
                }
            }
        });
        ui.label("Workspace root owns .facial/data and .facial/worktrees for this project.");
        ui.label("Copy output folder:");
        ui.horizontal(|ui| {
            let button_w = 72.0 + ui.spacing().item_spacing.x;
            let field_w = (ui.available_width() - button_w).max(140.0);
            ui.add_sized(
                [field_w, ui.spacing().interact_size.y],
                TextEdit::singleline(&mut self.copy_location).clip_text(true),
            );
            if ui.button("Set").clicked() {
                if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                    match svc.set_copy_location(&self.copy_location) {
                        Ok(resolved) => {
                            self.copy_location = resolved;
                            self.workspace_status = "Copy location set".to_string();
                        }
                        Err(err) => {
                            self.workspace_status = format!("Copy location error: {err}");
                        }
                    }
                }
            }
        });
        if self.copy_location.trim().is_empty() {
            ui.label("Required for copy mode, runs, and sorting.");
        }
        if !self.workspace_status.trim().is_empty() {
            ui.label(&self.workspace_status);
        }

        theme::kicker(ui, "Interface");
        ui.label("Theme");
        ui.horizontal(|ui| {
            let current = theme::mode();
            for (mode, label) in [
                (theme::Mode::Paper, "Paper (light)"),
                (theme::Mode::Ink, "Ink (dark)"),
            ] {
                if ui.selectable_label(current == mode, label).clicked() && current != mode {
                    theme::set_mode(mode);
                    theme::install_style(ui.ctx());
                    theme::apply_text_styles(ui.ctx(), self.font_size_pt);
                    let name = theme::mode_to_str(mode);
                    self.config.theme_mode = name.to_string();
                    let _ = crate::config::save_theme_mode(&self.config, name);
                }
            }
        });

        ui.add_space(4.0);
        ui.label("Font size");
        let resp = ui.add(egui::Slider::new(&mut self.font_size_pt, 12.0..=40.0).text("pt"));
        if resp.changed() {
            // Live preview while dragging.
            theme::apply_text_styles(ui.ctx(), self.font_size_pt);
        }
        if resp.drag_stopped() || resp.lost_focus() {
            // Persist so the choice survives restarts.
            let _ = crate::config::save_font_size(&self.config, self.font_size_pt);
        }
        ui.horizontal(|ui| {
            if ui.button("Reset to default (19 pt)").clicked() {
                self.font_size_pt = 19.0;
                theme::apply_text_styles(ui.ctx(), self.font_size_pt);
                let _ = crate::config::save_font_size(&self.config, self.font_size_pt);
            }
        });

        theme::kicker(ui, "Current configuration");
        ui.label(format!(
            "Settings file: {}",
            self.config
                .repo_root
                .join("product/config/default.json")
                .display()
        ));
        ui.label(format!("Font size: {:.0} pt", self.font_size_pt));
        ui.label(format!(
            "Max debug events: {}",
            self.config.max_debug_events
        ));
        ui.label(format!(
            "Ingest in-place by default: {}",
            self.config.ingest_in_place_default
        ));
        ui.label(format!(
            "Workspace root: {}",
            self.config.workspace_root.display()
        ));
        ui.label(format!(
            "Worktrees root: {}",
            self.config.worktrees_root.display()
        ));
    }

    /// Render the feature checkbox rows that belong to a given tab.
    fn draw_feature_rows_for(&mut self, ui: &mut egui::Ui, tab: Tab, scroll_id: &str) {
        let keys: Vec<(String, String)> = self
            .feature_rows
            .iter()
            .filter(|row| tab_for_feature(&row.key) == tab)
            .map(|row| (row.key.clone(), row.display.clone()))
            .collect();
        if keys.is_empty() {
            ui.label("No features mapped to this tab.");
            return;
        }
        ScrollArea::vertical()
            .id_source(scroll_id)
            .max_height(320.0)
            .show(ui, |ui| {
                for (key, display) in &keys {
                    let mut selected = self.selected_features.contains(key);
                    if ui.checkbox(&mut selected, display).clicked() {
                        if selected {
                            self.selected_features.insert(key.clone());
                        } else {
                            self.selected_features.remove(key);
                        }
                    }
                }
            });
    }

    fn draw_quality_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Quality & IQ");
        theme::hairline(ui);
        ui.label("Facet quality/composition/faces passes, all python-ofiq, all ediffiqa.");
        self.draw_feature_rows_for(ui, Tab::QualityIq, "quality_rows");
    }

    fn draw_identity_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Identity");

        theme::kicker(ui, "Identity engine");
        ui.horizontal(|ui| {
            ui.label("Model path (ArcFace ONNX):");
            ui.add(
                TextEdit::singleline(&mut self.identity_model_path)
                    .hint_text("path/to/w600k_r50.onnx")
                    .desired_width((ui.available_width() - 8.0).clamp(200.0, 560.0)),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Detector path (YuNet ONNX, optional):");
            ui.add(
                TextEdit::singleline(&mut self.identity_detector_path)
                    .hint_text("path/to/yunet_2023mar.onnx  (leave blank for resize fallback)")
                    .desired_width((ui.available_width() - 8.0).clamp(200.0, 560.0)),
            );
        });
        if ui.button("Set identity engine").clicked() {
            if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                match svc
                    .set_identity_paths(&self.identity_model_path, &self.identity_detector_path)
                {
                    Ok(info) => {
                        self.identity_engine_status = format!(
                            "loaded  sha256={}  align={}",
                            info["model_sha256"].as_str().unwrap_or("?"),
                            info["align"].as_str().unwrap_or("?")
                        );
                    }
                    Err(err) => {
                        self.identity_engine_status = format!("error: {err}");
                    }
                }
            }
        }
        if !self.identity_engine_status.is_empty() {
            ui.label(&self.identity_engine_status);
        }

        theme::hairline(ui);
        ui.label("All deepface identity features.");
        self.draw_feature_rows_for(ui, Tab::Identity, "identity_rows");
    }

    fn draw_duplicates_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Duplicates");
        theme::hairline(ui);
        ui.label("All imagededup features + facet duplicate_pass / burst_blink_pass.");
        self.draw_feature_rows_for(ui, Tab::Duplicates, "duplicate_rows");
    }

    fn draw_run_debug_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Run");
        theme::hairline(ui);

        let mut selected: Vec<String> = self.selected_features.iter().cloned().collect();
        selected.sort();
        ui.label(format!("Selected features: {}", selected.len()));
        if !selected.is_empty() {
            ui.label(selected.join(", "));
        }

        let dest_ready = !self.copy_location.trim().is_empty();
        ui.horizontal(|ui| {
            if ui.button("Refresh plugins").clicked() {
                if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                    self.feature_rows = Self::load_features(&mut svc);
                    self.selected_features.clear();
                }
            }
            if theme::primary_button_enabled(
                ui,
                dest_ready,
                &format!("{} Run selected features", icons::PLAY),
            )
            .clicked()
            {
                self.execute_pipeline();
            }
        });
        if !dest_ready {
            ui.label(
                "Set a copy output folder (Settings → App → Workspace settings) before running.",
            );
        }

        theme::hairline(ui);
        ui.label("Run output");
        ui.text_edit_singleline(&mut self.run_output);

        theme::hairline(ui);
        ui.label("Run summary");
        ScrollArea::vertical()
            .id_source("run_summary")
            .max_height(160.0)
            .show(ui, |ui| {
                if self.run_summary.trim().is_empty() {
                    ui.label("(no run yet)");
                } else {
                    ui.label(&self.run_summary);
                }
            });

        theme::kicker(ui, "Sort run into folders");
        ui.horizontal(|ui| {
            ui.label("Run id (blank = latest):");
            ui.text_edit_singleline(&mut self.sort_run_id);
        });
        ui.checkbox(
            &mut self.sort_in_parent,
            "Work in parent folder (set a path per bucket)",
        );
        if self.sort_in_parent {
            ui.horizontal(|ui| {
                ui.label("Keep folder:");
                ui.text_edit_singleline(&mut self.sort_keep_dir);
            });
            ui.horizontal(|ui| {
                ui.label("Review folder:");
                ui.text_edit_singleline(&mut self.sort_review_dir);
            });
            ui.horizontal(|ui| {
                ui.label("Cull folder:");
                ui.text_edit_singleline(&mut self.sort_cull_dir);
            });
        } else {
            ui.label("Copies into <copy output folder>/keep | review | cull (non-destructive).");
        }
        if ui.button("Sort now").clicked() {
            let run_id = if self.sort_run_id.trim().is_empty() {
                std::path::Path::new(&self.run_output)
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                self.sort_run_id.trim().to_string()
            };
            if run_id.is_empty() {
                self.sort_status = "Enter a run id (or run features first).".to_string();
            } else if let Ok(mut svc) = Arc::clone(&self.service).lock() {
                match svc.sort_run(
                    &run_id,
                    self.sort_in_parent,
                    &self.sort_keep_dir,
                    &self.sort_cull_dir,
                    &self.sort_review_dir,
                ) {
                    Ok(v) => {
                        self.sort_status = format!(
                            "Sorted run {run_id}: keep {} / review {} / cull {} (total {})",
                            v["keep"], v["review"], v["cull"], v["total"]
                        );
                    }
                    Err(err) => self.sort_status = format!("Sort error: {err}"),
                }
            }
        }
        ui.label(&self.sort_status);

        theme::hairline(ui);
        ui.label("facet diagnostics_pass:");
        self.draw_feature_rows_for(ui, Tab::RunDebug, "rundebug_feature_rows");
    }

    /// The model-drivable debug surface (event stream, last receipt, state snapshot,
    /// artifact links). Lives under Options -> Advanced / Debug so it stays out of a
    /// non-technical operator's way while remaining reachable for troubleshooting and
    /// LLM/agent drives (WP-026).
    fn draw_advanced_debug(&mut self, ui: &mut egui::Ui) {
        theme::kicker(ui, "Visual debugger (model-drivable control surface)");
        ui.label("No external windows are launched from here.");
        ui.label(format!(
            "Last applied model action: {}",
            self.last_applied_action.as_deref().unwrap_or("(none yet)")
        ));
        if let Some(receipt) = &self.last_receipt {
            ui.collapsing("Last receipt", |ui| {
                ui.monospace(receipt);
            });
        }

        theme::hairline(ui);
        ui.label("Events:");
        ScrollArea::vertical()
            .id_source("debug_events")
            .max_height(240.0)
            .show(ui, |ui| {
                ui.monospace(&self.debug_lines);
            });

        theme::hairline(ui);
        ui.collapsing("AppStateSnapshot", |ui| {
            let snapshot = self.current_state_snapshot();
            let text = serde_json::to_string_pretty(&snapshot)
                .unwrap_or_else(|_| "(snapshot serialization failed)".to_string());
            ScrollArea::vertical()
                .id_source("state_snapshot")
                .max_height(200.0)
                .show(ui, |ui| {
                    ui.monospace(text);
                });
        });

        theme::hairline(ui);
        ui.collapsing("Artifact links (run summary paths)", |ui| {
            if self.run_summary.is_empty() {
                ui.label("(no run yet)");
            } else {
                for line in self.run_summary.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("artifact:") || trimmed.starts_with("output=") {
                        ui.monospace(trimmed);
                    }
                }
            }
        });
    }

    fn draw_compare_tab(&mut self, ui: &mut egui::Ui) {
        // Slim toolbar: every saved vertical pixel goes to the images below.
        // Compare is the operator-facing visual tool; headless lanes are separate
        // command/API coordination units.
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{} Compare {}/16",
                    icons::COLUMNS,
                    self.compare_lanes.len()
                ))
                .strong()
                .color(theme::ink()),
            );
            ui.separator();
            for preset in [2usize, 4, 8] {
                let selected = self.compare_lanes.len() == preset;
                if ui
                    .selectable_label(selected, preset.to_string())
                    .on_hover_text(format!("Show {preset} compare panes"))
                    .clicked()
                {
                    self.set_compare_lane_count(preset);
                }
            }
            ui.separator();
            if ui
                .button(format!("{} Pane", icons::PLUS))
                .on_hover_text("Add one compare pane")
                .clicked()
            {
                self.add_compare_lane();
            }
            if ui
                .add_enabled(
                    self.compare_lanes.len() > 1,
                    egui::Button::new(format!("{} Pane", icons::MINUS)),
                )
                .on_hover_text("Remove the last compare pane")
                .clicked()
            {
                self.remove_compare_lane();
            }
            if ui
                .add_enabled(
                    self.compare_lanes.len() < 16,
                    egui::Button::new(format!("{} Clone", icons::COPY)),
                )
                .on_hover_text(
                    "Add a compare pane with the last pane's folder and recursive setting",
                )
                .clicked()
            {
                self.clone_last_compare_lane_setup();
            }
            ui.toggle_value(&mut self.compare_sync, "Sync panes")
                .on_hover_text("Prev/Next, arrow keys, and wheel move every compare pane together");
            let have_ref_dir = self.config.identity_reference_dir.is_some();
            let anchors_label = format!("{} Anchors", icons::USER_FOCUS);
            let anchor_resp = ui
                .add_enabled(
                    have_ref_dir,
                    egui::SelectableLabel::new(self.compare_anchors_on, anchors_label),
                )
                .on_hover_text("Pin the identity reference set above the compare panes")
                .on_disabled_hover_text(
                    "Set identity_reference_dir (or FACIAL_IDENTITY_REF_DIR) to pin anchors",
                );
            if anchor_resp.clicked() {
                self.compare_anchors_on = !self.compare_anchors_on;
            }
            if self.compare_anchors_on
                && self.compare_anchor_thumbs.is_empty()
                && !self.compare_anchors_loading
                && self.compare_anchor_error.is_empty()
            {
                self.start_anchor_load();
            }
        });
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Compare: arrow keys navigate hovered pane, wheel over an image steps, Enter or Ctrl/Cmd+O opens selected file, Ctrl/Cmd+L opens file location, Ctrl/Cmd+A select all, Ctrl/Cmd+Shift+A select none, Ctrl/Cmd+I invert, Ctrl/Cmd+C copy, Ctrl/Cmd+V paste, Delete/Backspace delete, number + Enter jumps",
                )
                .small()
                .color(theme::ink_faint()),
            )
            .wrap(false),
        );
        if !self.compare_clipboard.is_empty() {
            ui.label(
                egui::RichText::new(format!(
                    "Clipboard: {} image{}",
                    self.compare_clipboard.len(),
                    if self.compare_clipboard.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ))
                .small()
                .color(theme::ink_soft()),
            );
        }
        if !self.compare_action_message.is_empty() {
            ui.label(
                egui::RichText::new(&self.compare_action_message)
                    .small()
                    .color(theme::ink_faint()),
            );
        }
        ui.add_space(2.0);

        // Pinned anchor strip (WP-017): the ground-truth identity set stays in
        // view while judging candidates in the lanes below.
        if self.compare_anchors_on {
            theme::kicker(ui, "Anchors (identity reference set)");
            let strip_h = 100.0;
            ScrollArea::horizontal()
                .id_source("compare_anchor_strip")
                .max_height(strip_h + 10.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if self.compare_anchors_loading {
                            ui.label(
                                egui::RichText::new("loading anchors…")
                                    .small()
                                    .color(theme::ink_faint()),
                            );
                        } else if !self.compare_anchor_error.is_empty() {
                            ui.label(
                                egui::RichText::new(&self.compare_anchor_error)
                                    .small()
                                    .color(theme::error_ink()),
                            );
                        }
                        for (name, tex) in &self.compare_anchor_thumbs {
                            let size = tex.size_vec2();
                            let scale = (strip_h / size.y).min(1.0);
                            ui.add(egui::Image::new((tex.id(), size * scale)))
                                .on_hover_text(name);
                        }
                    });
                });
            ui.add_space(4.0);
        }

        let lane_ids: Vec<usize> = self.compare_lanes.iter().map(|lane| lane.id).collect();
        let lane_count = lane_ids.len().max(1);
        let spacing_x = ui.spacing().item_spacing.x;
        // Size lane cards from the cursor and the screen rect: the cards must
        // end inside the window so the footer nav row is always on screen.
        // Anchor to the central panel's max_rect (NOT the screen): the panel
        // already accounts for the header, status bar, and margins.
        let origin = ui.cursor().min;
        let bounds = ui.max_rect();
        let avail_w = (bounds.right() - origin.x).max(320.0);
        let avail_h = (bounds.bottom() - origin.y).max(300.0);
        let lane_width =
            ((avail_w - spacing_x * (lane_count - 1) as f32) / lane_count as f32).max(320.0);
        let lane_height = avail_h;

        let mut requests: Vec<(usize, CompareLaneRenderRequest)> = Vec::new();
        let mut hovered_lane: Option<usize> = None;
        let mut global_request = CompareLaneRenderRequest::default();

        ScrollArea::horizontal()
            .id_source("compare_lanes_scroll")
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for lane_id in &lane_ids {
                        // Hand-painted card on an exact rect: egui Frames size
                        // to their widest child row, which let cards overgrow
                        // the screen. Exact allocation + a hard clip keeps the
                        // card pixel-stable no matter what the rows request.
                        let (card_rect, _) = ui.allocate_exact_size(
                            egui::vec2(lane_width, lane_height),
                            Sense::hover(),
                        );
                        ui.painter().rect(
                            card_rect,
                            egui::Rounding::same(4.0),
                            theme::sheet(),
                            theme::rule_stroke(),
                        );
                        let inner = card_rect.shrink(10.0);
                        let mut card_ui = ui.child_ui(inner, egui::Layout::top_down(Align::Min));
                        card_ui.set_clip_rect(inner.intersect(card_ui.clip_rect()));
                        requests.push((
                            *lane_id,
                            self.draw_compare_lane_card(&mut card_ui, *lane_id, true),
                        ));
                        if ui.rect_contains_pointer(card_rect) {
                            hovered_lane = Some(*lane_id);
                        }
                    }
                });
            });

        // Keyboard actions while no widget holds focus: arrows for navigation and
        // explorer-like list operations (Open system app, Ctrl/Cmd + A Select all,
        // Ctrl/Cmd + C Copy, Ctrl/Cmd + V Paste, Delete/Backspace, Enter/Open).
        let (left, right) = ui.ctx().input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });
        let (
            mod_ctrl,
            mod_shift,
            copy_key,
            paste_key,
            open_file_key,
            open_location_key,
            delete_key,
            backspace_key,
            select_all_key,
            select_none_key,
            invert_key,
            enter_key,
        ) = ui.ctx().input(|i| {
            let mods = i.modifiers;
            (
                mods.ctrl || mods.command,
                mods.shift,
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::V),
                (mods.ctrl || mods.command) && i.key_pressed(egui::Key::O),
                (mods.ctrl || mods.command) && i.key_pressed(egui::Key::L),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::A) && mods.shift,
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::Enter),
            )
        });
        let active_lane_ids: Vec<usize> = if self.compare_sync {
            lane_ids.clone()
        } else if let Some(id) = hovered_lane {
            vec![id]
        } else if lane_ids.len() == 1 {
            lane_ids.clone()
        } else {
            Vec::new()
        };

        let focus_free = ui.ctx().memory(|m| m.focused().is_none());
        if focus_free && (left || right) {
            let delta: isize = if right { 1 } else { -1 };
            for id in active_lane_ids.clone() {
                self.nav_lane_relative(id, delta);
            }
        }
        if focus_free && !active_lane_ids.is_empty() {
            if open_file_key || enter_key {
                global_request.open_file = true;
            }
            if open_location_key {
                global_request.open_location = true;
            }
            if mod_ctrl && select_none_key {
                global_request.select_none = true;
            } else if mod_ctrl && select_all_key {
                global_request.select_all = true;
            }
            if mod_ctrl && invert_key {
                global_request.invert_selection = true;
            }
            if mod_ctrl && copy_key {
                global_request.copy_selected = true;
            }
            if mod_ctrl && paste_key {
                global_request.paste = true;
            }
            if delete_key || backspace_key {
                global_request.delete_selected = true;
            }
            if mod_shift && !mod_ctrl {
                // Shift alone in this surface is reserved for no-op here; list
                // row actions provide range selection for precise multi-select.
            }
        }

        for (lane_id, mut request) in requests {
            if active_lane_ids.contains(&lane_id) {
                request.open_file |= global_request.open_file;
                request.open_location |= global_request.open_location;
                request.select_all |= global_request.select_all;
                request.select_none |= global_request.select_none;
                request.invert_selection |= global_request.invert_selection;
                request.copy_selected |= global_request.copy_selected;
                request.paste |= global_request.paste;
                request.delete_selected |= global_request.delete_selected;
            }
            self.apply_compare_lane_request(lane_id, request, &lane_ids, self.compare_sync);
        }
    }

    fn set_pending_inline_video_target(&mut self, path: &str) {
        let Some(lane) = self.compare_lanes.first() else {
            self.media_inline_video_pending_target = None;
            return;
        };
        let path_key = self.media_db.key_for(path);
        // Resolve once at the explicit play/select boundary. The render path
        // may inspect a 100k+ display order, but must never normalize and
        // allocate a key for every file on every frame.
        let source_index = lane
            .files
            .iter()
            .position(|candidate| candidate == path)
            .or_else(|| {
                lane.files
                    .iter()
                    .position(|candidate| self.media_db.key_for(candidate) == path_key)
            });
        let Some(source_index) = source_index else {
            self.media_inline_video_pending_target = None;
            return;
        };
        self.media_inline_video_pending_target = Some(PendingInlineVideoTarget {
            tab_id: self.media_tabs.active_id().as_str().to_string(),
            path: path.to_string(),
            path_key,
            source_index,
            requested_scan_id: lane.scan_id,
            checked_display_key: None,
        });
    }

    /// Media tab (WP-044): Library panel = folder strip + virtualized thumbnail
    /// grid; Viewer panel = selected media playback + metadata. FullGrid
    /// expands the Library panel into a full-window thumbnail wall. Chrome-hide
    /// (F11 here, Esc restores) strips the toolbar (plus app header/status in
    /// `render_ui`) for immersive browsing.
    fn draw_media_tab(&mut self, ui: &mut egui::Ui) {
        if self.compare_lanes.is_empty() {
            self.compare_lanes = vec![CompareLane::new(0)];
            self.compare_next_lane_id = 1;
        }
        if !self.media_explorer.chrome_hidden {
            self.draw_media_document_tabs(ui);
            theme::hairline(ui);
        }
        let lane_id = self.compare_lanes[0].id;
        let active_media_tab_id = self.media_tabs.active_id().as_str().to_string();
        self.drain_thumbnails(ui.ctx());
        let mut request = CompareLaneRenderRequest::default();
        // One display-order computation per frame (grid + keys + clamps all
        // share it; recomputing with different widths caused nav drift).
        let display = self.media_display_indices(ui.ctx(), lane_id);
        if let Some(pending) = self.media_inline_video_pending_target.clone() {
            let current_tab = self.media_tabs.active_id().as_str();
            let lane = &self.compare_lanes[0];
            let display_is_current = self.media_display_cache_key.as_ref().is_some_and(|key| {
                key.lane_id == lane.id
                    && key.scan_id == lane.scan_id
                    && key.content_generation == self.media_content_generation
            });
            let same_scan = lane.scan_id == pending.requested_scan_id;
            if current_tab != pending.tab_id {
                self.media_inline_video_pending_target = None;
            } else if !same_scan {
                // A numeric source index is only a hint inside the inventory
                // scan that produced it. A replacement scan cancels the
                // placement, while append-only progressive batches in the
                // same scan may publish newer display generations.
                self.media_inline_video_pending_target = None;
            } else if display_is_current
                && pending.checked_display_key.as_ref() != self.media_display_cache_key.as_ref()
            {
                // `source_index` was resolved once when the explicit play
                // request was accepted. Search the integer display order at
                // most once per published key; a filtered/missing target must
                // not trigger an O(n) scan every render frame.
                let source_is_exact = lane
                    .files
                    .get(pending.source_index)
                    .is_some_and(|path| self.media_db.key_for(path) == pending.path_key);
                let display_index = source_is_exact
                    .then(|| {
                        display
                            .iter()
                            .position(|candidate| *candidate == pending.source_index)
                    })
                    .flatten();
                if let Some(display_index) = display_index {
                    // Recheck the canonical key at the resolved row before
                    // clearing the pending request. A reordered inventory can
                    // never retarget playback to a numeric neighbor.
                    let exact_match = display
                        .get(display_index)
                        .and_then(|index| lane.files.get(*index))
                        .is_some_and(|path| self.media_db.key_for(path) == pending.path_key);
                    if exact_match {
                        self.media_explorer.cursor = Some(display_index);
                        self.media_scroll_to_cursor = true;
                        self.media_inline_video_pending_target = None;
                    }
                } else if let Some(target) = self.media_inline_video_pending_target.as_mut() {
                    target.checked_display_key = self.media_display_cache_key.clone();
                }
            }
        }
        // A filter/search/sort change can shrink the display list under a
        // stored cursor; clamp BEFORE anything indexes with it.
        if let Some(cursor) = self.media_explorer.cursor {
            if cursor >= display.len() {
                self.media_explorer.cursor = display.len().checked_sub(1);
                self.media_explorer.sel_anchor_display = None;
            }
        }

        if !self.media_explorer.chrome_hidden {
            self.draw_media_toolbar(ui, lane_id, &mut request);
            let mut notices: Vec<(String, egui::Color32)> = Vec::new();
            if let Some(status) = self.media_db.status().map(String::from) {
                notices.push((status, theme::warn_ink()));
            }
            if !self.compare_action_message.is_empty() {
                notices.push((self.compare_action_message.clone(), theme::ink_faint()));
            }
            if let Some(pos) = self.compare_lane_position(lane_id) {
                let err = self.compare_lanes[pos].scan_error.clone();
                if !err.is_empty() {
                    notices.push((err, theme::error_ink()));
                }
            }
            if !notices.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for (text, color) in notices {
                        ui.label(egui::RichText::new(text).small().color(color));
                    }
                });
            }
            theme::hairline(ui);
        }

        // ---- Library / Viewer surface ----
        let surface = ui.available_rect_before_wrap().intersect(ui.max_rect());
        let gutter_w = 7.0;
        let two_panel =
            self.media_explorer.view_mode == crate::media_explorer::MediaViewMode::TwoPanel;
        let split = self.media_explorer.split_ratio.clamp(
            crate::media_explorer::SPLIT_MIN,
            crate::media_explorer::SPLIT_MAX,
        );
        let (library_panel_rect, viewer_panel_rect) = if two_panel && surface.width() > 480.0 {
            let left_w = (surface.width() * split - gutter_w / 2.0).max(220.0);
            let left = egui::Rect::from_min_max(
                surface.min,
                egui::pos2(surface.min.x + left_w, surface.max.y),
            );
            let right = egui::Rect::from_min_max(
                egui::pos2(left.max.x + gutter_w, surface.min.y),
                surface.max,
            );
            // Draggable gutter between the Library and Viewer panels.
            let gutter = egui::Rect::from_min_max(
                egui::pos2(left.max.x, surface.min.y),
                egui::pos2(left.max.x + gutter_w, surface.max.y),
            );
            let gutter_resp = ui.interact(
                gutter,
                ui.id().with(("media_gutter", &active_media_tab_id)),
                Sense::click_and_drag(),
            );
            if gutter_resp.dragged() {
                let x = gutter_resp
                    .interact_pointer_pos()
                    .map(|p| p.x)
                    .unwrap_or(left.max.x);
                let ratio = ((x - surface.min.x) / surface.width()).clamp(
                    crate::media_explorer::SPLIT_MIN,
                    crate::media_explorer::SPLIT_MAX,
                );
                self.media_explorer.split_ratio = ratio;
                self.touch_media_settings();
            }
            if gutter_resp.double_clicked() {
                self.media_explorer.split_ratio = 0.62;
                self.touch_media_settings();
            }
            let stroke_x = gutter.center().x;
            let handle_half = 18.0;
            ui.painter().line_segment(
                [
                    egui::pos2(stroke_x, gutter.center().y - handle_half),
                    egui::pos2(stroke_x, gutter.center().y + handle_half),
                ],
                if gutter_resp.hovered() || gutter_resp.dragged() {
                    egui::Stroke::new(2.0, theme::ink())
                } else {
                    theme::rule_stroke()
                },
            );
            if gutter_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            (left, Some(right))
        } else {
            (surface, None)
        };

        self.draw_media_library_panel(
            ui,
            library_panel_rect,
            lane_id,
            display.as_slice(),
            &mut request,
        );
        if let Some(viewer_panel_rect) = viewer_panel_rect {
            self.draw_media_viewer_panel(ui, viewer_panel_rect, lane_id, &mut request);
        }
        self.draw_media_overlays(ui, surface, lane_id, &mut request);

        // Transient restore hint painted ON the book (the notices row is
        // hidden together with the chrome, so it can't carry the hint).
        if self.media_explorer.chrome_hidden {
            let show_hint = self
                .media_explorer
                .chrome_hidden_at
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3));
            if show_hint {
                let pos = egui::pos2(surface.center().x, surface.min.y + 16.0);
                ui.painter().text(
                    pos,
                    egui::Align2::CENTER_CENTER,
                    "Fullscreen — Esc or Ctrl+F restores",
                    egui::TextStyle::Body.resolve(ui.style()),
                    theme::ink_soft(),
                );
            }
        }

        // Unified input layer (WP-046): keyboard chords + controller bindings
        // + rebind capture all resolve to MediaActions in one place.
        self.media_handle_input(ui, lane_id, display.as_slice(), &mut request);

        // WP-066: a result opened into a new tab selects once that tab's scan
        // has published an inventory containing the exact file.
        if let Some(pending) = self.media_pending_result_selection.clone() {
            let available = self
                .compare_lane_position(lane_id)
                .is_some_and(|pos| self.compare_lanes[pos].files.iter().any(|f| *f == pending));
            if available {
                self.media_pending_result_selection = None;
                self.media_select_paths(lane_id, std::slice::from_ref(&pending));
            }
        }

        self.media_maybe_spawn_stat_sweep(ui.ctx(), lane_id);
        // Semantic search runtime (WP-047): index the folder, then rank.
        self.maybe_start_clip_index(lane_id);
        self.maybe_start_clip_query(lane_id);
        self.media_query_diagnostics.queue_depth =
            usize::from(self.media_search_index_inflight.is_some())
                + usize::from(self.media_suggestion_inflight.is_some())
                + usize::from(self.media_display_inflight.is_some())
                + usize::from(self.media_stat_request.is_some())
                + usize::from(self.clip_index_request.is_some())
                + usize::from(self.clip_query_request.is_some());
        self.media_apply_extras(lane_id, &mut request);
        self.draw_media_modals(ui, lane_id, &mut request);
        self.apply_compare_lane_request(lane_id, request, std::slice::from_ref(&lane_id), false);
    }

    fn draw_media_document_tabs(&mut self, ui: &mut egui::Ui) {
        let active = self.media_tabs.active_id().as_str().to_string();
        let tab_shortcuts_enabled = !self.media_explorer.show_folder_navigator
            && !self.media_explorer.show_settings
            && self.media_rename.is_none()
            && self.media_new_folder.is_none();
        let (previous_shortcut, next_shortcut, new_shortcut, close_shortcut) =
            if tab_shortcuts_enabled {
                ui.input_mut(|input| {
                    let ctrl_shift = egui::Modifiers {
                        ctrl: true,
                        shift: true,
                        ..Default::default()
                    };
                    let ctrl = egui::Modifiers {
                        ctrl: true,
                        ..Default::default()
                    };
                    (
                        input.consume_key(ctrl_shift, egui::Key::Tab),
                        input.consume_key(ctrl, egui::Key::Tab),
                        input.consume_key(ctrl, egui::Key::T),
                        input.consume_key(ctrl, egui::Key::W),
                    )
                })
            } else {
                (false, false, false, false)
            };
        let mut rows = self
            .media_tabs
            .tabs()
            .iter()
            .map(|tab| {
                // WP-067: a collection tab has no folder; it is titled by what
                // it collects rather than by a path leaf.
                if tab.viewport.kind == crate::media_tabs::MediaTabKind::Collection {
                    return (
                        tab.id.as_str().to_string(),
                        "★ Favorites".to_string(),
                        String::new(),
                    );
                }
                let path = if tab.viewport.folder_key.is_empty() {
                    String::new()
                } else {
                    self.media_db.path_for_key(&tab.viewport.folder_key)
                };
                (
                    tab.id.as_str().to_string(),
                    crate::media_tabs::folder_tab_title(&path),
                    path,
                )
            })
            .collect::<Vec<_>>();
        let mut seen = HashMap::<String, usize>::new();
        for (_, title, _) in &rows {
            *seen.entry(title.clone()).or_default() += 1;
        }
        let mut ordinals = HashMap::<String, usize>::new();
        let mut activate = None;
        let mut close = None;
        let mut add = new_shortcut;
        if close_shortcut {
            close = Some(active.clone());
        } else if previous_shortcut || next_shortcut {
            if let Some(index) = rows.iter().position(|(id, _, _)| id == &active) {
                let target = if previous_shortcut {
                    index.checked_sub(1).unwrap_or(rows.len().saturating_sub(1))
                } else {
                    (index + 1) % rows.len().max(1)
                };
                activate = rows.get(target).map(|(id, _, _)| id.clone());
            }
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Media").strong().color(theme::ink()));
            egui::ScrollArea::horizontal()
                .id_source("media_document_tabs")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (id, title, path) in rows.drain(..) {
                            let ordinal = ordinals.entry(title.clone()).or_default();
                            *ordinal += 1;
                            let label = if seen.get(&title).copied().unwrap_or(0) > 1 {
                                format!("{title} · {ordinal}")
                            } else {
                                title
                            };
                            let selected = id == active;
                            ui.push_id(id.clone(), |ui| {
                                let response = ui.selectable_label(selected, label).on_hover_text(
                                    if path.is_empty() {
                                        "No folder selected"
                                    } else {
                                        &path
                                    },
                                );
                                if response.clicked() && !selected {
                                    activate = Some(id.clone());
                                }
                                if ui.small_button("×").on_hover_text("Close tab").clicked() {
                                    close = Some(id.clone());
                                }
                            });
                        }
                        if ui
                            .small_button("+")
                            .on_hover_text("Browse a folder to open in a new tab")
                            .clicked()
                        {
                            add = true;
                        }
                    });
                });
        });
        if add {
            if let Some(lane_id) = self.compare_lanes.first().map(|lane| lane.id) {
                self.request_media_folder_navigator(ui.ctx(), lane_id);
            }
        } else if let Some(id) = close {
            if let Err(error) = self.close_media_tab(&id) {
                self.compare_action_message = error;
            }
        } else if let Some(id) = activate {
            if let Err(error) = self.activate_media_tab(&id) {
                self.compare_action_message = error;
            }
        }
    }

    /// Apply the media-only request verbs (WP-045): cut, cut-paste (move),
    /// rename/new-folder modal arming, refresh, sort, labels, favorites.
    fn media_apply_extras(&mut self, lane_id: usize, request: &mut CompareLaneRenderRequest) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        // Plain Copy always downgrades a pending cut back to copy semantics.
        if request.copy_selected {
            self.compare_clipboard_cut = false;
        }
        if request.copy_absolute_path {
            self.media_copy_path_text(lane_id, true);
        }
        if request.copy_portable_path {
            self.media_copy_path_text(lane_id, false);
        }
        if request.cut_selected {
            let lane = &self.compare_lanes[pos];
            let mut paths: Vec<String> = lane
                .selected_files
                .iter()
                .filter_map(|&i| lane.files.get(i).cloned())
                .collect();
            paths.sort();
            if !paths.is_empty() {
                let count = paths.len();
                self.compare_clipboard = paths;
                self.compare_clipboard_cut = true;
                self.compare_action_message = format!("Cut {count} file(s) — paste moves them");
            }
        }
        // Cut-paste is a MOVE handled here; never forwarded to the copy path.
        if request.paste && self.compare_clipboard_cut && !self.compare_clipboard.is_empty() {
            request.paste = false;
            let dest = sanitize_folder_input(&self.compare_lanes[pos].folder);
            let dest_path = std::path::PathBuf::from(&dest);
            if dest_path.is_dir() {
                let sources = self.compare_clipboard.clone();
                let source_set: HashSet<&str> = sources.iter().map(String::as_str).collect();
                let (moved, failures) = crate::media_fs::move_files(&sources, &dest_path);
                // Same-folder cut-paste no-ops return their source path;
                // count only files that actually changed location (r3, f.7).
                let really_moved = moved
                    .iter()
                    .filter(|t| !source_set.contains(t.to_string_lossy().as_ref()))
                    .count();
                self.compare_action_message = if failures.is_empty() {
                    if really_moved == 0 {
                        "Files are already in this folder — nothing moved".to_string()
                    } else {
                        format!("Moved {really_moved} file(s)")
                    }
                } else {
                    format!(
                        "Moved {really_moved} file(s); {} failed: {}",
                        failures.len(),
                        failures
                            .iter()
                            .map(|(path, err)| {
                                let name = Path::new(path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(path);
                                format!("{name} ({err})")
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                };
                self.compare_clipboard.clear();
                self.compare_clipboard_cut = false;
                request.scan = true;
            } else {
                self.compare_action_message = "Paste target is not a folder".to_string();
            }
        }
        if request.rename_selected {
            let lane = &self.compare_lanes[pos];
            let target = lane
                .selected_files
                .iter()
                .min()
                .copied()
                .or(if lane.total() > 0 {
                    Some(lane.index)
                } else {
                    None
                });
            if let Some(file_idx) = target {
                if let Some(path) = lane.files.get(file_idx) {
                    let name = Path::new(path)
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    self.media_rename = Some((path.clone(), name));
                }
            }
        }
        if request.new_folder {
            self.media_new_folder = Some(String::new());
        }
        if request.refresh {
            request.scan = true;
        }
        if let Some((sort, desc)) = request.sort_to {
            self.media_explorer.sort = sort;
            self.media_explorer.sort_desc = desc;
            self.touch_media_settings();
        }
        if request.add_label.is_some() || request.remove_label.is_some() || request.clear_labels {
            let lane = &self.compare_lanes[pos];
            let keys: Vec<String> = lane
                .selected_files
                .iter()
                .filter_map(|&i| lane.files.get(i))
                .map(|p| self.media_db.key_for(p))
                .collect();
            for key in keys {
                let labels = Arc::make_mut(&mut self.media_color_labels);
                if request.clear_labels {
                    labels.remove(&key);
                } else {
                    let assigned = labels.entry(key.clone()).or_default();
                    if let Some(label) = request.add_label.as_ref() {
                        if !assigned.contains(label) {
                            assigned.push(label.clone());
                        }
                    }
                    if let Some(label) = request.remove_label.as_ref() {
                        assigned.retain(|assigned_id| assigned_id != label);
                    }
                    if assigned.is_empty() {
                        labels.remove(&key);
                    }
                }
                self.touch_media_meta(&key);
            }
        }
        if request.toggle_favorite {
            let target = self.media_selected_path(lane_id).or_else(|| {
                let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
                (!folder.is_empty()).then_some(folder)
            });
            if let Some(path) = target {
                self.media_toggle_favorite(&path);
            }
        }
    }

    /// Centered inline editors for rename / new folder (WP-045): themed,
    /// in-window, validate-on-save with the error shown in place.
    fn draw_media_modals(
        &mut self,
        ui: &mut egui::Ui,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
        let screen = ui.ctx().screen_rect();
        let anchor = egui::pos2(screen.center().x - 180.0, screen.center().y - 60.0);

        if let Some((old_path, mut buffer)) = self.media_rename.take() {
            let mut keep_open = true;
            egui::Area::new(egui::Id::new("media_rename_modal"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor)
                .show(ui.ctx(), |ui| {
                    theme::sheet_frame().show(ui, |ui| {
                        ui.set_min_width(360.0);
                        let old_name = Path::new(&old_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file");
                        ui.label(
                            egui::RichText::new(format!("Rename {old_name}"))
                                .strong()
                                .color(theme::ink()),
                        );
                        let edit = ui.add(
                            TextEdit::singleline(&mut buffer)
                                .desired_width(f32::INFINITY)
                                .hint_text("new name (extension kept unless typed)"),
                        );
                        edit.request_focus();
                        let mut error: Option<String> = None;
                        let submit = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        ui.horizontal(|ui| {
                            let save = theme::primary_button(ui, "Rename").clicked() || submit;
                            let quit = ui.button("Cancel").clicked() || cancel;
                            if save {
                                match crate::media_fs::rename_file(
                                    Path::new(&old_path),
                                    &buffer,
                                    true,
                                ) {
                                    Ok(new_path) => {
                                        self.compare_action_message = format!(
                                            "Renamed to {}",
                                            new_path
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("file")
                                        );
                                        request.scan = true;
                                        keep_open = false;
                                    }
                                    Err(err) => error = Some(err),
                                }
                            } else if quit {
                                keep_open = false;
                            }
                        });
                        if let Some(err) = error {
                            self.compare_action_message = format!("Rename failed: {err}");
                            ui.label(
                                egui::RichText::new(&self.compare_action_message)
                                    .small()
                                    .color(theme::error_ink()),
                            );
                        }
                    });
                });
            if keep_open {
                self.media_rename = Some((old_path.clone(), buffer));
            }
        }

        if let Some(mut buffer) = self.media_new_folder.take() {
            let mut keep_open = true;
            egui::Area::new(egui::Id::new("media_new_folder_modal"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor)
                .show(ui.ctx(), |ui| {
                    theme::sheet_frame().show(ui, |ui| {
                        ui.set_min_width(360.0);
                        ui.label(
                            egui::RichText::new("New folder")
                                .strong()
                                .color(theme::ink()),
                        );
                        let edit = ui.add(
                            TextEdit::singleline(&mut buffer)
                                .desired_width(f32::INFINITY)
                                .hint_text("folder name"),
                        );
                        edit.request_focus();
                        let submit = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        ui.horizontal(|ui| {
                            let save = theme::primary_button(ui, "Create").clicked() || submit;
                            let quit = ui.button("Cancel").clicked() || cancel;
                            if save {
                                match crate::media_fs::create_folder(Path::new(&folder), &buffer) {
                                    Ok(created) => {
                                        self.compare_action_message = format!(
                                            "Created {}",
                                            created
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("folder")
                                        );
                                        request.scan = true;
                                        keep_open = false;
                                    }
                                    Err(err) => {
                                        self.compare_action_message =
                                            format!("New folder failed: {err}");
                                    }
                                }
                            } else if quit {
                                keep_open = false;
                            }
                        });
                        if self.compare_action_message.starts_with("New folder failed") {
                            ui.label(
                                egui::RichText::new(&self.compare_action_message)
                                    .small()
                                    .color(theme::error_ink()),
                            );
                        }
                    });
                });
            if keep_open {
                self.media_new_folder = Some(buffer);
            }
        }
    }

    /// Single-row minimalist toolbar: filter · subfolders · sort · view ·
    /// tile size · search · favorites/settings.
    fn draw_media_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        // WP-067: a collection tab swaps the folder toolbar for a sub-view
        // selector. Everything below it (grid, viewer, playback) is unchanged.
        if self.media_tabs.active().viewport.kind == crate::media_tabs::MediaTabKind::Collection {
            self.draw_media_collection_toolbar(ui, lane_id);
            return;
        }
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let mut search_id: Option<egui::Id> = None;
        let mut search_rect = egui::Rect::NOTHING;
        let mut search_focused = false;
        ui.horizontal(|ui| {
            // A single media-kind dropdown keeps Images/Videos/All discoverable
            // without consuming the toolbar with a three-way segmented control.
            let current_filter = self.compare_lanes[pos].media_filter;
            egui::ComboBox::from_id_source("media_kind_filter_combo")
                .selected_text(format!("{} media", current_filter.label()))
                .width(112.0)
                .show_ui(ui, |ui| {
                    for filter in [
                        MediaFilterMode::All,
                        MediaFilterMode::ImagesOnly,
                        MediaFilterMode::VideosOnly,
                    ] {
                        if ui
                            .selectable_label(current_filter == filter, filter.label())
                            .clicked()
                            && self.compare_lanes[pos].media_filter != filter
                        {
                            self.compare_lanes[pos].media_filter = filter;
                            if !self.compare_lanes[pos].folder.trim().is_empty() {
                                request.scan = true;
                            }
                        }
                    }
                });
            ui.separator();
            // Show media down the tree (recursive scan).
            let mut recursive = self.compare_lanes[pos].recursive;
            if ui
                .selectable_label(recursive, "Tree")
                .on_hover_text("Show media from subfolders too (recursive)")
                .clicked()
            {
                recursive = !recursive;
                self.compare_lanes[pos].recursive = recursive;
                if !self.compare_lanes[pos].folder.trim().is_empty() {
                    request.scan = true;
                }
            }
            // WP-066: scope search to this folder without giving up the
            // recursive inventory. Deliberately does NOT request a scan — that
            // is the whole point of separating scope from the Tree flag.
            if recursive {
                let mut folder_only = self.media_search_folder_only;
                if ui
                    .selectable_label(folder_only, "This folder")
                    .on_hover_text(
                        "Search only files directly in this folder (keeps the subfolder scan)",
                    )
                    .clicked()
                {
                    folder_only = !folder_only;
                    self.media_search_folder_only = folder_only;
                    self.touch_media_settings();
                }
            } else if self.media_search_folder_only {
                // Without a recursive inventory the scope is already the folder.
                self.media_search_folder_only = false;
            }
            ui.separator();
            // Sort menu.
            let sort_label = format!(
                "{} {}",
                self.media_explorer.sort.label(),
                if self.media_explorer.sort_desc {
                    "↓"
                } else {
                    "↑"
                }
            );
            egui::ComboBox::from_id_source("media_sort_combo")
                .selected_text(sort_label)
                .width(110.0)
                .show_ui(ui, |ui| {
                    for sort in [
                        crate::media_explorer::MediaSort::Name,
                        crate::media_explorer::MediaSort::Modified,
                        crate::media_explorer::MediaSort::Size,
                        crate::media_explorer::MediaSort::Created,
                    ] {
                        if ui
                            .selectable_label(self.media_explorer.sort == sort, sort.label())
                            .clicked()
                        {
                            if self.media_explorer.sort == sort {
                                self.media_explorer.sort_desc = !self.media_explorer.sort_desc;
                            } else {
                                // New key starts ascending (matches the
                                // context-menu semantics; round 3, f.12).
                                self.media_explorer.sort = sort;
                                self.media_explorer.sort_desc = false;
                            }
                            self.touch_media_settings();
                        }
                    }
                });
            // View mode toggle.
            let full =
                self.media_explorer.view_mode == crate::media_explorer::MediaViewMode::FullGrid;
            if ui
                .selectable_label(full, icons::SQUARES_FOUR)
                .on_hover_text("Full-window thumbnail wall (Tab)")
                .clicked()
            {
                self.media_explorer.view_mode = if full {
                    crate::media_explorer::MediaViewMode::TwoPanel
                } else {
                    crate::media_explorer::MediaViewMode::FullGrid
                };
                self.touch_media_settings();
            }
            if ui
                .selectable_label(self.media_explorer.show_names, "Names")
                .on_hover_text("Show filenames below thumbnails")
                .clicked()
            {
                self.media_explorer.show_names = !self.media_explorer.show_names;
                self.touch_media_settings();
            }
            // Tile size.
            let mut edge = self.media_explorer.tile_edge;
            if ui
                .add_sized(
                    [74.0, ui.spacing().interact_size.y],
                    egui::Slider::new(
                        &mut edge,
                        crate::media_explorer::TILE_MIN..=crate::media_explorer::TILE_MAX,
                    )
                    .show_value(false),
                )
                .on_hover_text("Thumbnail size (Ctrl+wheel over the grid)")
                .changed()
            {
                self.media_explorer.tile_edge = edge;
                self.touch_media_settings();
            }
            ui.separator();
        });
        ui.horizontal(|ui| {
            // Search box + mode (WP-047/WP-061: chips like tag:x label:selects kind:img
            // note:text combine with free text; mode picks the ranker).
            // WP-066: the old hint was both misleading and clipped — it claimed
            // folder scope the implementation did not provide, and its 403-point
            // text was cut off inside a 170-point box (inspector reported
            // clipped=true). State the ACTUAL scope in a hint that fits, and put
            // the full grammar in hover text where it has room.
            let scope_hint = if self.media_search_folder_only {
                "search this folder…"
            } else {
                "search this tab…"
            };
            let search_resp = ui
                .add(
                    TextEdit::singleline(&mut self.media_search_query)
                        .desired_width(240.0)
                        .hint_text(scope_hint),
                )
                .on_hover_text(
                    "Filter chips: tag: label: kind: note: fav:\n\
                     Subtract with ! or - (e.g. -label:red, !fav:, -blooper)\n\
                     Quote a term to keep it literal: \"-take01\"\n\
                     Toggle 'This folder' to search only this folder",
                );
            if self.media_focus_search {
                search_resp.request_focus();
                self.media_focus_search = false;
            }
            search_id = Some(search_resp.id);
            search_rect = search_resp.rect;
            search_focused = search_resp.has_focus();
            let modes = ["Name", "Fuzzy", "Semantic"];
            self.media_search_mode = self.media_search_mode.min(2);
            egui::ComboBox::from_id_source("media_search_mode_combo")
                .selected_text(modes[self.media_search_mode])
                .width(96.0)
                .show_ui(ui, |ui| {
                    for (index, mode) in modes.iter().enumerate() {
                        if ui
                            .selectable_label(self.media_search_mode == index, *mode)
                            .clicked()
                        {
                            self.media_search_mode = index;
                        }
                    }
                });
            if !self.media_search_query.trim().is_empty() && ui.small_button("×").clicked() {
                self.media_search_query.clear();
            }
            // Right-aligned: status + panels.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(false, format!("{} Refresh", icons::ARROW_CLOCKWISE))
                    .on_hover_text("Rescan only the selected folder using this media type and Tree setting (F5)")
                    .clicked()
                {
                    request.refresh = true;
                }
                if ui
                    .selectable_label(self.media_explorer.show_favorites, icons::STAR)
                    .on_hover_text("Favorite folders (Ctrl+B)")
                    .clicked()
                {
                    self.media_explorer.show_favorites = !self.media_explorer.show_favorites;
                    if self.media_explorer.show_favorites {
                        self.media_explorer.show_settings = false;
                        self.close_media_folder_navigator();
                    }
                }
                if ui
                    .selectable_label(
                        self.media_explorer.show_folder_navigator,
                        format!("{} Folders  Ctrl+G", icons::FOLDERS),
                    )
                    .on_hover_text(
                        "Large couch-distance folder navigator (Ctrl+G / controller Select)",
                    )
                    .clicked()
                {
                    self.media_toggle_folder_navigator(ui.ctx(), lane_id);
                }
                if ui
                    .selectable_label(self.controller_pointer_mode, "Cursor")
                    .on_hover_text(
                        "Controller cursor mode (Ctrl+M / R3): right stick moves, A/B click",
                    )
                    .clicked()
                {
                    self.set_controller_pointer_mode(!self.controller_pointer_mode);
                }
                if self.controller_active.is_some() || self.controller_legacy_active {
                    ui.label(egui::RichText::new(icons::GAME_CONTROLLER).color(theme::ink_soft()))
                        .on_hover_text("Controller connected");
                }
                let lane = &self.compare_lanes[pos];
                let status = if lane.scanning {
                    "scanning…".to_string()
                } else if lane.total() > 0 {
                    format!("{}", group_thousands(lane.total()))
                } else {
                    String::new()
                };
                if !status.is_empty() {
                    ui.label(
                        egui::RichText::new(status)
                            .small()
                            .color(theme::ink_faint()),
                    );
                }
            });
        });

        // ---- active filter chips (removable) ----
        let parsed = crate::media_search::parse_query(&self.media_search_query);
        if parsed.has_chips() {
            let mut remove_token: Option<String> = None;
            ui.horizontal_wrapped(|ui| {
                let mut chip = |ui: &mut egui::Ui, token: String| {
                    if ui
                        .small_button(egui::RichText::new(format!("{token} ×")).small())
                        .on_hover_text("Remove this filter")
                        .clicked()
                    {
                        remove_token = Some(token);
                    }
                };
                // Tokens are rebuilt with quoting so removal matches the raw
                // token even for multi-word values (tag:"red dress").
                for tag in &parsed.tags {
                    chip(
                        ui,
                        format!("tag:{}", crate::media_search::quote_chip_value(tag)),
                    );
                }
                for label in &parsed.labels {
                    chip(
                        ui,
                        format!("label:{}", crate::media_search::quote_chip_value(label)),
                    );
                }
                for note in &parsed.notes_contain {
                    chip(
                        ui,
                        format!("note:{}", crate::media_search::quote_chip_value(note)),
                    );
                }
                for kind in &parsed.kinds {
                    chip(
                        ui,
                        match kind {
                            crate::media_search::MediaKindFilter::Image => "kind:img".to_string(),
                            crate::media_search::MediaKindFilter::Video => "kind:vid".to_string(),
                        },
                    );
                }
            });
            if let Some(token) = remove_token {
                self.media_search_query =
                    crate::media_search::remove_query_token(&self.media_search_query, &token);
            }
        }

        // ---- provisional-order notice (WP-069) ----
        // While a query is active the grid may be showing an immediately
        // renderable traversal order rather than the ranked result. Say so, or
        // an operator reads unranked rows as search results.
        if !self.media_search_query.trim().is_empty()
            && !self.media_display_cache.is_empty()
            && self.media_display_cache_key.is_none()
        {
            ui.label(
                egui::RichText::new("showing all rows while the search ranks…")
                    .small()
                    .color(theme::warn_ink()),
            );
        }

        // ---- semantic status line ----
        if (self.media_search_mode == 2 || self.clip_loading || self.clip_indexing)
            && !self.clip_status.is_empty()
        {
            ui.label(
                egui::RichText::new(&self.clip_status)
                    .small()
                    .color(theme::ink_faint()),
            );
        }

        // ---- autocomplete popup (WP-047) ----
        if search_focused && !self.media_search_query.trim().is_empty() {
            let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
            let query = self.media_search_query.clone();
            let suggestions =
                self.media_autocomplete_suggestions(ui.ctx(), lane_id, &folder, &query);
            if !suggestions.is_empty() {
                let mut insert: Option<String> = None;
                // WP-066: a file row is a real result, not just text. Plain
                // click opens it in this tab; Ctrl+click opens it in a new tab.
                let mut open_file: Option<(crate::media_search::FileSuggestion, bool)> = None;
                let ctrl_held = ui.ctx().input(|input| input.modifiers.command);
                egui::Area::new(egui::Id::new("media_search_suggest"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(search_rect.min.x, search_rect.max.y + 2.0))
                    .show(ui.ctx(), |ui| {
                        theme::sheet_frame().show(ui, |ui| {
                            ui.set_min_width(search_rect.width().max(220.0));
                            for suggestion in suggestions.iter() {
                                let (kind, value) = suggestion.display();
                                let is_file = suggestion.file().is_some();
                                let label = if is_file {
                                    format!("{kind}: {value}   ↵ open · Ctrl new tab")
                                } else {
                                    format!("{kind}: {value}")
                                };
                                let row = ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new(label).small(),
                                    );
                                let row = if is_file {
                                    row.on_hover_text(
                                        "Open in this tab — hold Ctrl to open in a new tab",
                                    )
                                } else {
                                    row
                                };
                                if row.clicked() {
                                    match suggestion.file() {
                                        Some(file) => {
                                            open_file = Some((file.clone(), ctrl_held));
                                        }
                                        None => insert = Some(suggestion.insert_text()),
                                    }
                                }
                            }
                        });
                    });
                if let Some((file, new_tab)) = open_file {
                    self.media_activate_search_result(lane_id, &file, new_tab, request);
                }
                if let Some(text) = insert {
                    // Replace the token being typed with the completion.
                    let mut tokens: Vec<&str> =
                        self.media_search_query.split_whitespace().collect();
                    if tokens.is_empty() {
                        tokens.push("");
                    }
                    let last = tokens.len() - 1;
                    let owned: String = text;
                    let mut rebuilt: Vec<String> =
                        tokens[..last].iter().map(|s| s.to_string()).collect();
                    rebuilt.push(owned);
                    self.media_search_query = rebuilt.join(" ");
                    if let Some(id) = search_id {
                        ui.ctx().memory_mut(|m| m.request_focus(id));
                    }
                }
            }
        }
    }

    fn media_search_index_key_for(&self, lane_id: usize) -> Option<MediaSearchIndexKey> {
        let pos = self.compare_lane_position(lane_id)?;
        let lane = &self.compare_lanes[pos];
        Some(MediaSearchIndexKey {
            lane_id,
            scan_id: lane.scan_id,
            content_generation: self.media_content_generation,
            inventory_generation: lane.inventory_generation,
            meta_generation: self.media_meta_generation,
        })
    }

    /// Build the immutable normalized query index off-thread. Arc snapshots
    /// make the render-thread handoff O(1), including large metadata maps.
    fn media_ensure_search_index(
        &mut self,
        ctx: &egui::Context,
        lane_id: usize,
        key: MediaSearchIndexKey,
    ) {
        if self.media_search_index_key.as_ref() == Some(&key)
            || self.media_search_index_inflight.as_ref() == Some(&key)
        {
            return;
        }
        if let Some(cancel) = self.media_search_index_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let files = Arc::clone(&self.compare_lanes[pos].files);
        let notes = Arc::clone(&self.media_notes);
        let tags = Arc::clone(&self.media_tags);
        let labels = Arc::clone(&self.media_color_labels);
        // WP-066: carry favorite membership into the index so `fav:` filters
        // evaluate on the same immutable snapshot as every other chip.
        let favorite_keys = self.media_favorite_keys.clone();
        let label_names: BTreeMap<String, String> = self
            .media_label_definitions
            .iter()
            .map(|definition| (definition.id.clone(), definition.name.clone()))
            .collect();
        let workspace = self.config.workspace_root.clone();
        let generation = crate::media_search::SearchIndexGeneration(key.content_generation);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.media_search_index_cancel = Some(Arc::clone(&cancelled));
        self.media_search_index_inflight = Some(key.clone());
        self.media_query_diagnostics.status = "building_index".to_string();
        let tx = self.compare_work_tx.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut rows = Vec::with_capacity(files.len());
            for (source_index, path) in files.iter().enumerate() {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let db_key = crate::media_db::canonical_key(&workspace, path);
                let name = Path::new(path)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(path)
                    .to_string();
                let mut row_labels = Vec::new();
                if let Some(assigned) = labels.get(&db_key) {
                    for id in assigned {
                        row_labels.push(id.clone());
                        if let Some(name) = label_names.get(id) {
                            row_labels.push(name.clone());
                        }
                    }
                }
                rows.push(crate::media_search::IndexedMediaRow::new(
                    source_index,
                    name,
                    path.clone(),
                    crate::media_search::IndexedRowMeta::from_owned_labels(
                        tags.get(&db_key).cloned(),
                        notes.get(&db_key).cloned(),
                        row_labels,
                        crate::media_explorer::is_video_path(path),
                    )
                    .with_favorite(favorite_keys.contains(&db_key)),
                ));
            }
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let index = Arc::new(crate::media_search::MediaSearchIndex::new(generation, rows));
            let _ = tx.send(CompareWorkEvent::MediaSearchIndexReady {
                key,
                index,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
            repaint.request_repaint();
        });
    }

    /// Return only an immutable cached suggestion list to the render path.
    /// Candidate preparation and ranking run on one latest-only worker against
    /// the normalized search index; neither redb nor the collection is walked
    /// from an immediate-mode frame.
    fn media_autocomplete_suggestions(
        &mut self,
        ctx: &egui::Context,
        lane_id: usize,
        folder: &str,
        query: &str,
    ) -> Arc<Vec<crate::media_search::Suggestion>> {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return Arc::new(Vec::new());
        };
        let lane = &self.compare_lanes[pos];
        if !media_background_index_work_allowed(lane.scanning, lane.scan_using_cached_inventory) {
            // Progressive batches must retain unique ownership of lane.files;
            // lending an Arc snapshot to an index worker would make the next
            // UI-thread Arc::make_mut clone the accumulated collection.
            if let Some(cancel) = self.media_suggestion_cancel.take() {
                cancel.store(true, Ordering::Release);
            }
            return Arc::new(Vec::new());
        }
        let Some(index_key) = self.media_search_index_key_for(lane_id) else {
            return Arc::new(Vec::new());
        };
        self.media_ensure_search_index(ctx, lane_id, index_key.clone());
        if self.media_search_index_key.as_ref() != Some(&index_key) {
            return Arc::new(Vec::new());
        }
        let key = MediaSuggestionRequestKey {
            index_key,
            folder: folder.to_string(),
            query: query.to_string(),
        };
        if self.media_suggestion_key.as_ref() == Some(&key) {
            return Arc::clone(&self.media_suggestions);
        }
        if self.media_suggestion_inflight.as_ref() == Some(&key) {
            return Arc::new(Vec::new());
        }
        if self.media_suggestion_inflight.is_some() {
            if let Some(cancel) = self.media_suggestion_cancel.take() {
                cancel.store(true, Ordering::Release);
                self.media_query_diagnostics.cancellations =
                    self.media_query_diagnostics.cancellations.saturating_add(1);
            }
            // The cancelled worker reports completion and releases the single
            // in-flight slot; the next frame starts the newest request.
            return Arc::new(Vec::new());
        }
        let Some(index) = self.media_search_index.clone() else {
            return Arc::new(Vec::new());
        };
        let folder_names = if folder.is_empty() {
            Arc::new(Vec::new())
        } else {
            self.media_child_folders(lane_id, folder)
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.media_suggestion_cancel = Some(Arc::clone(&cancelled));
        self.media_suggestion_inflight = Some(key.clone());
        let tx = self.compare_work_tx.clone();
        let repaint = ctx.clone();
        let label_vocab: Vec<String> = self
            .media_label_definitions
            .iter()
            .flat_map(|definition| [definition.name.clone(), definition.id.clone()])
            .collect();
        thread::spawn(move || {
            let label_vocab_refs: Vec<&str> = label_vocab.iter().map(String::as_str).collect();
            let result = crate::media_search::suggestions_indexed_cancellable(
                &index,
                &key.query,
                &label_vocab_refs,
                &folder_names,
                6,
                || cancelled.load(Ordering::Acquire),
            );
            let was_cancelled = !result.is_complete();
            let _ = tx.send(CompareWorkEvent::MediaSuggestionsDone {
                key,
                suggestions: Arc::new(result.suggestions),
                cancelled: was_cancelled,
            });
            repaint.request_repaint();
        });
        Arc::new(Vec::new())
    }

    /// Compose the display order asynchronously (WP-056). The immediate-mode
    /// render path only compares fixed-size keys and clones Arcs; indexing,
    /// query ranking, and final sorting all run in workers.
    fn media_display_indices(&mut self, ctx: &egui::Context, lane_id: usize) -> Arc<Vec<usize>> {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return Arc::new(Vec::new());
        };
        let lane = &self.compare_lanes[pos];
        if !media_background_index_work_allowed(lane.scanning, lane.scan_using_cached_inventory) {
            // Progressive batches append only bounded index ranges below. Do
            // not lend their shared Vec to search/sort/stat workers: that
            // would make the next Arc::make_mut clone every accumulated row
            // on the UI thread.
            return Arc::clone(&self.media_display_cache);
        }
        let key = MediaDisplayCacheKey {
            lane_id,
            scan_id: lane.scan_id,
            content_generation: self.media_content_generation,
            stats_generation: self.media_stats_generation,
            semantic_generation: self.media_semantic_generation,
            meta_generation: self.media_meta_generation,
            sort: self.media_explorer.sort,
            sort_desc: self.media_explorer.sort_desc,
            query: self.media_search_query.clone(),
            search_mode: self.media_search_mode,
            search_folder_only: self.media_search_folder_only,
        };
        if self.media_display_cache_key.as_ref() == Some(&key) {
            return Arc::clone(&self.media_display_cache);
        }

        if self.media_display_desired_key.as_ref() != Some(&key) {
            let structural_change = match self.media_display_cache_key.as_ref() {
                None => self.media_display_cache.is_empty(),
                Some(old) => {
                    old.lane_id != key.lane_id
                        || old.scan_id != key.scan_id
                        || (!lane.scanning && old.content_generation != key.content_generation)
                }
            };
            if structural_change {
                self.media_display_cache = Arc::new(Vec::new());
                self.media_display_cache_key = None;
            }
            if self.media_display_inflight.is_some() {
                self.media_query_diagnostics.cancellations =
                    self.media_query_diagnostics.cancellations.saturating_add(1);
            }
            self.media_search_requests.cancel_current();
            self.media_display_desired_key = Some(key.clone());
            self.media_display_pending_since = Some(std::time::Instant::now());
            self.media_query_diagnostics.status = "debouncing".to_string();
            let repaint = ctx.clone();
            thread::spawn(move || {
                thread::sleep(std::time::Duration::from_millis(75));
                repaint.request_repaint();
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(75));
            return Arc::clone(&self.media_display_cache);
        }

        if self
            .media_display_pending_since
            .is_some_and(|started| started.elapsed() < std::time::Duration::from_millis(75))
        {
            return Arc::clone(&self.media_display_cache);
        }

        let Some(index_key) = self.media_search_index_key_for(lane_id) else {
            return Arc::clone(&self.media_display_cache);
        };
        self.media_ensure_search_index(ctx, lane_id, index_key.clone());
        if self.media_search_index_key.as_ref() != Some(&index_key) {
            return Arc::clone(&self.media_display_cache);
        }
        let Some(index) = self.media_search_index.clone() else {
            return Arc::clone(&self.media_display_cache);
        };
        if self.media_display_inflight.is_some() {
            return Arc::clone(&self.media_display_cache);
        }

        let mut parsed = crate::media_search::parse_query(&key.query);
        // WP-066: folder-only scope filters the existing inventory. It is not
        // the scan's `recursive` flag, so the recursive rows stay loaded and
        // toggling scope never rescans.
        if key.search_folder_only {
            let folder = self
                .compare_lane_position(lane_id)
                .map(|pos| self.compare_lanes[pos].folder.clone())
                .unwrap_or_default();
            if !folder.trim().is_empty() {
                parsed.folder_only = Some(folder);
            }
        }
        let mode = match key.search_mode {
            0 => crate::media_search::RankMode::Name,
            1 => crate::media_search::RankMode::Fuzzy,
            _ => crate::media_search::RankMode::Metadata,
        };
        let (request, cancellation) =
            self.media_search_requests
                .begin(index.generation(), parsed.clone(), mode, 0);
        let request_key = request.key;
        self.media_display_inflight = Some(request_key);
        self.media_display_pending_since = None;
        self.media_query_diagnostics.status = "querying".to_string();
        let files = Arc::clone(&self.compare_lanes[pos].files);
        let stats = Arc::clone(&self.media_explorer.stats);
        let semantic = if key.search_mode == 2 {
            self.media_semantic
                .as_ref()
                .and_then(|(folder, query, indices)| {
                    let current = sanitize_folder_input(&self.compare_lanes[pos].folder);
                    (folder == &current && query == &parsed.text).then(|| Arc::clone(indices))
                })
        } else {
            None
        };
        let tx = self.compare_work_tx.clone();
        let repaint = ctx.clone();
        let result_key = key.clone();
        thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut cancelled;
            let scanned_rows;
            let matched_rows;
            let indices = if parsed.is_empty() {
                let sorted = crate::media_explorer::sorted_indices_cancellable(
                    files.as_ref(),
                    result_key.sort,
                    result_key.sort_desc,
                    stats.as_ref(),
                    || cancellation.is_cancelled(),
                );
                cancelled = sorted.is_none() || cancellation.is_cancelled();
                scanned_rows = files.len();
                matched_rows = sorted.as_ref().map_or(0, Vec::len);
                sorted.unwrap_or_default()
            } else if let Some(semantic) = semantic {
                let mut chips_only = request.clone();
                chips_only.query.text.clear();
                let filtered =
                    crate::media_search::rank_indexed(&index, &chips_only, &cancellation);
                cancelled = !filtered.is_complete();
                scanned_rows = filtered.diagnostics.scanned_rows;
                let allowed: HashSet<usize> =
                    filtered.hits.into_iter().map(|hit| hit.index).collect();
                let ordered: Vec<usize> = semantic
                    .iter()
                    .copied()
                    .take_while(|_| !cancellation.is_cancelled())
                    .filter(|source_index| allowed.contains(source_index))
                    .collect();
                matched_rows = ordered.len();
                ordered
            } else {
                let result = crate::media_search::rank_indexed(&index, &request, &cancellation);
                cancelled = !result.is_complete();
                scanned_rows = result.diagnostics.scanned_rows;
                matched_rows = result.diagnostics.matched_rows;
                result.hits.into_iter().map(|hit| hit.index).collect()
            };
            cancelled |= cancellation.is_cancelled();
            let _ = tx.send(CompareWorkEvent::MediaDisplayDone {
                key: result_key,
                request_key,
                indices: Arc::new(if cancelled { Vec::new() } else { indices }),
                elapsed_ms: started.elapsed().as_millis() as u64,
                scanned_rows,
                matched_rows,
                cancelled,
            });
            repaint.request_repaint();
        });
        Arc::clone(&self.media_display_cache)
    }

    /// Library panel: folder strip pinned at the top of the scroll content (it
    /// scrolls away with the grid) + virtualized thumbnail grid.
    fn draw_media_library_panel(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        lane_id: usize,
        display: &[usize],
        request: &mut CompareLaneRenderRequest,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
        // A non-empty lane folder is the cached navigation state. Scan and
        // child-folder workers validate it away from this hot render path.
        let has_folder = !folder.is_empty();
        // WP-067: a collection tab legitimately has no folder — its rows come
        // from the metadata cache. Without this, the favourites grid rendered
        // the "Choose a folder to browse" empty state over real rows.
        let is_collection =
            self.media_tabs.active().viewport.kind == crate::media_tabs::MediaTabKind::Collection;

        // Empty state: no folder chosen yet.
        if !has_folder && !is_collection {
            let mut child = ui.child_ui(rect, egui::Layout::top_down(egui::Align::Center));
            child.add_space(rect.height() * 0.35);
            child.label(
                egui::RichText::new("Choose a folder to browse")
                    .heading()
                    .color(theme::ink_soft()),
            );
            child.add_space(8.0);
            child.horizontal(|ui| {
                ui.add_space(rect.width() / 2.0 - 120.0);
                if theme::primary_button(ui, &format!("{} Browse…", icons::FOLDER_OPEN)).clicked()
                {
                    request.browse = true;
                }
                let mut path_input = self.compare_lanes[pos].folder.clone();
                let resp = ui.add(
                    TextEdit::singleline(&mut path_input)
                        .desired_width(200.0)
                        .hint_text("or paste a path"),
                );
                if resp.changed() {
                    self.compare_lanes[pos].folder = path_input;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    request.scan = true;
                }
            });
            return;
        }

        let mut child = ui.child_ui(rect, egui::Layout::top_down(egui::Align::Min));
        let ppp = ui.ctx().pixels_per_point();
        let tile_edge = self.media_explorer.tile_edge;
        let cache_edge = crate::media_thumbs::edge_for_display(tile_edge * ppp);
        // A collection tab has no folder to enumerate, so it never shows the
        // drive rail / breadcrumb / child-folder strip (WP-067).
        let child_folders = if is_collection {
            Arc::new(Vec::new())
        } else {
            self.media_child_folders(lane_id, &folder)
        };
        let file_count = self.compare_lanes[pos].files.len();
        let selected_snapshot: HashSet<usize> = self.compare_lanes[pos].selected_files.clone();
        let query_active = !self.media_search_query.trim().is_empty();

        let mut navigate_to: Option<String> = None;
        let mut clicked_tile: Option<(usize, bool, bool)> = None; // (display_idx, ctrl, shift)
        let mut context_tile: Option<usize> = None;
        let mut double_clicked: Option<usize> = None;
        let mut inline_video_action: Option<(usize, String)> = None;
        let mut inline_video_seen = false;
        let mut zoom_factor: f32 = 1.0;
        let mut visible_files: Vec<String> = Vec::new(); // paths to request (visible band)
        let mut prefetch_files: Vec<String> = Vec::new(); // paths to request (overscan band)

        let strip_max = self.media_explorer.strip_height.clamp(
            crate::media_explorer::STRIP_MIN,
            crate::media_explorer::STRIP_MAX,
        );
        let active_media_tab_id = self.media_tabs.active_id().as_str().to_string();
        let scroll_out = ScrollArea::vertical()
            .id_source(("media_grid_scroll", &active_media_tab_id))
            .auto_shrink([false, false])
            .show_viewport(&mut child, |ui, viewport| {
                let content_top = ui.cursor().min.y;

                // ---- folder strip (scrolls away with the grid) ----
                // WP-067: a collection tab is not a filesystem location, so the
                // drive rail and breadcrumbs are meaningless there.
                if !is_collection {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} Drives", icons::HARD_DRIVES)).small(),
                        );
                        for root in crate::media_explorer::filesystem_roots() {
                            let selected = crate::media_explorer::path_is_on_root(&folder, &root);
                            let label = root.trim_end_matches(['\\', '/']);
                            if ui.selectable_label(selected, label).clicked() {
                                navigate_to = Some(root);
                            }
                        }
                    });
                }
                if !is_collection {
                    ui.horizontal_wrapped(|ui| {
                    // The separator lives INSIDE each crumb label ("name /")
                    // so wrapping can never orphan a "/" onto the next row
                    // (a standalone separator label wrapped away from its
                    // crumb and overlapped the next one).
                    let crumbs = crate::media_explorer::breadcrumbs(&folder);
                    let last = crumbs.len().saturating_sub(1);
                    for (i, (label, path)) in crumbs.into_iter().enumerate() {
                        let text = if i == last {
                            label
                        } else {
                            format!("{label} /")
                        };
                        if ui
                            .selectable_label(false, egui::RichText::new(text).small())
                            .clicked()
                        {
                            navigate_to = Some(path.clone());
                        }
                    }
                    });
                }
                let child_count = child_folders.len();
                if child_count > 0 {
                    // +1 row for the '..' parent entry; per-row height must
                    // include item spacing or the last row gets clipped.
                    let row_h =
                        ui.spacing().interact_size.y.max(24.0) + ui.spacing().item_spacing.y;
                    let list_h = (((child_count + 1) as f32) * row_h + 4.0).min(strip_max);
                    // WP-070: floating scrollbars reserve no layout width, so a
                    // scrollable folder strip nested inside the scrollable grid
                    // drew its bar on top of the grid's bar at the same right
                    // edge. Inset the strip by one bar lane, and only when the
                    // strip actually scrolls, so short strips keep full width
                    // and folder names are not truncated.
                    let strip_scrolls = ((child_count + 1) as f32) * row_h + 4.0 > strip_max;
                    let strip_inset = if strip_scrolls {
                        ui.spacing().scroll.bar_width + 4.0
                    } else {
                        0.0
                    };
                    let strip_width = (ui.available_width() - strip_inset).max(48.0);
                    ScrollArea::vertical()
                        .id_source(("media_folder_strip", &active_media_tab_id))
                        .max_height(list_h)
                        .max_width(strip_width)
                        .auto_shrink([false, true])
                        .show_rows(ui, row_h, child_count + 1, |ui, visible_rows| {
                            for row_index in visible_rows {
                                if row_index == 0 {
                                    if ui
                                        .add_sized(
                                            [ui.available_width(), row_h],
                                            egui::SelectableLabel::new(
                                                false,
                                                format!("{} ..", icons::ARROW_ELBOW_LEFT_UP),
                                            ),
                                        )
                                        .on_hover_text("Parent folder (Backspace / Alt+Up)")
                                        .clicked()
                                    {
                                        if let Some(parent) =
                                            Path::new(&folder).parent().and_then(|p| p.to_str())
                                        {
                                            navigate_to = Some(parent.to_string());
                                        }
                                    }
                                    continue;
                                }
                                let name = &child_folders[row_index - 1];
                                let full =
                                    Path::new(&folder).join(name).to_string_lossy().to_string();
                                let row = ui.add_sized(
                                    [ui.available_width(), row_h],
                                    egui::SelectableLabel::new(
                                        false,
                                        format!("{} {}", icons::FOLDER, name),
                                    ),
                                );
                                if row.clicked() {
                                    navigate_to = Some(full.clone());
                                }
                                row.context_menu(|ui| {
                                    if ui.button("Open").clicked() {
                                        navigate_to = Some(full.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button("Open folder location").clicked() {
                                        request.open_path_in_system = Some(full.clone());
                                        ui.close_menu();
                                    }
                                    let fav_key = self.media_db.key_for(&full);
                                    let is_fav = self.media_favorite_keys.contains(&fav_key);
                                    if ui
                                        .button(if is_fav {
                                            "Remove favorite"
                                        } else {
                                            "Add to favorites"
                                        })
                                        .clicked()
                                    {
                                        self.media_toggle_favorite(&full);
                                        ui.close_menu();
                                    }
                                });
                            }
                        });
                    // Strip height drag handle.
                    let (handle_rect, handle_resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 7.0),
                        Sense::click_and_drag(),
                    );
                    let y = handle_rect.center().y;
                    let handle_half = 18.0;
                    ui.painter().line_segment(
                        [
                            egui::pos2(handle_rect.center().x - handle_half, y),
                            egui::pos2(handle_rect.center().x + handle_half, y),
                        ],
                        if handle_resp.hovered() || handle_resp.dragged() {
                            egui::Stroke::new(2.0, theme::ink())
                        } else {
                            theme::rule_stroke()
                        },
                    );
                    if handle_resp.dragged() {
                        self.media_explorer.strip_height =
                            (self.media_explorer.strip_height + handle_resp.drag_delta().y).clamp(
                                crate::media_explorer::STRIP_MIN,
                                crate::media_explorer::STRIP_MAX,
                            );
                        self.touch_media_settings();
                    }
                    if handle_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }
                }

                // ---- media count line ----
                let count_text = if query_active {
                    format!(
                        "{} / {}",
                        group_thousands(display.len()),
                        group_thousands(file_count)
                    )
                } else {
                    group_thousands(file_count)
                };
                ui.label(
                    egui::RichText::new(count_text)
                        .small()
                        .color(theme::ink_faint()),
                );

                if display.is_empty() {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(if file_count == 0 {
                            "No media in this folder."
                        } else {
                            "No media matches this search."
                        })
                        .color(theme::ink_soft()),
                    );
                    return;
                }

                // ---- virtualized grid ----
                let avail_w = ui.available_width();
                let layout = crate::media_explorer::grid_layout(
                    avail_w,
                    tile_edge,
                    display.len(),
                    self.media_explorer.show_names,
                );
                // Keyboard navigation must use the columns the grid ACTUALLY
                // rendered with (recomputing from the full tab width made
                // arrows drift diagonally in TwoPanel mode).
                self.media_explorer.last_grid_columns = layout.columns;
                let grid_start_abs = ui.cursor().min.y;
                let strip_offset = grid_start_abs - content_top;
                let (grid_rect_alloc, grid_resp) = ui.allocate_exact_size(
                    egui::vec2(avail_w, layout.content_height),
                    Sense::hover(),
                );
                // Ctrl+wheel zoom: egui-winit surfaces ctrl+wheel as
                // Event::Zoom (raw_scroll_delta stays 0), so read zoom_delta.
                if grid_resp.hovered() {
                    let zoom = ui.input(|i| i.zoom_delta());
                    if (zoom - 1.0).abs() > 0.001 {
                        zoom_factor = zoom;
                    }
                }
                let current_sort = (self.media_explorer.sort, self.media_explorer.sort_desc);
                grid_resp.context_menu(|ui| {
                    context_tile = Some(usize::MAX); // background context menu
                    let has_selection = !selected_snapshot.is_empty();
                    let can_paste = !self.compare_clipboard.is_empty();
                    Self::draw_media_context_menu(
                        ui,
                        request,
                        &self.media_label_definitions,
                        has_selection || file_count > 0,
                        has_selection,
                        file_count > 0,
                        true,
                        can_paste,
                        None,
                        current_sort,
                    );
                });

                // Controller/keyboard navigation keeps the cursor visible:
                // compute the cursor tile's rect from pure layout math (it
                // may be outside the painted range) and scroll to it.
                if self.media_scroll_to_cursor {
                    if let Some(cursor) = self.media_explorer.cursor {
                        let row = cursor / layout.columns.max(1);
                        let col = cursor % layout.columns.max(1);
                        let target = egui::Rect::from_min_size(
                            egui::pos2(
                                grid_rect_alloc.min.x
                                    + col as f32
                                        * (layout.tile_w + crate::media_explorer::TILE_GAP),
                                grid_rect_alloc.min.y
                                    + row as f32
                                        * (layout.tile_h + crate::media_explorer::TILE_GAP),
                            ),
                            egui::vec2(layout.tile_w, layout.tile_h),
                        );
                        ui.scroll_to_rect(target, None);
                    }
                    self.media_scroll_to_cursor = false;
                }
                let grid_viewport_top = (viewport.min.y - strip_offset).max(0.0);
                let visible = crate::media_explorer::visible_range(
                    &layout,
                    grid_viewport_top,
                    viewport.height(),
                    display.len(),
                );
                // Track scroll movement for stale-job cancellation.
                if (grid_viewport_top - self.media_explorer.last_scroll_top).abs() > layout.tile_h {
                    self.media_explorer.last_scroll_top = grid_viewport_top;
                    if let Some(engine) = self.thumb_engine.as_ref() {
                        engine.bump_generation();
                    }
                }

                let row_h = layout.tile_h + crate::media_explorer::TILE_GAP;
                let painter = ui.painter().clone();
                // Cut files render dimmed (pending-move affordance).
                let cut_set: HashSet<String> = if self.compare_clipboard_cut {
                    self.compare_clipboard.iter().cloned().collect()
                } else {
                    HashSet::new()
                };
                for display_idx in visible.clone() {
                    let file_idx = display[display_idx];
                    let Some(path) = self.compare_lanes[pos].files.get(file_idx).cloned() else {
                        continue;
                    };
                    let col = display_idx % layout.columns;
                    let row = display_idx / layout.columns;
                    let tile_min = egui::pos2(
                        grid_rect_alloc.min.x
                            + col as f32 * (layout.tile_w + crate::media_explorer::TILE_GAP),
                        grid_rect_alloc.min.y + row as f32 * row_h,
                    );
                    let tile_rect = egui::Rect::from_min_size(
                        tile_min,
                        egui::vec2(layout.tile_w, layout.tile_h),
                    );
                    if !ui.is_rect_visible(tile_rect) {
                        continue;
                    }
                    let selected = selected_snapshot.contains(&file_idx);
                    let is_cursor = self.media_explorer.cursor == Some(display_idx);
                    let id = ui.id().with(("media_tile", file_idx));
                    let resp = ui.interact(tile_rect, id, Sense::click());
                    let is_cut = cut_set.contains(path.as_str());
                    self.paint_media_tile(
                        &painter,
                        tile_rect,
                        &path,
                        cache_edge,
                        selected,
                        is_cursor,
                        self.media_explorer.show_names,
                    );
                    let caption_h = if self.media_explorer.show_names {
                        crate::media_explorer::TILE_CAPTION_H
                    } else {
                        0.0
                    };
                    let image_rect = egui::Rect::from_min_max(
                        tile_rect.min,
                        egui::pos2(tile_rect.max.x, tile_rect.max.y - caption_h),
                    );
                    if crate::media_explorer::is_video_path(&path) {
                        if self.media_inline_video_path.as_deref() == Some(path.as_str()) {
                            inline_video_seen = true;
                            self.draw_media_inline_video_tile(ui, image_rect, &path, lane_id);
                        } else {
                            let button_rect = egui::Rect::from_min_size(
                                egui::pos2(image_rect.min.x + 7.0, image_rect.max.y - 39.0),
                                egui::vec2(32.0, 32.0),
                            );
                            painter.circle_filled(
                                button_rect.center(),
                                15.0,
                                egui::Color32::from_black_alpha(150),
                            );
                            painter.text(
                                button_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                icons::PLAY,
                                egui::FontId::proportional(18.0),
                                egui::Color32::WHITE,
                            );
                            if ui
                                .interact(button_rect, id.with("inline_play"), Sense::click())
                                .on_hover_text("Play inside this thumbnail")
                                .clicked()
                            {
                                inline_video_action = Some((display_idx, path.clone()));
                            }
                        }
                    }
                    if is_cut {
                        // Dim pending-move tiles with a translucent wash.
                        painter.rect_filled(
                            tile_rect,
                            egui::Rounding::ZERO,
                            theme::desk().gamma_multiply(0.55),
                        );
                    }
                    visible_files.push(path.clone());
                    if resp.clicked() {
                        let (ctrl, shift) = ui.input(|i| {
                            (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift)
                        });
                        clicked_tile = Some((display_idx, ctrl, shift));
                    }
                    if resp.double_clicked() {
                        double_clicked = Some(file_idx);
                    }
                    resp.context_menu(|ui| {
                        context_tile = Some(display_idx);
                        let clicked_is_selected = selected_snapshot.contains(&file_idx);
                        let was_multi = selected_snapshot.len() > 1;
                        let open_index_for_menu = if was_multi && clicked_is_selected {
                            None
                        } else {
                            Some(file_idx)
                        };
                        let can_paste = !self.compare_clipboard.is_empty();
                        Self::draw_media_context_menu(
                            ui,
                            request,
                            &self.media_label_definitions,
                            true,
                            true,
                            file_count > 0,
                            true,
                            can_paste,
                            open_index_for_menu,
                            current_sort,
                        );
                    });
                    if resp.hovered() {
                        resp.on_hover_text(&path);
                    }
                }

                // Overscan band beyond the visible range decodes at Prefetch
                // priority — scrolling into it finds warm tiles, and the
                // generation counter can cancel it when scrolling turns.
                let prefetch_rows = 3 * layout.columns;
                let below = visible.end..(visible.end + prefetch_rows).min(display.len());
                let above = visible.start.saturating_sub(prefetch_rows)..visible.start;
                for display_idx in above.chain(below) {
                    if let Some(path) = display
                        .get(display_idx)
                        .and_then(|&fi| self.compare_lanes[pos].files.get(fi))
                    {
                        prefetch_files.push(path.clone());
                    }
                }
            });
        let _ = scroll_out;

        // ---- deferred mutations (lane borrow released) ----
        if let Some(next) = navigate_to {
            if let Some(pos) = self.compare_lane_position(lane_id) {
                self.compare_lanes[pos].folder = next;
                request.scan = true;
                self.media_explorer.cursor = None;
            }
        }
        if (zoom_factor - 1.0).abs() > 0.001 {
            self.media_explorer.tile_edge = (self.media_explorer.tile_edge * zoom_factor).clamp(
                crate::media_explorer::TILE_MIN,
                crate::media_explorer::TILE_MAX,
            );
            self.touch_media_settings();
        }
        if let Some((display_idx, ctrl, shift)) = clicked_tile {
            self.media_apply_tile_click(lane_id, display, display_idx, ctrl, shift);
        }
        if let Some((display_idx, path)) = inline_video_action {
            // A transport click also makes its tile the selected/contextual
            // item without opening an external player.
            self.media_apply_tile_click(lane_id, display, display_idx, false, false);
            let result = if self.video_player.active_path() == Some(path.as_str()) {
                self.video_player.toggle_pause()
            } else {
                self.play_media_video(Path::new(&path))
            };
            match result {
                Ok(()) => {
                    self.media_inline_video_path = Some(path);
                    self.media_inline_video_requested_at = Some(std::time::Instant::now());
                    self.media_inline_video_pending_target = None;
                    inline_video_seen = true;
                    self.begin_media_playback_priority();
                }
                Err(error) => {
                    self.media_inline_video_path = None;
                    self.media_inline_video_requested_at = None;
                    self.media_inline_video_pending_target = None;
                    self.set_compare_lane_message(lane_id, error);
                }
            }
        }
        // Publish Library ownership for this frame before the Viewer panel is
        // drawn, so the Viewer can tell real Library ownership from an
        // abandoned surface (WP-065).
        self.media_inline_video_seen = inline_video_seen;
        let scan_reconciling = self
            .compare_lane_position(lane_id)
            .is_some_and(|pos| self.compare_lanes[pos].scanning);
        if self.media_inline_video_path.is_some() && !inline_video_seen {
            // A progressive scan can publish the selected video, then hide
            // its traversal-order tile while the full inventory is sorted and
            // committed. Keep the explicitly requested decoder alive through
            // that bounded reconciliation; ScanDone re-establishes its exact
            // terminal placement. Outside a scan, retain the normal 10-second
            // invisible-tile safety cutoff.
            let awaiting_scroll = keep_inline_video_awaiting(
                scan_reconciling,
                self.media_inline_video_requested_at
                    .map(|started| started.elapsed()),
            );
            if awaiting_scroll {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(if scan_reconciling {
                        100
                    } else {
                        16
                    }));
            } else {
                // Never keep an invisible decoder/audio stream alive after its
                // virtualized tile scrolls or filters out of the rendered set.
                self.video_player.stop();
                self.media_inline_video_path = None;
                self.media_inline_video_requested_at = None;
                self.media_inline_video_pending_target = None;
                self.media_playback_lease = None;
            }
        } else if inline_video_seen {
            if !preserve_inline_request_anchor(scan_reconciling) {
                self.media_inline_video_requested_at = None;
            }
            self.media_inline_video_pending_target = None;
        }
        if let Some(display_idx) = context_tile {
            if display_idx != usize::MAX {
                // Right-click selects the tile when it wasn't in the selection.
                if let Some(pos) = self.compare_lane_position(lane_id) {
                    let file_idx = display[display_idx];
                    if !self.compare_lanes[pos].selected_files.contains(&file_idx) {
                        self.compare_lanes[pos].selected_files.clear();
                        self.compare_lanes[pos].selected_files.insert(file_idx);
                        self.compare_lanes[pos].selection_anchor = Some(file_idx);
                        self.media_explorer.cursor = Some(display_idx);
                        self.media_sync_preview(lane_id, file_idx);
                    }
                }
            }
        }
        if let Some(file_idx) = double_clicked {
            request.open_index_in_system = Some(file_idx);
        }
        // Queue thumbnails: visible band first, then the overscan prefetch
        // band. CRITICAL: skip anything that already has an uploaded texture
        // — requesting completed keys every frame created a self-sustaining
        // decode/repaint loop with unbounded channel growth (review B1).
        if let Some(engine) = self.thumb_engine.as_mut() {
            let bands = [
                (&visible_files, crate::media_thumbs::ThumbPriority::Visible),
                (
                    &prefetch_files,
                    crate::media_thumbs::ThumbPriority::Prefetch,
                ),
            ];
            for (paths, priority) in bands {
                for path in paths.iter() {
                    let key = crate::media_thumbs::ThumbKey {
                        path: path.clone(),
                        edge: cache_edge,
                    };
                    if self.thumb_textures.contains(&key) {
                        continue;
                    }
                    engine.request(path, cache_edge, priority);
                }
            }
        }
    }

    /// Host the one shared native LibVLC player in a visible grid tile. A
    /// small bottom strip remains outside the child HWND so egui controls stay
    /// clickable; hover expands it to scrubber + volume without allocating a
    /// second decoder or doing any work for other video tiles.
    fn draw_media_inline_video_tile(
        &mut self,
        ui: &mut egui::Ui,
        image_rect: egui::Rect,
        path: &str,
        lane_id: usize,
    ) {
        let hovered = ui
            .ctx()
            .pointer_hover_pos()
            .is_some_and(|position| image_rect.contains(position));
        let controls_h =
            (if hovered { 68.0_f32 } else { 36.0_f32 }).min(image_rect.height() * 0.55);
        let player_rect = egui::Rect::from_min_max(
            image_rect.min,
            egui::pos2(
                image_rect.max.x,
                (image_rect.max.y - controls_h).max(image_rect.min.y),
            ),
        );
        let controls_rect = egui::Rect::from_min_max(
            egui::pos2(image_rect.min.x, player_rect.max.y),
            image_rect.max,
        );
        let obscured = self.media_explorer.show_settings
            || self.settings_backdrop_requested_at.is_some()
            || self.media_explorer.show_favorites
            || self.media_folder_navigator_active()
            || self.folder_picker.is_open();
        if obscured {
            self.video_player.hide();
        } else if let Err(error) = self.video_player.show_clipped(
            player_rect.shrink(1.0),
            // WP-065: the Library tile lives inside a virtualized ScrollArea.
            // Without the scroll clip rect a half-scrolled tile placed a
            // full-size, top-of-Z native child over the toolbar and Viewer.
            Some(ui.clip_rect()),
            ui.ctx().pixels_per_point(),
        ) {
            self.set_compare_lane_message(lane_id, error);
        }

        let snapshot = self
            .video_player
            .snapshot()
            .filter(|state| state.path == path);
        self.sync_media_playback_priority(snapshot.as_ref());
        ui.painter().rect_filled(
            controls_rect,
            0.0,
            egui::Color32::from_black_alpha(if hovered { 150 } else { 118 }),
        );
        let mut controls = ui.child_ui(
            controls_rect.shrink2(egui::vec2(6.0, 3.0)),
            egui::Layout::top_down(egui::Align::Min),
        );
        let transport = snapshot
            .as_ref()
            .map(|state| {
                if state.playing {
                    icons::PAUSE
                } else {
                    icons::PLAY
                }
            })
            .unwrap_or(icons::PLAY);
        controls.horizontal(|ui| {
            if ui
                .add_sized(
                    [30.0, 28.0],
                    egui::Button::new(
                        egui::RichText::new(transport)
                            .size(17.0)
                            .color(egui::Color32::WHITE),
                    )
                    .frame(false),
                )
                .on_hover_text("Play / pause")
                .clicked()
            {
                match self.video_player.toggle_pause() {
                    Ok(()) => self.begin_media_playback_priority(),
                    Err(error) => self.set_compare_lane_message(lane_id, error),
                }
            }
            if hovered {
                if let Some(state) = snapshot.as_ref() {
                    let mut time = state.time_ms as f64;
                    let length = state.length_ms.max(1) as f64;
                    let width = (ui.available_width() - 8.0).max(48.0);
                    theme::transport_slider(ui, width);
                    if ui
                        .add_sized(
                            [width, 24.0],
                            egui::Slider::new(&mut time, 0.0..=length)
                                .show_value(false)
                                .clamp_to_range(true),
                        )
                        .on_hover_text(format!(
                            "{} / {}",
                            format_media_time(state.time_ms),
                            format_media_time(state.length_ms)
                        ))
                        .changed()
                    {
                        match self.video_player.set_time(time.round() as i64) {
                            Ok(()) => self.begin_media_playback_priority(),
                            Err(error) => self.set_compare_lane_message(lane_id, error),
                        }
                    }
                }
            }
        });
        if hovered {
            if let Some(state) = snapshot.as_ref() {
                controls.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(icons::SPEAKER_HIGH)
                            .small()
                            .color(egui::Color32::WHITE),
                    );
                    let mut volume = state.volume.clamp(0, 125);
                    let volume_width = ui.available_width().max(48.0);
                    theme::transport_slider(ui, volume_width);
                    if ui
                        .add_sized(
                            [volume_width, 22.0],
                            egui::Slider::new(&mut volume, 0..=125).show_value(false),
                        )
                        .on_hover_text("Volume")
                        .changed()
                    {
                        match self.video_player.set_volume(volume) {
                            Ok(()) => self.begin_media_playback_priority(),
                            Err(error) => self.set_compare_lane_message(lane_id, error),
                        }
                    }
                });
            }
        }
        if snapshot.as_ref().is_some_and(|state| state.playing)
            || controls.input(|input| input.pointer.any_down())
        {
            controls
                .ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    /// Paint one grid tile: thumbnail (or placeholder / video / error tile),
    /// name caption, selection + cursor strokes, label dot, favorite star.
    #[allow(clippy::too_many_arguments)]
    fn paint_media_tile(
        &mut self,
        painter: &egui::Painter,
        tile_rect: egui::Rect,
        path: &str,
        cache_edge: u16,
        selected: bool,
        is_cursor: bool,
        show_names: bool,
    ) {
        let caption_h = if show_names {
            crate::media_explorer::TILE_CAPTION_H
        } else {
            0.0
        };
        let image_rect = egui::Rect::from_min_max(
            tile_rect.min,
            egui::pos2(tile_rect.max.x, tile_rect.max.y - caption_h),
        );
        // Borderless tile background + selection fill. The image itself and
        // whitespace define the tile; only the accent cursor may draw a rule.
        painter.rect(
            image_rect,
            egui::Rounding::ZERO,
            if selected {
                theme::selection_bg()
            } else {
                theme::well()
            },
            egui::Stroke::NONE,
        );

        let key = crate::media_thumbs::ThumbKey {
            path: path.to_string(),
            edge: cache_edge,
        };
        let is_video = crate::media_explorer::is_video_path(path);
        let failure = self
            .thumb_engine
            .as_ref()
            .and_then(|e| e.failure(path, cache_edge).map(String::from));
        if let Some(texture) = self.thumb_textures.get(&key) {
            let tex_size = texture.size_vec2();
            let fitted = fit_for_compare_frame(
                tex_size,
                egui::vec2(image_rect.width() - 4.0, image_rect.height() - 4.0),
            );
            let draw_rect = egui::Rect::from_center_size(image_rect.center(), fitted);
            painter.image(
                texture.id(),
                draw_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else if is_video {
            painter.text(
                image_rect.center(),
                egui::Align2::CENTER_CENTER,
                icons::FILM_STRIP,
                egui::FontId::proportional(image_rect.height() * 0.42),
                theme::ink_soft(),
            );
            let ext = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("video")
                .to_ascii_uppercase();
            painter.text(
                egui::pos2(image_rect.center().x, image_rect.max.y - 12.0),
                egui::Align2::CENTER_CENTER,
                ext,
                egui::FontId::proportional(11.0),
                theme::ink_faint(),
            );
        } else if failure.is_some() {
            painter.text(
                image_rect.center(),
                egui::Align2::CENTER_CENTER,
                icons::WARNING,
                egui::FontId::proportional(image_rect.height() * 0.3),
                theme::error_ink(),
            );
        } else {
            painter.text(
                image_rect.center(),
                egui::Align2::CENTER_CENTER,
                icons::IMAGE,
                egui::FontId::proportional(image_rect.height() * 0.3),
                theme::rule(),
            );
        }

        // Bounded label lane (top-right) + favorite star (top-left). Rendering
        // remains proportional to visible tiles; long catalogs collapse to +N.
        // WP-069: the canonical key is a pure string transform, but it was
        // recomputed and allocated for every visible tile on every frame. Cache
        // it per path so scrolling a large grid stops churning the allocator.
        let meta_key = match self.media_tile_key_cache.get(path) {
            Some(cached) => cached.clone(),
            None => {
                let computed = self.media_db.key_for(path);
                if self.media_tile_key_cache.len() > MAX_MEDIA_TILE_KEY_CACHE {
                    self.media_tile_key_cache.clear();
                }
                self.media_tile_key_cache
                    .insert(path.to_string(), computed.clone());
                computed
            }
        };
        if let Some(cache_lookups) = self.debug_label_paint_probe.as_mut() {
            *cache_lookups = cache_lookups.saturating_add(1);
        }
        if let Some(labels) = self.media_color_labels.get(&meta_key) {
            let shown = labels.len().min(3);
            for (offset, label) in labels.iter().take(shown).enumerate() {
                painter.circle_filled(
                    egui::pos2(
                        image_rect.max.x - 10.0 - offset as f32 * 13.0,
                        image_rect.min.y + 10.0,
                    ),
                    5.0,
                    self.media_label_colors
                        .get(label)
                        .copied()
                        .unwrap_or_else(|| egui::Color32::from_rgb(128, 128, 128)),
                );
            }
            if labels.len() > shown {
                painter.text(
                    egui::pos2(
                        image_rect.max.x - 12.0 - shown as f32 * 13.0,
                        image_rect.min.y + 10.0,
                    ),
                    egui::Align2::RIGHT_CENTER,
                    format!("+{}", labels.len() - shown),
                    egui::FontId::proportional(10.0),
                    theme::ink(),
                );
            }
        }
        if self.media_favorite_keys.contains(&meta_key) {
            painter.text(
                egui::pos2(image_rect.min.x + 12.0, image_rect.min.y + 10.0),
                egui::Align2::CENTER_CENTER,
                icons::STAR,
                egui::FontId::proportional(12.0),
                theme::ink(),
            );
        }

        // Selection is fill-only; the focused cursor keeps a functional
        // vermilion rule rather than restoring the removed black tile frame.
        if is_cursor {
            painter.rect_stroke(
                image_rect.expand(1.5),
                egui::Rounding::ZERO,
                egui::Stroke::new(1.0, theme::accent()),
            );
        }

        if show_names {
            let name = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path);
            painter.text(
                egui::pos2(
                    tile_rect.center().x,
                    tile_rect.max.y - crate::media_explorer::TILE_CAPTION_H / 2.0,
                ),
                egui::Align2::CENTER_CENTER,
                elide_middle(name, (tile_rect.width() / 6.5) as usize),
                egui::TextStyle::Small.resolve(&painter.ctx().style()),
                if selected {
                    theme::ink()
                } else {
                    theme::ink_soft()
                },
            );
        }
    }

    /// Click semantics: plain = single-select + cursor; Ctrl = toggle;
    /// Shift = range from the display-space anchor (Explorer-style).
    fn media_apply_tile_click(
        &mut self,
        lane_id: usize,
        display: &[usize],
        display_idx: usize,
        ctrl: bool,
        shift: bool,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        // A stored cursor can outlive a shrinking display list (filter/sort
        // change between frames) — never index unchecked (review B2 panic).
        let Some(&file_idx) = display.get(display_idx) else {
            self.media_explorer.cursor = display.len().checked_sub(1);
            return;
        };
        let lane = &mut self.compare_lanes[pos];
        if shift {
            let anchor = self
                .media_explorer
                .sel_anchor_display
                .unwrap_or(display_idx);
            let lo = anchor.min(display_idx);
            let hi = anchor.max(display_idx);
            lane.selected_files.clear();
            for d in lo..=hi {
                if let Some(&fi) = display.get(d) {
                    lane.selected_files.insert(fi);
                }
            }
        } else if ctrl {
            if lane.selected_files.contains(&file_idx) {
                lane.selected_files.remove(&file_idx);
            } else {
                lane.selected_files.insert(file_idx);
            }
            self.media_explorer.sel_anchor_display = Some(display_idx);
        } else {
            lane.selected_files.clear();
            lane.selected_files.insert(file_idx);
            self.media_explorer.sel_anchor_display = Some(display_idx);
        }
        lane.selection_anchor = Some(file_idx);
        self.media_explorer.cursor = Some(display_idx);
        self.media_sync_preview(lane_id, file_idx);
    }

    /// Point the preview at a file index and kick its async load.
    fn media_sync_preview(&mut self, lane_id: usize, file_idx: usize) {
        if let Some(pos) = self.compare_lane_position(lane_id) {
            if self.compare_lanes[pos].index != file_idx {
                self.compare_lanes[pos].index = file_idx;
                self.request_compare_image(lane_id, file_idx);
            } else if self.compare_lanes[pos].texture.is_none() {
                self.request_compare_image(lane_id, file_idx);
            }
        }
    }

    /// Viewer panel: fitted preview + filename + metadata editors.
    fn draw_media_viewer_panel(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let fullscreen = self.media_explorer.chrome_hidden;
        // Fullscreen is a clean media surface: metadata (favorite/rating-like
        // star, labels, tags, notes) is not merely collapsed but not rendered.
        // Normal mode keeps a compact editor instead of reserving 42%/178px.
        let meta_h = if fullscreen {
            0.0
        } else {
            142.0_f32.min(rect.height() * 0.30)
        };
        let image_rect = if fullscreen {
            rect
        } else {
            egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.max.x, (rect.max.y - meta_h).max(rect.min.y + 60.0)),
            )
        };
        let meta_rect =
            egui::Rect::from_min_max(egui::pos2(rect.min.x, image_rect.max.y + 4.0), rect.max);

        // ---- image area ----
        let has_selection = !self.compare_lanes[pos].selected_files.is_empty();
        let can_paste = !self.compare_clipboard.is_empty();
        let has_files = !self.compare_lanes[pos].files.is_empty();
        let active_path = self.media_selected_path(lane_id);
        let texture = self.compare_lanes[pos].texture.clone();
        let image_error = self.compare_lanes[pos].image_error.clone();
        let loading =
            self.compare_lanes[pos].loading_image || self.compare_lanes[pos].loading_image_inflight;
        let lane_index = self.compare_lanes[pos].index;

        let preview_resp = ui.interact(image_rect, ui.id().with("media_preview"), Sense::click());
        if let Some(path) = active_path.clone() {
            if crate::media_explorer::is_video_path(&path) {
                self.draw_media_video_preview(ui, image_rect, &path, lane_id);
            } else if let Some(texture) = texture.as_ref() {
                if self.video_player.active_path().is_some() {
                    self.video_player.stop();
                    self.media_playback_lease = None;
                }
                let fitted = fit_for_compare_frame(
                    texture.size_vec2(),
                    egui::vec2(image_rect.width() - 4.0, image_rect.height() - 4.0),
                );
                let draw_rect = egui::Rect::from_center_size(image_rect.center(), fitted);
                if self.debug_preview_fixture {
                    let cell = egui::vec2(draw_rect.width() / 8.0, draw_rect.height() / 5.0);
                    for row in 0..5 {
                        for column in 0..8 {
                            let min = draw_rect.min
                                + egui::vec2(column as f32 * cell.x, row as f32 * cell.y);
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(min, cell),
                                0.0,
                                if (row + column) % 2 == 0 {
                                    egui::Color32::from_rgb(72, 116, 164)
                                } else {
                                    egui::Color32::from_rgb(194, 137, 86)
                                },
                            );
                        }
                    }
                }
                ui.painter().image(
                    texture.id(),
                    draw_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else if !image_error.is_empty() {
                ui.painter().text(
                    image_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{} {}", icons::WARNING, image_error),
                    egui::TextStyle::Body.resolve(ui.style()),
                    theme::error_ink(),
                );
            } else if loading {
                ui.painter().text(
                    image_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "loading…",
                    egui::TextStyle::Body.resolve(ui.style()),
                    theme::ink_faint(),
                );
            }
        } else {
            if self.video_player.active_path().is_some() {
                self.video_player.stop();
                self.media_playback_lease = None;
            }
            ui.painter().text(
                image_rect.center(),
                egui::Align2::CENTER_CENTER,
                "select a thumbnail",
                egui::TextStyle::Body.resolve(ui.style()),
                theme::ink_faint(),
            );
        }
        if preview_resp.double_clicked() {
            request.open_index_in_system = Some(lane_index);
        }
        let current_sort = (self.media_explorer.sort, self.media_explorer.sort_desc);
        preview_resp.context_menu(|ui| {
            let open_index = if has_selection {
                None
            } else {
                Some(lane_index)
            };
            Self::draw_media_context_menu(
                ui,
                request,
                &self.media_label_definitions,
                true,
                has_selection,
                has_files,
                true,
                can_paste,
                open_index,
                current_sort,
            );
        });

        if fullscreen {
            return;
        }

        // ---- metadata block ----
        let mut meta_ui = ui.child_ui(meta_rect, egui::Layout::top_down(egui::Align::Min));
        // Bound the metadata block to its own rect. `horizontal_wrapped` wraps
        // at the available width, and without this a long row of assigned label
        // chips kept extending past the Viewer panel and off the window edge
        // instead of wrapping (seen in the media_labels_multi snapshot).
        meta_ui.set_max_width(meta_rect.width());
        meta_ui.set_clip_rect(meta_rect.intersect(ui.clip_rect()));
        theme::hairline(&mut meta_ui);
        let editable = self.media_db.is_writable();
        if let Some(path) = active_path {
            let key = self.media_key(&path);
            let stat = self.media_explorer.stats.get(&path).copied();
            meta_ui.horizontal(|ui| {
                let name = Path::new(&path)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file");
                ui.label(
                    egui::RichText::new(elide_middle(name, 36))
                        .small()
                        .color(theme::ink_faint()),
                )
                .on_hover_text(&path);
                if let Some(size) = stat.and_then(crate::media_explorer::FileStat::size) {
                    ui.label(
                        egui::RichText::new(format!("{:.1} MB", size as f64 / 1e6))
                            .small()
                            .color(theme::ink_faint()),
                    );
                }
                let is_fav = self.media_favorite_keys.contains(&key);
                if ui
                    .add_enabled(editable, egui::SelectableLabel::new(is_fav, icons::STAR))
                    .on_hover_text("Toggle favorite")
                    .clicked()
                {
                    self.media_toggle_favorite(&path);
                }
            });
            let label_definitions = self.media_label_definitions.clone();
            let label_colors = Arc::clone(&self.media_label_colors);
            let mut assigned = self
                .media_color_labels
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let mut labels_changed = false;
            let mut open_creator = false;
            meta_ui.horizontal_wrapped(|ui| {
                for id in &assigned {
                    if let Some(definition) = label_definitions.iter().find(|item| &item.id == id) {
                        let color = label_colors
                            .get(id)
                            .copied()
                            .unwrap_or_else(|| egui::Color32::from_rgb(128, 128, 128));
                        ui.label(
                            egui::RichText::new(format!("● {}", definition.name))
                                .small()
                                .color(color),
                        );
                    }
                }
                ui.menu_button("Labels ▾", |ui| {
                    ui.set_min_width(220.0);
                    ui.label(
                        egui::RichText::new("Choose to add; choose again to remove")
                            .small()
                            .color(theme::ink_faint()),
                    );
                    if ui.small_button("Create custom label…").clicked() {
                        open_creator = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_source("media-viewer-labels")
                        // The trigger sits low in the Viewer metadata band.
                        // Keep the fixed create action reachable at 800px and
                        // high font sizes; the arbitrary catalog scrolls here.
                        .max_height(100.0)
                        .show(ui, |ui| {
                            for definition in &label_definitions {
                                let active = assigned.contains(&definition.id);
                                if ui
                                    .selectable_label(active, format!("● {}", definition.name))
                                    .on_hover_text(&definition.hex)
                                    .clicked()
                                {
                                    if active {
                                        assigned.retain(|id| id != &definition.id);
                                    } else {
                                        assigned.push(definition.id.clone());
                                    }
                                    labels_changed = true;
                                }
                            }
                        });
                });
                if assigned.is_empty() && ui.small_button("Create label").clicked() {
                    open_creator = true;
                }
            });
            if labels_changed && editable {
                if assigned.is_empty() {
                    Arc::make_mut(&mut self.media_color_labels).remove(&key);
                } else {
                    Arc::make_mut(&mut self.media_color_labels).insert(key.clone(), assigned);
                }
                self.touch_media_meta(&key);
            }
            if open_creator && editable {
                self.media_label_create_for_key = Some(key.clone());
                self.media_label_create_name.clear();
            }
            if self.media_label_create_for_key.as_deref() == Some(key.as_str()) {
                let mut create = false;
                let mut cancel = false;
                meta_ui.horizontal(|ui| {
                    ui.color_edit_button_srgb(&mut self.media_label_create_rgb)
                        .on_hover_text("Choose a unique label color");
                    ui.add(
                        TextEdit::singleline(&mut self.media_label_create_name)
                            .desired_width(180.0)
                            .hint_text("Unique label name"),
                    );
                    create = ui
                        .add_enabled(
                            editable && !self.media_label_create_name.trim().is_empty(),
                            egui::Button::new("Create & add"),
                        )
                        .clicked();
                    cancel = ui.small_button("Cancel").clicked();
                });
                if create {
                    let hex = format!(
                        "#{:02X}{:02X}{:02X}",
                        self.media_label_create_rgb[0],
                        self.media_label_create_rgb[1],
                        self.media_label_create_rgb[2]
                    );
                    match self.media_db.create_color_label_and_assign(
                        &path,
                        &self.media_label_create_name,
                        &hex,
                    ) {
                        Ok(definition) => {
                            self.media_label_definitions = self.media_db.color_label_definitions();
                            self.refresh_media_label_colors();
                            Arc::make_mut(&mut self.media_color_labels)
                                .entry(key.clone())
                                .or_default()
                                .push(definition.id);
                            self.media_label_create_for_key = None;
                            self.media_label_create_name.clear();
                            self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
                            self.compare_action_message = "Label created and added".to_string();
                        }
                        Err(error) => {
                            self.compare_action_message = format!("Label not created: {error}");
                        }
                    }
                } else if cancel {
                    self.media_label_create_for_key = None;
                    self.media_label_create_name.clear();
                }
            }
            let mut tags = self.media_tags.get(&key).cloned().unwrap_or_default();
            let tags_resp = meta_ui
                .scope(|ui| {
                    let visuals = &mut ui.style_mut().visuals.widgets;
                    for widget in [
                        &mut visuals.noninteractive,
                        &mut visuals.inactive,
                        &mut visuals.hovered,
                        &mut visuals.active,
                        &mut visuals.open,
                    ] {
                        widget.bg_fill = theme::media_field();
                        widget.weak_bg_fill = theme::media_field();
                        widget.bg_stroke = egui::Stroke::NONE;
                    }
                    ui.add_enabled(
                        editable,
                        TextEdit::singleline(&mut tags)
                            .desired_width(f32::INFINITY)
                            .hint_text("tags, comma separated"),
                    )
                })
                .inner;
            if tags_resp.changed() {
                Arc::make_mut(&mut self.media_tags).insert(key.clone(), tags);
                self.touch_media_meta(&key);
            }
            let mut notes = self.media_notes.get(&key).cloned().unwrap_or_default();
            let notes_resp = meta_ui
                .scope(|ui| {
                    let visuals = &mut ui.style_mut().visuals.widgets;
                    for widget in [
                        &mut visuals.noninteractive,
                        &mut visuals.inactive,
                        &mut visuals.hovered,
                        &mut visuals.active,
                        &mut visuals.open,
                    ] {
                        widget.bg_fill = theme::media_field();
                        widget.weak_bg_fill = theme::media_field();
                        widget.bg_stroke = egui::Stroke::NONE;
                    }
                    ui.add_enabled(
                        editable,
                        TextEdit::multiline(&mut notes)
                            .desired_width(f32::INFINITY)
                            .desired_rows(3)
                            .hint_text("notes"),
                    )
                })
                .inner;
            if notes_resp.changed() {
                Arc::make_mut(&mut self.media_notes).insert(key.clone(), notes);
                self.touch_media_meta(&key);
            }
            if !editable {
                if let Some(status) = self.media_db.status() {
                    meta_ui.label(egui::RichText::new(status).small().color(theme::warn_ink()));
                }
            }
        } else {
            meta_ui.label(
                egui::RichText::new("Tags, notes, and color labels appear here.")
                    .small()
                    .color(theme::ink_faint()),
            );
        }
    }

    fn draw_media_video_preview(
        &mut self,
        ui: &mut egui::Ui,
        image_rect: egui::Rect,
        path: &str,
        lane_id: usize,
    ) {
        // WP-065: only yield the native child to the Library when its tile
        // actually rendered this frame. `media_inline_video_path` alone was not
        // enough: when the owning tile was virtualized out of the grid, filtered
        // away, or waiting on a display order, the Library never placed the
        // surface and the Viewer returned here without placing or hiding it, so
        // no owner touched it. LibVLC kept decoding into a hidden 16x16 child —
        // audio with no picture — or left the previous frame stranded on screen.
        if self.media_inline_video_path.as_deref() == Some(path) && self.media_inline_video_seen {
            // The grid was rendered first and owns the single native child
            // this frame. Paint a passive right-page reference without
            // moving or hiding that child out from under the active tile.
            let key = crate::media_thumbs::ThumbKey {
                path: path.to_string(),
                edge: crate::media_thumbs::edge_for_display(
                    image_rect.width().max(image_rect.height()),
                ),
            };
            if let Some(texture) = self.thumb_textures.get(&key).cloned() {
                let fitted = fit_for_compare_frame(
                    texture.size_vec2(),
                    egui::vec2(image_rect.width() - 4.0, image_rect.height() - 4.0),
                );
                ui.painter().image(
                    texture.id(),
                    egui::Rect::from_center_size(image_rect.center(), fitted),
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            ui.painter().text(
                egui::pos2(image_rect.center().x, image_rect.max.y - 18.0),
                egui::Align2::CENTER_CENTER,
                "Playing in thumbnail",
                egui::TextStyle::Small.resolve(ui.style()),
                theme::ink_faint(),
            );
            return;
        }
        let fullscreen = self.media_explorer.chrome_hidden;
        let pointer_over_video = ui
            .ctx()
            .pointer_hover_pos()
            .is_some_and(|position| image_rect.contains(position));
        let controls_visible = !fullscreen || pointer_over_video;
        let controls_h = if !controls_visible {
            0.0
        } else if fullscreen {
            74.0_f32.min(image_rect.height() * 0.24)
        } else {
            154.0_f32.min(image_rect.height() * 0.34)
        };
        let video_rect = egui::Rect::from_min_max(
            image_rect.min,
            egui::pos2(image_rect.max.x, image_rect.max.y - controls_h),
        );
        let controls_rect = egui::Rect::from_min_max(
            egui::pos2(image_rect.min.x, video_rect.max.y),
            image_rect.max,
        );

        if self
            .video_player
            .active_path()
            .is_some_and(|active| active != path)
        {
            self.video_player.stop();
            self.media_playback_lease = None;
        }
        let active = self.video_player.active_path() == Some(path);
        let obscured = self.media_explorer.show_settings
            || self.settings_backdrop_requested_at.is_some()
            || self.media_explorer.show_favorites
            || self.media_folder_navigator_active()
            || self.folder_picker.is_open();
        if active && !obscured {
            if let Err(error) = self.video_player.show_clipped(
                video_rect.shrink(2.0),
                Some(ui.clip_rect()),
                ui.ctx().pixels_per_point(),
            ) {
                self.set_compare_lane_message(lane_id, error);
            }
        } else {
            self.video_player.hide();
            let key = crate::media_thumbs::ThumbKey {
                path: path.to_string(),
                edge: crate::media_thumbs::edge_for_display(
                    video_rect.width().max(video_rect.height()),
                ),
            };
            let texture = self.thumb_textures.get(&key).cloned();
            if let Some(texture) = texture {
                let fitted = fit_for_compare_frame(
                    texture.size_vec2(),
                    egui::vec2(video_rect.width() - 8.0, video_rect.height() - 8.0),
                );
                ui.painter().image(
                    texture.id(),
                    egui::Rect::from_center_size(video_rect.center(), fitted),
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                ui.painter().text(
                    video_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    icons::FILM_STRIP,
                    egui::FontId::proportional(64.0),
                    theme::ink_soft(),
                );
            }
        }

        let snapshot = self
            .video_player
            .snapshot()
            .filter(|state| state.path == path);
        self.sync_media_playback_priority(snapshot.as_ref());

        if fullscreen {
            if !controls_visible {
                if snapshot.as_ref().is_some_and(|state| state.playing) {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(33));
                }
                return;
            }
            // LibVLC owns a native child HWND, so egui cannot safely paint on
            // top of its pixels. While hovered we shorten that child by this
            // one compact strip and paint the transparent control overlay in
            // the vacated bottom band; when the pointer leaves, video returns
            // to the full panel immediately.
            ui.painter()
                .rect_filled(controls_rect, 0.0, egui::Color32::from_black_alpha(126));
            let mut controls = ui.child_ui(
                controls_rect.shrink2(egui::vec2(10.0, 8.0)),
                egui::Layout::left_to_right(egui::Align::Center),
            );
            controls.spacing_mut().item_spacing.x = 10.0;
            let transport = snapshot
                .as_ref()
                .map(|state| {
                    if state.playing {
                        icons::PAUSE
                    } else {
                        icons::PLAY
                    }
                })
                .unwrap_or(icons::PLAY);
            if controls
                .add_sized(
                    [44.0, 44.0],
                    egui::Button::new(
                        egui::RichText::new(transport)
                            .size(24.0)
                            .color(egui::Color32::WHITE),
                    )
                    .frame(false),
                )
                .on_hover_text("Play / pause")
                .clicked()
            {
                let result = if active {
                    self.video_player.toggle_pause()
                } else {
                    self.play_media_video(Path::new(path))
                };
                match result {
                    Ok(()) => self.begin_media_playback_priority(),
                    Err(error) => self.set_compare_lane_message(lane_id, error),
                }
            }
            if let Some(state) = snapshot.as_ref() {
                let mut time = state.time_ms as f64;
                let length = state.length_ms.max(1) as f64;
                // WP-070: reserve only what the trailing widgets on this row
                // actually need (time label + speaker icon + 90pt volume),
                // instead of a hard-coded 250, so the scrubber grows with the
                // panel at other window sizes and font scales.
                let trailing = controls.spacing().item_spacing.x * 3.0
                    + media_time_label_width(&controls, state.length_ms)
                    + 24.0
                    + 90.0;
                let scrub_width = (controls.available_width() - trailing).max(120.0);
                theme::transport_slider(&mut controls, scrub_width);
                if controls
                    .add_sized(
                        [scrub_width, 40.0],
                        egui::Slider::new(&mut time, 0.0..=length)
                            .show_value(false)
                            .clamp_to_range(true),
                    )
                    .on_hover_text("Scrub timeline")
                    .changed()
                {
                    match self.video_player.set_time(time.round() as i64) {
                        Ok(()) => self.begin_media_playback_priority(),
                        Err(error) => self.set_compare_lane_message(lane_id, error),
                    }
                }
                controls.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_media_time(state.time_ms),
                        format_media_time(state.length_ms)
                    ))
                    .color(egui::Color32::WHITE),
                );
                let mut volume = state.volume.clamp(0, 125);
                controls
                    .label(egui::RichText::new(icons::SPEAKER_HIGH).color(egui::Color32::WHITE));
                theme::transport_slider(&mut controls, 90.0);
                if controls
                    .add_sized(
                        [90.0, 36.0],
                        egui::Slider::new(&mut volume, 0..=125).show_value(false),
                    )
                    .changed()
                {
                    match self.video_player.set_volume(volume) {
                        Ok(()) => self.begin_media_playback_priority(),
                        Err(error) => self.set_compare_lane_message(lane_id, error),
                    }
                }
            } else {
                controls.label(egui::RichText::new("Play to load VLC").color(egui::Color32::WHITE));
            }
            if snapshot.as_ref().is_some_and(|state| state.playing)
                || controls.input(|input| input.pointer.any_down())
            {
                controls
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(33));
            }
            return;
        }

        let mut controls = ui.child_ui(controls_rect, egui::Layout::top_down(egui::Align::Min));
        controls.spacing_mut().item_spacing = egui::vec2(12.0, 8.0);
        controls.spacing_mut().interact_size.y = 40.0;
        controls.horizontal(|ui| {
            let transport = snapshot
                .as_ref()
                .map(|state| {
                    if state.playing {
                        icons::PAUSE
                    } else {
                        icons::PLAY
                    }
                })
                .unwrap_or(icons::PLAY);
            if ui
                .add_sized(
                    [64.0, 48.0],
                    egui::Button::new(egui::RichText::new(transport).size(28.0)),
                )
                .on_hover_text(if active {
                    "Play / pause"
                } else {
                    "Play here with VLC"
                })
                .clicked()
            {
                let result = if active {
                    self.video_player.toggle_pause()
                } else {
                    self.play_media_video(Path::new(path))
                };
                match result {
                    Ok(()) => self.begin_media_playback_priority(),
                    Err(error) => self.set_compare_lane_message(lane_id, error),
                }
            }
            if let Some(state) = snapshot.as_ref() {
                let mut time = state.time_ms as f64;
                let length = state.length_ms.max(1) as f64;
                // WP-070: this used plain `ui.add`, inheriting egui's fixed
                // 100-point slider width while the fullscreen scrubber scaled
                // with the panel — the reason the windowed one read as far too
                // short. Derive it from the row like fullscreen does.
                let trailing = ui.spacing().item_spacing.x * 2.0
                    + media_time_label_width(ui, state.length_ms)
                    + 12.0;
                let scrub_width = (ui.available_width() - trailing).max(120.0);
                theme::transport_slider(ui, scrub_width);
                if ui
                    .add_sized(
                        [scrub_width, 32.0],
                        egui::Slider::new(&mut time, 0.0..=length)
                            .show_value(false)
                            .clamp_to_range(true),
                    )
                    .on_hover_text("Scrub timeline")
                    .changed()
                {
                    match self.video_player.set_time(time.round() as i64) {
                        Ok(()) => self.begin_media_playback_priority(),
                        Err(error) => self.set_compare_lane_message(lane_id, error),
                    }
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_media_time(state.time_ms),
                        format_media_time(state.length_ms)
                    ))
                    .size(20.0)
                    .color(theme::ink_faint()),
                );
            } else {
                ui.label(
                    egui::RichText::new(if self.video_player_available {
                        "Play to load VLC"
                    } else {
                        "VLC not found — set FACIAL_VLC_DIR"
                    })
                    .size(20.0)
                    .color(theme::ink_faint()),
                );
            }
        });
        controls.horizontal_wrapped(|ui| {
            if let Some(state) = snapshot.as_ref() {
                let mut volume = state.volume.clamp(0, 125);
                ui.label(
                    egui::RichText::new("Vol")
                        .size(18.0)
                        .color(theme::ink_faint()),
                );
                theme::transport_slider(ui, 140.0);
                if ui
                    .add_sized(
                        [140.0, 36.0],
                        egui::Slider::new(&mut volume, 0..=125).show_value(false),
                    )
                    .changed()
                {
                    match self.video_player.set_volume(volume) {
                        Ok(()) => self.begin_media_playback_priority(),
                        Err(error) => self.set_compare_lane_message(lane_id, error),
                    }
                }

                let audio_label = state
                    .audio_tracks
                    .iter()
                    .find(|track| track.id == state.audio_track)
                    .map(|track| track.name.as_str())
                    .unwrap_or("Audio");
                egui::ComboBox::from_id_source("media_video_audio_track")
                    .selected_text(egui::RichText::new(elide_middle(audio_label, 18)).size(18.0))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for track in &state.audio_tracks {
                            if ui
                                .selectable_label(track.id == state.audio_track, &track.name)
                                .clicked()
                            {
                                match self.video_player.set_audio_track(track.id) {
                                    Ok(()) => self.begin_media_playback_priority(),
                                    Err(error) => self.set_compare_lane_message(lane_id, error),
                                }
                            }
                        }
                    });

                let subtitle_label = state
                    .subtitle_tracks
                    .iter()
                    .find(|track| track.id == state.subtitle_track)
                    .map(|track| track.name.as_str())
                    .unwrap_or("Subtitles");
                egui::ComboBox::from_id_source("media_video_subtitle_track")
                    .selected_text(egui::RichText::new(elide_middle(subtitle_label, 18)).size(18.0))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for track in &state.subtitle_tracks {
                            if ui
                                .selectable_label(track.id == state.subtitle_track, &track.name)
                                .clicked()
                            {
                                match self.video_player.set_subtitle_track(track.id) {
                                    Ok(()) => self.begin_media_playback_priority(),
                                    Err(error) => self.set_compare_lane_message(lane_id, error),
                                }
                            }
                        }
                    });
            }
            if ui
                .add_sized(
                    [150.0, 40.0],
                    egui::Button::new(egui::RichText::new("Open in VLC").size(18.0)),
                )
                .clicked()
            {
                if let Err(error) = crate::video_player::open_in_vlc(Path::new(path)) {
                    self.set_compare_lane_message(lane_id, error);
                }
            }
            if ui
                .add_sized(
                    [150.0, 40.0],
                    egui::Button::new(egui::RichText::new("Choose app…").size(18.0)),
                )
                .clicked()
            {
                self.video_player.hide();
                if let Err(error) = crate::video_player::open_with_dialog(Path::new(path)) {
                    self.set_compare_lane_message(lane_id, error);
                }
            }
        });
        let name = Path::new(path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("video");
        controls.label(
            egui::RichText::new(elide_middle(name, 58))
                .size(18.0)
                .color(theme::ink_faint()),
        );

        // Smooth transport feedback only while it matters. Playing video is
        // refreshed at ~30fps; a paused/idle player returns to the app's low
        // cadence unless the operator is actively pressing/dragging here.
        let interacting = controls.rect_contains_pointer(controls_rect)
            && controls.input(|input| input.pointer.any_down());
        if snapshot.as_ref().is_some_and(|state| state.playing) || interacting {
            controls
                .ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    /// Right-edge overlays: favorites (clickable navigation) and settings.
    fn draw_media_overlays(
        &mut self,
        ui: &mut egui::Ui,
        book: egui::Rect,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        if self.media_explorer.show_folder_navigator {
            self.draw_media_folder_navigator(ui.ctx(), lane_id, request);
        }
        if self.media_explorer.show_settings {
            self.draw_media_settings_window(ui.ctx());
        }
        if !self.media_explorer.show_favorites {
            return;
        }
        let width = 300.0_f32.min(book.width() * 0.5);
        let panel_rect =
            egui::Rect::from_min_max(egui::pos2(book.max.x - width, book.min.y), book.max);
        egui::Area::new(egui::Id::new("media_side_overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(panel_rect.min)
            .show(ui.ctx(), |ui| {
                ui.set_clip_rect(panel_rect.expand(2.0));
                theme::sheet_frame().show(ui, |ui| {
                    ui.set_min_size(panel_rect.size());
                    ui.set_max_width(panel_rect.width());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Favorites")
                                .strong()
                                .color(theme::ink()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("×").clicked() {
                                self.media_explorer.show_favorites = false;
                                self.media_explorer.show_settings = false;
                            }
                        });
                    });
                    theme::hairline(ui);
                    self.draw_media_favorites_body(ui, lane_id);
                });
            });
    }

    fn media_folder_entries(&mut self, lane_id: usize) -> Arc<PreparedFolderEntries> {
        let current = if self.media_explorer.folder_navigator_location.is_empty() {
            let Some(pos) = self.compare_lane_position(lane_id) else {
                return Arc::new(PreparedFolderEntries::default());
            };
            sanitize_folder_input(&self.compare_lanes[pos].folder)
        } else {
            sanitize_folder_input(&self.media_explorer.folder_navigator_location)
        };
        if let Some(cached) = self.media_folder_entry_cache.get(&current) {
            return Arc::clone(cached);
        }
        let child_names = self.media_child_folders(lane_id, &current);
        let prepared = Arc::new(crate::media_explorer::prepare_folder_entries(
            &current,
            child_names.as_ref(),
        ));
        self.media_folder_entry_cache
            .insert(current, Arc::clone(&prepared));
        prepared
    }

    /// True while the folder navigator is open **or** its pre-open backdrop
    /// capture is still in flight. During the capture window
    /// `show_folder_navigator` is deliberately false so the modal cannot appear
    /// inside its own blurred screenshot, but the navigator is logically active
    /// and must accept commands (WP-064).
    /// Open or focus the favourites/labels collection tab (WP-067).
    fn open_media_collection_tab(&mut self) -> Result<String, String> {
        self.snapshot_active_media_tab();
        let mut candidate = self.media_tabs.clone();
        let id = candidate.open_collection_tab()?;
        self.persist_media_tabs_state(&candidate)?;
        self.media_tabs = candidate;
        self.materialize_active_media_tab();
        Ok(id.as_str().to_string())
    }

    /// Sub-view selector for a collection tab (WP-067): favourite videos,
    /// favourite images, and the created colour labels. Label CRUD stays in
    /// Settings so there is exactly one mutation authority for the catalog.
    fn draw_media_collection_toolbar(&mut self, ui: &mut egui::Ui, lane_id: usize) {
        use crate::media_tabs::MediaCollectionView as View;
        let mut view = self.media_tabs.active().viewport.collection_view;
        let mut label_id = self.media_tabs.active().viewport.collection_label_id.clone();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            for candidate in View::all() {
                if ui
                    .selectable_label(view == candidate, candidate.label())
                    .clicked()
                    && view != candidate
                {
                    view = candidate;
                    changed = true;
                }
            }
            if view == View::Labels {
                ui.separator();
                let selected_name = self
                    .media_label_definitions
                    .iter()
                    .find(|definition| definition.id == label_id)
                    .map(|definition| definition.name.clone())
                    .unwrap_or_else(|| "Choose a label".to_string());
                egui::ComboBox::from_id_source("media_collection_label_combo")
                    .selected_text(selected_name)
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        let definitions = self.media_label_definitions.clone();
                        for definition in definitions {
                            let count = self
                                .media_label_usage_counts
                                .get(&definition.id)
                                .copied()
                                .unwrap_or(0);
                            if ui
                                .selectable_label(
                                    label_id == definition.id,
                                    format!("{}  ({count})", definition.name),
                                )
                                .clicked()
                                && label_id != definition.id
                            {
                                label_id = definition.id.clone();
                                changed = true;
                            }
                        }
                    });
                if self.media_label_definitions.is_empty() {
                    ui.label(
                        egui::RichText::new("No labels yet — create them in Settings")
                            .small()
                            .color(theme::ink_faint()),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = self
                    .compare_lane_position(lane_id)
                    .map(|pos| self.compare_lanes[pos].files.len())
                    .unwrap_or(0);
                ui.label(
                    egui::RichText::new(format!("{} items", group_thousands(count)))
                        .small()
                        .color(theme::ink_faint()),
                );
            });
        });
        if changed {
            {
                let viewport = &mut self.media_tabs.active_mut().viewport;
                viewport.collection_view = view;
                viewport.collection_label_id = label_id;
            }
            let state = self.media_tabs.clone();
            if let Err(error) = self.persist_media_tabs_state(&state) {
                self.compare_action_message = error;
            }
            self.materialize_media_collection_tab(lane_id);
        }
    }

    /// Build the rows for a collection tab straight from the in-memory metadata
    /// cache (WP-067).
    ///
    /// No filesystem scan happens here — that is the point. Favorites and label
    /// assignments are already hydrated in one batched pass, and media kind is
    /// derived from the path extension exactly as the search index does, so an
    /// images/videos split needs no extra I/O. Folder favorites are excluded:
    /// they are destinations, not media rows, and are offered separately.
    fn media_collection_rows(
        &self,
        view: crate::media_tabs::MediaCollectionView,
        label_id: &str,
    ) -> Vec<String> {
        use crate::media_tabs::MediaCollectionView as View;
        let mut rows: Vec<String> = match view {
            View::FavoriteVideos | View::FavoriteImages => {
                let want_video = view == View::FavoriteVideos;
                self.media_favorites
                    .iter()
                    .map(|(_, display)| display.clone())
                    .filter(|path| {
                        let is_video = crate::media_explorer::is_video_path(path);
                        let is_image = is_supported_image_path(Path::new(path));
                        (is_video || is_image) && is_video == want_video
                    })
                    .collect()
            }
            View::Labels => {
                if label_id.trim().is_empty() {
                    Vec::new()
                } else {
                    self.media_color_labels
                        .iter()
                        .filter(|(_, labels)| labels.iter().any(|id| id == label_id))
                        .map(|(key, _)| self.media_db.path_for_key(key))
                        .filter(|path| {
                            crate::media_explorer::is_video_path(path)
                                || is_supported_image_path(Path::new(path))
                        })
                        .collect()
                }
            }
        };
        rows.sort_by_key(|path| path.to_lowercase());
        rows.dedup();
        // Honour the tab's Name direction. Stat-dependent keys are refused for
        // collection tabs, so name order is the only meaningful one here.
        if self.media_explorer.sort_desc {
            rows.reverse();
        }
        rows
    }

    /// Publish a collection tab's rows into the shared lane without scanning.
    fn materialize_media_collection_tab(&mut self, lane_id: usize) {
        let viewport = self.media_tabs.active().viewport.clone();
        let rows = self.media_collection_rows(
            viewport.collection_view,
            &viewport.collection_label_id,
        );
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let count = rows.len();
        let lane = &mut self.compare_lanes[pos];
        lane.folder = String::new();
        lane.name = "Favorites".to_string();
        lane.files = Arc::new(rows);
        lane.inventory_generation = None;
        lane.scanning = false;
        lane.scan_error.clear();
        lane.index = 0;
        lane.image_path.clear();
        lane.selected_files.clear();
        lane.selection_anchor = None;
        lane.texture = None;
        lane.texture_size = None;
        // Rows are already in their final order, so publish a display order in
        // this same frame rather than waiting on the display worker.
        self.media_display_cache = Arc::new((0..count).collect());
        self.media_display_cache_key = None;
        self.media_content_generation = self.media_content_generation.wrapping_add(1);
        self.media_scan_diagnostics = MediaScanDiagnostics {
            status: "collection".to_string(),
            final_items: count,
            ..MediaScanDiagnostics::default()
        };
        if count > 0 {
            self.start_compare_image_load(lane_id);
        }
    }

    /// Select the given paths in a lane and reveal the first match. Shared by
    /// the `media_select` intent and search-result activation (WP-066).
    fn media_select_paths(&mut self, lane_id: usize, paths: &[String]) -> usize {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return 0;
        };
        // Separator + casing insensitive matching: models naturally send
        // forward-slash paths while scans produce native ones.
        let normalize = |value: &str| {
            sanitize_folder_input(value)
                .replace('\\', "/")
                .to_lowercase()
        };
        let wanted: HashSet<String> = paths.iter().map(|path| normalize(path)).collect();
        let (matched, first_match) = {
            let lane = &mut self.compare_lanes[pos];
            let mut matched = 0usize;
            let mut first_match = None;
            lane.selected_files.clear();
            for (index, file) in lane.files.iter().enumerate() {
                if wanted.contains(&normalize(file)) {
                    lane.selected_files.insert(index);
                    first_match.get_or_insert(index);
                    matched += 1;
                }
            }
            if let Some(index) = first_match {
                lane.index = index;
                lane.image_path = lane.files[index].clone();
                lane.selection_anchor = Some(index);
            }
            (matched, first_match)
        };
        if let Some(index) = first_match {
            let selected_path = self.compare_lanes[pos].files[index].clone();
            self.media_explorer.cursor = self
                .media_display_cache
                .iter()
                .position(|file_index| *file_index == index);
            self.set_pending_inline_video_target(&selected_path);
            self.media_scroll_to_cursor = true;
            self.request_compare_image(lane_id, index);
        }
        matched
    }

    /// Open an activated search result (WP-066).
    ///
    /// Resolution order matters. A stored `source_index` is only meaningful
    /// inside the generation that produced it, so it is used only when that
    /// generation still matches. Otherwise the exact path is relocated in the
    /// current inventory, and if it is gone the operator is told so rather than
    /// having a neighbouring file opened silently.
    fn media_activate_search_result(
        &mut self,
        lane_id: usize,
        file: &crate::media_search::FileSuggestion,
        new_tab: bool,
        request: &mut CompareLaneRenderRequest,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let generation_matches = file.generation
            == crate::media_search::SearchIndexGeneration(self.media_content_generation);
        let resolved = if generation_matches && !file.path.is_empty() {
            self.compare_lanes[pos]
                .files
                .get(file.source_index)
                .filter(|candidate| candidate.as_str() == file.path)
                .cloned()
        } else {
            None
        };
        let resolved = resolved.or_else(|| {
            let wanted_path = (!file.path.is_empty()).then(|| file.path.to_lowercase());
            let wanted_name = file.name.to_lowercase();
            self.compare_lanes[pos]
                .files
                .iter()
                .find(|candidate| match wanted_path.as_deref() {
                    Some(path) => candidate.to_lowercase() == path,
                    None => Path::new(candidate.as_str())
                        .file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|name| name.to_lowercase() == wanted_name),
                })
                .cloned()
        });
        let Some(path) = resolved else {
            self.compare_action_message =
                format!("'{}' is no longer in this folder", file.name);
            return;
        };
        if new_tab {
            let parent = Path::new(&path)
                .parent()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            if parent.is_empty() {
                self.compare_action_message =
                    format!("Cannot resolve a folder for '{}'", file.name);
                return;
            }
            request.open_folder_in_new_tab = Some(parent);
            self.media_pending_result_selection = Some(path);
            return;
        }
        self.media_select_paths(lane_id, std::slice::from_ref(&path));
        self.compare_action_message = format!("Opened {}", file.name);
    }

    fn media_folder_navigator_active(&self) -> bool {
        folder_navigator_is_active(
            self.media_explorer.show_folder_navigator,
            self.folder_navigator_backdrop_requested_at.is_some(),
        )
    }

    /// Resolve an in-flight backdrop capture immediately, opening the navigator
    /// over the shared neutral fallback. Used when an action arrives while the
    /// capture is pending so behavior never depends on capture latency.
    fn settle_media_folder_navigator_capture(&mut self, lane_id: usize) {
        if self.folder_navigator_backdrop_requested_at.is_none() {
            return;
        }
        self.folder_navigator_backdrop_requested_at = None;
        self.open_media_folder_navigator_without_capture(lane_id);
    }

    fn request_media_folder_navigator(&mut self, ctx: &egui::Context, lane_id: usize) {
        self.media_explorer.show_settings = false;
        self.media_explorer.show_favorites = false;
        self.settings_backdrop = None;
        self.settings_backdrop_requested_at = None;
        let active = self
            .compare_lane_position(lane_id)
            .map(|pos| sanitize_folder_input(&self.compare_lanes[pos].folder))
            .unwrap_or_default();
        self.media_explorer.folder_navigator_location = active.clone();
        self.media_explorer.folder_location_input = active;
        self.media_explorer.show_folder_navigator = false;
        self.folder_navigator_backdrop = None;
        if self.pending_model_snapshot.is_some() {
            // Screenshot replies carry no request ID. Never overlap the folder
            // backdrop request with a receipt-backed model capture; open over
            // the neutral fallback exactly as Settings already does (WP-064).
            self.folder_navigator_backdrop_requested_at = None;
            self.open_media_folder_navigator_without_capture(lane_id);
            ctx.request_repaint();
            return;
        }
        self.folder_navigator_backdrop_requested_at = Some(std::time::Instant::now());
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        ctx.request_repaint();
    }

    fn open_media_folder_navigator_without_capture(&mut self, lane_id: usize) {
        self.media_explorer.show_settings = false;
        self.media_explorer.show_favorites = false;
        let active = self
            .compare_lane_position(lane_id)
            .map(|pos| sanitize_folder_input(&self.compare_lanes[pos].folder))
            .unwrap_or_default();
        self.media_explorer.folder_navigator_location = active.clone();
        self.media_explorer.folder_location_input = active;
        self.media_explorer.show_folder_navigator = true;
        let entries = self.media_folder_entries(lane_id);
        self.media_explorer.folder_cursor = entries
            .iter()
            .position(|entry| !entry.is_drive)
            .or_else(|| (!entries.is_empty()).then_some(0));
        self.media_explorer.folder_scroll_to_cursor = true;
    }

    fn close_media_folder_navigator(&mut self) {
        self.media_explorer.show_folder_navigator = false;
        self.folder_navigator_backdrop = None;
        self.folder_navigator_backdrop_requested_at = None;
    }

    fn media_toggle_folder_navigator(&mut self, ctx: &egui::Context, lane_id: usize) {
        if self.media_explorer.show_folder_navigator
            || self.folder_navigator_backdrop_requested_at.is_some()
        {
            self.close_media_folder_navigator();
        } else {
            self.request_media_folder_navigator(ctx, lane_id);
        }
    }

    fn media_navigator_move(&mut self, lane_id: usize, delta: isize) {
        let entries = self.media_folder_entries(lane_id);
        let drive_count = entries.iter().take_while(|entry| entry.is_drive).count();
        let current = self.media_explorer.folder_cursor;
        self.media_explorer.folder_cursor = match (current, delta) {
            (Some(index), step)
                if index < drive_count && step > 0 && entries.len() > drive_count =>
            {
                Some(drive_count)
            }
            (Some(index), step) if index == drive_count && step < 0 && drive_count > 0 => {
                let current_folder =
                    sanitize_folder_input(&self.media_explorer.folder_navigator_location);
                entries[..drive_count]
                    .iter()
                    .position(|entry| {
                        crate::media_explorer::path_is_on_root(&current_folder, &entry.path)
                    })
                    .or(Some(0))
            }
            _ => crate::media_explorer::move_list_cursor(current, delta, entries.len()),
        };
        self.media_explorer.folder_scroll_to_cursor = true;
    }

    fn media_navigator_move_drive(&mut self, lane_id: usize, delta: isize) -> bool {
        let entries = self.media_folder_entries(lane_id);
        let drive_count = entries.iter().take_while(|entry| entry.is_drive).count();
        let Some(current) = self.media_explorer.folder_cursor else {
            return false;
        };
        if current >= drive_count || drive_count == 0 {
            return false;
        }
        self.media_explorer.folder_cursor = Some(
            (current as isize + delta).clamp(0, drive_count.saturating_sub(1) as isize) as usize,
        );
        self.media_explorer.folder_scroll_to_cursor = true;
        true
    }

    fn media_navigator_navigate_to(
        &mut self,
        lane_id: usize,
        target: &str,
        restore_child: Option<&str>,
    ) {
        if self.compare_lane_position(lane_id).is_none() {
            return;
        }
        self.media_explorer.folder_navigator_location = target.to_string();
        self.media_explorer.folder_location_input = target.to_string();
        self.media_child_folder_cache.remove(target);
        self.media_folder_entry_cache.remove(target);
        let entries = self.media_folder_entries(lane_id);
        self.media_explorer.folder_cursor = restore_child
            .and_then(|child| {
                entries
                    .iter()
                    .position(|entry| entry.path.eq_ignore_ascii_case(child))
            })
            .or_else(|| {
                entries
                    .iter()
                    .position(|entry| !entry.is_parent && !entry.is_drive)
            })
            .or_else(|| (!entries.is_empty()).then_some(0));
        self.media_explorer.folder_scroll_to_cursor = true;
        self.compare_action_message = format!("Browsing folder: {target} (not opened)");
    }

    fn media_navigator_enter(&mut self, lane_id: usize) {
        let entries = self.media_folder_entries(lane_id);
        let Some(entry) = self
            .media_explorer
            .folder_cursor
            .and_then(|index| entries.get(index))
            .cloned()
        else {
            return;
        };
        let old = sanitize_folder_input(&self.media_explorer.folder_navigator_location);
        self.media_navigator_navigate_to(
            lane_id,
            &entry.path,
            entry.is_parent.then_some(old.as_str()),
        );
    }

    fn media_navigator_parent_or_close(&mut self, lane_id: usize) {
        if self.compare_lane_position(lane_id).is_none() {
            return;
        }
        let current = sanitize_folder_input(&self.media_explorer.folder_navigator_location);
        let Some(parent) = Path::new(&current)
            .parent()
            .and_then(|path| path.to_str())
            .map(String::from)
        else {
            self.media_explorer.folder_cursor =
                self.media_folder_entries(lane_id).iter().position(|entry| {
                    entry.is_drive && crate::media_explorer::path_is_on_root(&current, &entry.path)
                });
            self.media_explorer.folder_scroll_to_cursor = true;
            return;
        };
        self.media_navigator_navigate_to(lane_id, &parent, Some(&current));
    }

    fn media_navigator_commit_current(
        &mut self,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) -> bool {
        let target = sanitize_folder_input(&self.media_explorer.folder_navigator_location);
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return false;
        };
        if target.is_empty() {
            self.compare_action_message = "No folder selected to open".to_string();
            return false;
        }
        self.compare_lanes[pos].folder = target.clone();
        self.media_explorer.cursor = None;
        request.scan = true;
        self.compare_action_message = format!("Opened folder: {target}");
        self.close_media_folder_navigator();
        true
    }

    /// Large 10-foot folder surface (WP-051). The desktop strip remains in
    /// place underneath; this window is an in-app controller focus group.
    fn draw_media_folder_navigator(
        &mut self,
        ctx: &egui::Context,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        const ROW_STRIDE: f32 = 120.0;
        const ROW_HEIGHT: f32 = 112.0;
        const FOLDER_FONT: f32 = 52.0;

        let screen = ctx.screen_rect();
        let max_w = (screen.width() - 32.0).max(520.0);
        let max_h = (screen.height() - 32.0).max(420.0);
        let couch_size = egui::vec2(1800.0_f32.min(max_w), 1360.0_f32.min(max_h));
        let current = sanitize_folder_input(&self.media_explorer.folder_navigator_location);
        let entries = self.media_folder_entries(lane_id);
        if self
            .media_explorer
            .folder_cursor
            .is_some_and(|index| index >= entries.len())
        {
            self.media_explorer.folder_cursor = entries.len().checked_sub(1);
        }
        let mut open = self.media_explorer.show_folder_navigator;
        let mut activate: Option<usize> = None;
        let mut clicked_cursor: Option<usize> = None;
        let mut reveal_consumed = false;
        let mut direct_location: Option<String> = None;
        let mut commit_current = false;
        let mut open_in_new_tab = false;
        let mut footer_close = false;

        // Folder navigation and Settings share the same pre-open Gaussian
        // capture pipeline and dismissible modal behavior. This preserves the
        // Media context without the old inconsistent flat veil.
        let backdrop_clicked = draw_soft_modal_backdrop(
            ctx,
            "media_couch_folder_backdrop",
            true,
            self.folder_navigator_backdrop.as_ref(),
        );

        egui::Window::new("Folders")
            .id(egui::Id::new("media_couch_folder_navigator"))
            .open(&mut open)
            .title_bar(false)
            .collapsible(false)
            .fixed_size(couch_size)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .frame(theme::sheet_frame())
            .show(ctx, |ui| {
                ui.heading(
                    egui::RichText::new("Folders")
                        .size(48.0)
                        .color(theme::ink()),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&current)
                            .size(36.0)
                            .color(theme::ink_soft()),
                    )
                    .truncate(true),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let go_width = 104.0 + ui.spacing().item_spacing.x;
                    let response = ui.add_sized(
                        [(ui.available_width() - go_width).max(180.0), 44.0],
                        TextEdit::singleline(&mut self.media_explorer.folder_location_input)
                            .hint_text("Local, mapped-drive, or NAS UNC location (\\\\server\\share)"),
                    );
                    let go = ui
                        .add_sized([96.0, 44.0], egui::Button::new("Go"))
                        .on_hover_text("Open this location inside Facial; no external Explorer window")
                        .clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                    if go {
                        direct_location = Some(self.media_explorer.folder_location_input.clone());
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Mapped network drives appear above. Paste a UNC share here when it has no drive letter.",
                    )
                    .size(20.0)
                    .color(theme::ink_faint()),
                );
                ui.add_space(12.0);
                theme::hairline(ui);
                ui.add_space(12.0);
                let drive_count = entries.iter().take_while(|entry| entry.is_drive).count();
                if drive_count > 0 {
                    ScrollArea::horizontal()
                        .id_source("media_couch_drive_roots")
                        .max_height(88.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (index, entry) in entries[..drive_count].iter().enumerate() {
                                    let selected = self.media_explorer.folder_cursor == Some(index);
                                    let response = ui.add_sized(
                                        [160.0, 76.0],
                                        egui::Button::new(
                                            egui::RichText::new(format!(
                                                "{} {}",
                                                icons::HARD_DRIVE,
                                                entry.path.trim_end_matches(['\\', '/'])
                                            ))
                                            .size(36.0),
                                        )
                                        .fill(if selected {
                                            theme::selection_bg()
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        })
                                        .stroke(egui::Stroke::NONE),
                                    );
                                    if selected && self.media_explorer.folder_scroll_to_cursor {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                        reveal_consumed = true;
                                    }
                                    if response.clicked() {
                                        clicked_cursor = Some(index);
                                        activate = Some(index);
                                    }
                                }
                            });
                        });
                    ui.add_space(8.0);
                    theme::hairline(ui);
                    ui.add_space(8.0);
                }
                let folder_entries = &entries[drive_count..];
                // Use a whole-row viewport and reserve the full couch footer.
                // This prevents partial row text from touching the controls.
                let list_budget = (ui.available_height() - 148.0).max(ROW_STRIDE);
                let available_h = (list_budget / ROW_STRIDE).floor() * ROW_STRIDE;
                if folder_entries.is_empty() {
                    ScrollArea::vertical()
                        .id_source("media_couch_folder_list")
                        .max_height(available_h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add_space(24.0);
                            ui.label(
                                egui::RichText::new("No folders here")
                                    .size(FOLDER_FONT)
                                    .color(theme::ink_soft()),
                            );
                        });
                } else {
                    // `show_rows` keeps a folder containing thousands of
                    // immediate children as cheap as the visible couch rows.
                    let requested_offset = self
                        .media_explorer
                        .folder_scroll_to_cursor
                        .then_some(self.media_explorer.folder_cursor)
                        .flatten()
                        .filter(|index| *index >= drive_count)
                        .map(|index| {
                            let index = index - drive_count;
                            let centered = (index as f32 * ROW_STRIDE - available_h * 0.5
                                + ROW_STRIDE * 0.5)
                                .max(0.0);
                            // Keep the viewport on whole-row boundaries so a
                            // controller reveal cannot straddle the footer.
                            (centered / ROW_STRIDE).floor() * ROW_STRIDE
                        });
                    let mut folder_scroll = ScrollArea::vertical()
                        .id_source("media_couch_folder_list")
                        .max_height(available_h)
                        .auto_shrink([false, false]);
                    if let Some(offset) = requested_offset {
                        // Virtual rows do not exist until they enter the
                        // viewport, so position it directly from the cursor.
                        folder_scroll = folder_scroll.vertical_scroll_offset(offset);
                    }
                    folder_scroll.show_rows(
                        ui,
                        ROW_STRIDE,
                        folder_entries.len(),
                        |ui, visible_rows| {
                            for relative_index in visible_rows {
                                let index = drive_count + relative_index;
                                let entry = &entries[index];
                                let selected = self.media_explorer.folder_cursor == Some(index);
                                let text = if entry.is_parent {
                                    format!("{}  {}", icons::ARROW_ELBOW_LEFT_UP, entry.label)
                                } else {
                                    format!("{}  {}", icons::FOLDER, entry.label)
                                };
                                let button = egui::Button::new(
                                    egui::RichText::new(text)
                                        .size(FOLDER_FONT)
                                        .color(theme::ink()),
                                )
                                .fill(if selected {
                                    theme::selection_bg()
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .stroke(egui::Stroke::NONE)
                                .rounding(theme::rounding());
                                let response =
                                    ui.add_sized([ui.available_width(), ROW_HEIGHT], button);
                                if selected {
                                    let marker = egui::Rect::from_min_max(
                                        response.rect.min,
                                        egui::pos2(response.rect.min.x + 12.0, response.rect.max.y),
                                    );
                                    ui.painter().rect_filled(marker, 0.0, theme::accent());
                                    if self.media_explorer.folder_scroll_to_cursor {
                                        reveal_consumed = true;
                                    }
                                }
                                if response.clicked() {
                                    clicked_cursor = Some(index);
                                    activate = Some(index);
                                }
                            }
                        },
                    );
                }
                // Pin the footer to the bottom of the fixed surface even when
                // the whole-row viewport leaves a small amount of slack.
                let footer_gap = (ui.available_height() - 144.0).max(12.0);
                ui.add_space(footer_gap);
                theme::hairline(ui);
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(
                            "D-pad Navigate   A Browse   B Parent / Drives   Select Close",
                        )
                        .size(24.0)
                        .color(theme::ink_soft()),
                    )
                    .truncate(true),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_sized(
                                [220.0, 84.0],
                                egui::Button::new(egui::RichText::new("Close").size(36.0)),
                            )
                            .clicked()
                        {
                            footer_close = true;
                        }
                        if ui
                            .add_sized(
                                [300.0, 84.0],
                                egui::Button::new(
                                    egui::RichText::new("Open in new tab").size(32.0),
                                ),
                            )
                            .on_hover_text(
                                "Open this staged folder in a separate Media tab",
                            )
                            .clicked()
                        {
                            open_in_new_tab = true;
                        }
                        if ui
                            .add_sized(
                                [260.0, 84.0],
                                egui::Button::new(
                                    egui::RichText::new("Open folder").size(34.0),
                                ),
                            )
                            .on_hover_text(
                                "Commit this staged folder to the current Media tab",
                            )
                            .clicked()
                        {
                            commit_current = true;
                        }
                    });
                });
            });
        if reveal_consumed {
            self.media_explorer.folder_scroll_to_cursor = false;
        }
        if let Some(index) = clicked_cursor {
            self.media_explorer.folder_cursor = Some(index);
        }
        if let Some(index) = activate {
            self.media_explorer.folder_cursor = Some(index);
            self.media_navigator_enter(lane_id);
        }
        if let Some(location) = direct_location {
            let location = sanitize_folder_input(&location);
            if location.is_empty() {
                self.compare_action_message =
                    "Enter a local, mapped-drive, or UNC folder location".to_string();
            } else {
                self.media_navigator_navigate_to(lane_id, &location, None);
            }
        }
        if commit_current {
            self.media_navigator_commit_current(lane_id, request);
        } else if open_in_new_tab {
            let target = sanitize_folder_input(&self.media_explorer.folder_navigator_location);
            if target.is_empty() {
                self.compare_action_message = "No folder selected to open in a new tab".to_string();
            } else {
                request.open_folder_in_new_tab = Some(target.clone());
                self.compare_action_message = format!("Opening folder in new tab: {target}");
            }
        } else if backdrop_clicked || footer_close || !open {
            self.close_media_folder_navigator();
        }
    }

    /// Large, readable, resizable in-app settings popup (WP-050/WP-055). This is an
    /// egui window inside Facial, so it remains model-safe and never launches
    /// or focuses an external OS window.
    fn draw_media_settings_window(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let backdrop_clicked = draw_soft_modal_backdrop(
            ctx,
            "media_settings_backdrop",
            true,
            self.settings_backdrop.as_ref(),
        );
        // Clamp both the preferred and minimum size to the *actual* viewport.
        // Never use `.max(minimum)` here: on a small display that would create
        // an off-screen window before egui has a chance to constrain it.
        let couch = self.media_explorer.settings_couch_fullscreen;
        let (available, default_size, min_size) = media_settings_sizes(screen.size());
        let mut open = self.media_explorer.show_settings;
        let mut footer_close = false;
        let mut toggle_couch = false;
        // A separate identity is essential: egui remembers window geometry by
        // ID, and a viewport-sized couch surface must never contaminate the
        // compact normal Settings bounds when it closes (WP-062).
        let settings_id = egui::Id::new(if couch {
            "media_settings_window_couch"
        } else {
            "media_settings_window"
        });
        let settings_layer = egui::LayerId::new(egui::Order::Middle, settings_id);
        ctx.move_to_top(settings_layer);
        let mut window = egui::Window::new("Media settings")
            .id(settings_id)
            .open(&mut open)
            .collapsible(false)
            // The window itself never scrolls or sizes itself from category
            // content. One explicit child viewport below owns scrolling.
            .scroll2([false, false])
            .constrain_to(screen.shrink(12.0))
            .frame(theme::sheet_frame());
        if couch {
            let couch_rect = screen.shrink(12.0);
            window = window
                .title_bar(false)
                .resizable(false)
                .fixed_pos(couch_rect.min)
                .fixed_size(couch_rect.size());
        } else {
            window = window
                .resizable(true)
                .default_size(default_size)
                .min_size(min_size)
                .max_size(available)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
        }
        window.show(ctx, |ui| {
            let result = self.draw_media_settings_body(ui, couch);
            footer_close = result.0;
            toggle_couch = result.1;
        });
        // The full-screen backdrop and egui windows share Order::Middle.
        // Raising the exact window layer keeps the veil from winning
        // hit-testing, which made the old Settings surface non-interactable.
        ctx.move_to_top(settings_layer);
        if toggle_couch {
            if couch {
                self.exit_settings_couch_fullscreen(ctx);
            } else {
                self.enter_settings_couch_fullscreen(ctx);
            }
        } else if backdrop_clicked || footer_close || !open {
            self.close_media_settings(ctx);
        } else {
            self.media_explorer.show_settings = true;
        }
    }

    fn draw_media_favorites_body(&mut self, ui: &mut egui::Ui, lane_id: usize) {
        let current_folder = self
            .compare_lane_position(lane_id)
            .map(|pos| sanitize_folder_input(&self.compare_lanes[pos].folder))
            .unwrap_or_default();
        // The selected lane/scan is the cached validity source. Never touch a
        // NAS path merely to decide whether this button is enabled.
        let has_current = !current_folder.is_empty();
        let already = has_current
            && self
                .media_favorite_keys
                .contains(&self.media_db.key_for(&current_folder));
        if ui
            .add_enabled(
                has_current && !already,
                egui::Button::new(format!("{} Pin current folder", icons::PUSH_PIN)),
            )
            .clicked()
        {
            self.media_toggle_favorite(&current_folder.clone());
        }
        ui.add_space(4.0);
        let favorites = self.media_favorites.clone();
        if favorites.is_empty() {
            ui.label(
                egui::RichText::new("No favorites yet. Pin folders here for one-click jumps.")
                    .small()
                    .color(theme::ink_faint()),
            );
            return;
        }
        let mut navigate: Option<String> = None;
        let mut remove: Option<String> = None;
        ScrollArea::vertical()
            .id_source("media_favorites_list")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (key, display) in &favorites {
                    // Navigation always resolves through the key so relative
                    // favorites keep working after a workspace move; the
                    // display path only provides the pretty name.
                    let resolved = self.media_db.path_for_key(key);
                    let name = Path::new(display)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(display.as_str());
                    ui.horizontal(|ui| {
                        // Favorites store paths, not file-kind metadata. Use
                        // supported-media extensions for the cosmetic icon;
                        // exact validation happens only after an operator
                        // activates the row, never during every render frame.
                        let is_media = is_supported_image_path(Path::new(&resolved))
                            || crate::media_explorer::is_video_path(&resolved);
                        let icon = if is_media {
                            icons::IMAGE
                        } else {
                            icons::FOLDER
                        };
                        let row = ui
                            .selectable_label(false, format!("{icon} {name}"))
                            .on_hover_text(&resolved);
                        if row.clicked() {
                            navigate = Some(resolved.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("×")
                                .on_hover_text("Remove favorite")
                                .clicked()
                            {
                                remove = Some(resolved.clone());
                            }
                        });
                    });
                }
            });
        if let Some(target) = navigate {
            let target_is_media = is_supported_image_path(Path::new(&target))
                || crate::media_explorer::is_video_path(&target);
            let folder = if target_is_media {
                Path::new(&target)
                    .parent()
                    .and_then(|p| p.to_str())
                    .map(String::from)
            } else {
                Some(target.clone())
            };
            if let Some(folder) = folder {
                if let Some(pos) = self.compare_lane_position(lane_id) {
                    self.compare_lanes[pos].folder = folder;
                    if target_is_media {
                        self.compare_lanes[pos].pending_jump = target;
                    }
                    self.media_explorer.cursor = None;
                    self.start_compare_scan(lane_id);
                }
            }
        }
        if let Some(target) = remove {
            self.media_toggle_favorite(&target);
        }
    }

    /// Unified settings entrypoint. The old separate Options tab is now an App
    /// category here, adjacent to the refresh control in the header.
    fn draw_media_settings_body(&mut self, ui: &mut egui::Ui, couch: bool) -> (bool, bool) {
        const CATEGORIES: [&str; 4] = ["Media", "Playback", "Controls", "App"];
        self.media_explorer.settings_category = self.media_explorer.settings_category.min(3);

        if couch {
            apply_settings_couch_style(ui);
        }

        // Reserve the exact current Resize rectangle in the parent, then draw
        // header/content/footer into child UIs. Child content can no longer
        // increase the window's remembered desired size on later frames.
        let shell_rect = ui.max_rect();
        ui.allocate_rect(shell_rect, egui::Sense::hover());
        let footer_h = (egui::TextStyle::Button.resolve(ui.style()).size + 34.0).max(58.0);
        let footer_top = (shell_rect.max.y - footer_h).max(shell_rect.min.y);

        let mut header_ui = ui.child_ui(
            egui::Rect::from_min_max(shell_rect.min, egui::pos2(shell_rect.max.x, footer_top)),
            egui::Layout::top_down(egui::Align::Min),
        );
        if couch {
            header_ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Media settings")
                        .heading()
                        .strong()
                        .color(theme::ink()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("COUCH FULLSCREEN")
                            .strong()
                            .color(theme::ink_faint()),
                    );
                });
            });
        }
        let mut toggle_couch = false;
        header_ui.horizontal_wrapped(|ui| {
            for (index, label) in CATEGORIES.iter().enumerate() {
                if ui
                    .selectable_label(self.media_explorer.settings_category == index as u8, *label)
                    .clicked()
                {
                    self.media_explorer.settings_category = index as u8;
                }
            }
        });
        // Keep the mode control in its own height-bounded row. A bare
        // `with_layout` inside this top-down child consumes the remaining
        // vertical extent and can push the scroll content below the footer.
        header_ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if couch {
                    "Windowed settings".to_string()
                } else {
                    "Couch fullscreen".to_string()
                };
                toggle_couch = ui.button(label).clicked();
            });
        });
        theme::hairline(&mut header_ui);
        let content_top = (header_ui.min_rect().max.y + 8.0).min(footer_top);
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(shell_rect.min.x, content_top),
            egui::pos2(shell_rect.max.x, footer_top),
        );
        let mut content_ui = ui.child_ui(content_rect, egui::Layout::top_down(egui::Align::Min));
        content_ui.set_clip_rect(content_rect);
        ScrollArea::vertical()
            .id_source((
                "media_settings_content",
                self.media_explorer.settings_category,
            ))
            .auto_shrink([false, false])
            .show(&mut content_ui, |ui| {
                match self.media_explorer.settings_category {
                    0 => self.draw_media_settings_media(ui),
                    1 => self.draw_media_settings_playback(ui),
                    2 => self.draw_media_settings_controls(ui),
                    _ => self.draw_options_tab(ui),
                }
            });

        let footer_rect =
            egui::Rect::from_min_max(egui::pos2(shell_rect.min.x, footer_top), shell_rect.max);
        let mut footer_ui = ui.child_ui(footer_rect, egui::Layout::top_down(egui::Align::Min));
        footer_ui.set_clip_rect(footer_rect);
        theme::hairline(&mut footer_ui);
        footer_ui.add_space(6.0);
        let mut close = false;
        footer_ui.horizontal(|ui| {
            let status = if self
                .compare_action_message
                .starts_with("media metadata not saved:")
            {
                "Save failed — retrying"
            } else if self.media_explorer.settings_dirty {
                "Saving…"
            } else if self.media_explorer.settings_category == 3 && self.app_path_draft_staged() {
                "Path draft staged — use Set to apply"
            } else {
                "Saved"
            };
            ui.label(
                egui::RichText::new(status)
                    .small()
                    .color(theme::ink_faint()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui
                    .add_sized(
                        [104.0, ui.spacing().interact_size.y],
                        egui::Button::new("Close"),
                    )
                    .clicked();
            });
        });
        (close, toggle_couch)
    }

    fn draw_media_settings_playback(&mut self, ui: &mut egui::Ui) {
        theme::kicker(ui, "Playback");
        let mut loop_enabled = self.media_explorer.video_loop;
        if ui
            .checkbox(&mut loop_enabled, "Loop videos by default")
            .on_hover_text("Repeat the selected video continuously in Facial's embedded player")
            .changed()
        {
            self.media_explorer.video_loop = loop_enabled;
            match self.video_player.set_loop(loop_enabled) {
                Ok(()) => {
                    self.compare_action_message = if loop_enabled {
                        "Video looping enabled".to_string()
                    } else {
                        "Video looping disabled".to_string()
                    }
                }
                Err(error) => self.compare_action_message = error,
            }
            self.touch_media_settings();
        }
        for line in [
            "A / Enter: play or pause the selected video.",
            "Right stick: left/right seek 10s · up/down volume.",
        ] {
            ui.label(
                egui::RichText::new(line)
                    .size(16.0)
                    .color(theme::ink_faint()),
            );
        }
        ui.add_space(10.0);
        theme::kicker(ui, "Embedded player");
        for line in [
            "VLC loads only after Play. Open in VLC / Choose app are explicit.",
            "Scanning and inspection never start playback.",
        ] {
            ui.label(
                egui::RichText::new(line)
                    .size(16.0)
                    .color(theme::ink_soft()),
            );
        }
        if let Some(snapshot) = self.video_player.snapshot() {
            ui.label(format!(
                "Active: {} · {} · loop {}",
                Path::new(&snapshot.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("video"),
                if snapshot.playing {
                    "playing"
                } else {
                    "paused"
                },
                if snapshot.looping { "on" } else { "off" }
            ));
        }
    }

    fn draw_media_settings_media(&mut self, ui: &mut egui::Ui) {
        theme::kicker(ui, "Layout");
        let mut split = self.media_explorer.split_ratio;
        if ui
            .add(
                egui::Slider::new(
                    &mut split,
                    crate::media_explorer::SPLIT_MIN..=crate::media_explorer::SPLIT_MAX,
                )
                .text("Library / Viewer split"),
            )
            .changed()
        {
            self.media_explorer.split_ratio = split;
            self.touch_media_settings();
        }
        let mut edge = self.media_explorer.tile_edge;
        if ui
            .add(
                egui::Slider::new(
                    &mut edge,
                    crate::media_explorer::TILE_MIN..=crate::media_explorer::TILE_MAX,
                )
                .text("Thumbnail size"),
            )
            .changed()
        {
            self.media_explorer.tile_edge = edge;
            self.touch_media_settings();
        }
        let mut show_names = self.media_explorer.show_names;
        if ui
            .checkbox(&mut show_names, "Show filenames below thumbnails")
            .changed()
        {
            self.media_explorer.show_names = show_names;
            self.touch_media_settings();
        }
        let mut strip = self.media_explorer.strip_height;
        if ui
            .add(
                egui::Slider::new(
                    &mut strip,
                    crate::media_explorer::STRIP_MIN..=crate::media_explorer::STRIP_MAX,
                )
                .text("Folder list height"),
            )
            .changed()
        {
            self.media_explorer.strip_height = strip;
            self.touch_media_settings();
        }
        ui.add_space(6.0);
        theme::kicker(ui, "Thumbnail cache");
        if let Some(engine) = self.thumb_engine.as_ref() {
            let (decodes, disk_hits, failures, stale) = engine.stats();
            ui.label(
                egui::RichText::new(format!(
                    "decodes {decodes} · disk hits {disk_hits} · failures {failures} · skips {stale}"
                ))
                .small()
                .color(theme::ink_faint()),
            );
        }
        ui.add_space(10.0);
        theme::kicker(ui, "Label manager");
        ui.label(
            egui::RichText::new(
                "Create, rename, recolor, or remove labels. Names and hex colors must be unique. File assignments use stable IDs, so rename and recolor are safe.",
            )
            .small()
            .color(theme::ink_faint()),
        );
        let label_manager_large_rows = ui.style().spacing.interact_size.y >= 44.0;
        let mut create_requested = false;
        let create_hex = format!(
            "#{:02X}{:02X}{:02X}",
            self.media_label_create_rgb[0],
            self.media_label_create_rgb[1],
            self.media_label_create_rgb[2]
        );
        if label_manager_large_rows {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                let name_width = (ui.available_width() - 90.0).clamp(260.0, 520.0);
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgb(&mut self.media_label_create_rgb)
                        .on_hover_text("Choose a unique label color");
                    ui.add(
                        TextEdit::singleline(&mut self.media_label_create_name)
                            .desired_width(name_width)
                            .hint_text("New label name"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Hex").strong());
                    ui.label(egui::RichText::new(&create_hex).monospace().strong());
                    create_requested = ui
                        .add_enabled(
                            self.media_db.is_writable()
                                && !self.media_label_create_name.trim().is_empty(),
                            egui::Button::new("Create label"),
                        )
                        .clicked();
                });
            });
        } else {
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut self.media_label_create_rgb)
                    .on_hover_text("Choose a unique label color");
                ui.add(
                    TextEdit::singleline(&mut self.media_label_create_name)
                        .desired_width(210.0)
                        .hint_text("New label name"),
                );
                ui.label(egui::RichText::new(&create_hex).monospace().small());
                create_requested = ui
                    .add_enabled(
                        self.media_db.is_writable()
                            && !self.media_label_create_name.trim().is_empty(),
                        egui::Button::new("Create label"),
                    )
                    .clicked();
            });
        }
        if create_requested {
            let hex = format!(
                "#{:02X}{:02X}{:02X}",
                self.media_label_create_rgb[0],
                self.media_label_create_rgb[1],
                self.media_label_create_rgb[2]
            );
            match self
                .media_db
                .create_color_label(&self.media_label_create_name, &hex)
            {
                Ok(_) => {
                    self.media_label_definitions = self.media_db.color_label_definitions();
                    self.refresh_media_label_colors();
                    self.media_label_usage_counts = self.media_db.color_label_usage_counts();
                    self.media_label_create_name.clear();
                    self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
                    self.compare_action_message = "Label created".to_string();
                }
                Err(error) => {
                    self.compare_action_message = format!("Label not created: {error}");
                }
            }
        }
        ui.add_space(6.0);
        let mut definitions = self.media_label_definitions.clone();
        let mut save_id: Option<String> = None;
        let mut delete_id: Option<String> = None;
        let mut label_colors_changed = false;
        for definition in &mut definitions {
            let usage = self
                .media_label_usage_counts
                .get(&definition.id)
                .copied()
                .unwrap_or(0);
            let mut draw_label_identity = |ui: &mut egui::Ui, name_width: f32, hex_width: f32| {
                let (_, rgb) = crate::media_db::normalize_hex_color(&definition.hex)
                    .unwrap_or_else(|| ("#808080".to_string(), [128, 128, 128]));
                let mut rgb = rgb;
                if ui
                    .color_edit_button_srgb(&mut rgb)
                    .on_hover_text("Choose label color")
                    .changed()
                {
                    definition.hex = format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
                    label_colors_changed = true;
                }
                ui.add(
                    TextEdit::singleline(&mut definition.name)
                        .desired_width(name_width)
                        .hint_text("Label name"),
                );
                if ui
                    .add(
                        TextEdit::singleline(&mut definition.hex)
                            .desired_width(hex_width)
                            .font(egui::TextStyle::Monospace),
                    )
                    .changed()
                {
                    label_colors_changed = true;
                }
            };
            let usage_text = format!("{usage} file{}", if usage == 1 { "" } else { "s" });
            let confirming =
                self.media_label_delete_confirm.as_deref() == Some(definition.id.as_str());
            let delete_text = if usage > 0 && !confirming {
                "Remove…"
            } else if usage > 0 {
                "Confirm remove"
            } else {
                "Remove"
            };
            if label_manager_large_rows {
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    let name_width = (ui.available_width() - 260.0).clamp(260.0, 430.0);
                    ui.horizontal(|ui| draw_label_identity(ui, name_width, 156.0));
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&usage_text)
                                .strong()
                                .color(theme::ink_soft()),
                        );
                        if ui
                            .add_sized([120.0, 44.0], egui::Button::new("Save"))
                            .clicked()
                        {
                            save_id = Some(definition.id.clone());
                        }
                        if ui
                            .add_sized([220.0, 44.0], egui::Button::new(delete_text))
                            .clicked()
                        {
                            if usage > 0 && !confirming {
                                self.media_label_delete_confirm = Some(definition.id.clone());
                            } else {
                                delete_id = Some(definition.id.clone());
                            }
                        }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    draw_label_identity(ui, 190.0, 82.0);
                    ui.label(
                        egui::RichText::new(&usage_text)
                            .small()
                            .color(theme::ink_faint()),
                    );
                    if ui.small_button("Save").clicked() {
                        save_id = Some(definition.id.clone());
                    }
                    if ui.small_button(delete_text).clicked() {
                        if usage > 0 && !confirming {
                            self.media_label_delete_confirm = Some(definition.id.clone());
                        } else {
                            delete_id = Some(definition.id.clone());
                        }
                    }
                });
            }
        }
        self.media_label_definitions = definitions.clone();
        if label_colors_changed {
            self.refresh_media_label_colors();
        }
        if let Some(id) = save_id {
            if let Some(definition) = definitions.iter().find(|item| item.id == id) {
                match self.media_db.update_color_label(
                    &id,
                    Some(&definition.name),
                    Some(&definition.hex),
                ) {
                    Ok(_) => {
                        self.media_label_definitions = self.media_db.color_label_definitions();
                        self.refresh_media_label_colors();
                        self.media_label_delete_confirm = None;
                        self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
                        self.compare_action_message = "Label saved".to_string();
                    }
                    Err(error) => {
                        self.compare_action_message = format!("Label not saved: {error}");
                    }
                }
            }
        }
        if let Some(id) = delete_id {
            match self.media_db.delete_color_label(&id, true) {
                Ok(result) => {
                    self.load_media_metadata();
                    self.media_label_delete_confirm = None;
                    self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
                    self.compare_action_message = format!(
                        "Label removed from {} file{}",
                        result.assignments_removed,
                        if result.assignments_removed == 1 {
                            ""
                        } else {
                            "s"
                        }
                    );
                }
                Err(error) => {
                    self.compare_action_message = format!("Label not removed: {error}");
                }
            }
        }
    }

    /// Compact grouped controls editor (WP-062). Normal/couch widths use one
    /// centered three-column table; constrained widths stack the two explicitly
    /// labelled bindings under their action. There is no nested ScrollArea.
    fn draw_media_settings_controls(&mut self, ui: &mut egui::Ui) {
        theme::kicker(ui, "Keyboard & controller mappings");
        ui.label(
            egui::RichText::new("Choose a Keyboard or Controller cell, then press the replacement input. Controller video defaults use the right stick.")
                .small()
                .color(theme::ink_faint()),
        );
        let controller_status = if self.controller_active.is_some() || self.controller_legacy_active
        {
            "Controller: connected · ready to navigate or remap".to_string()
        } else {
            "Controller: not detected · connect one to capture or test mappings".to_string()
        };
        ui.label(
            egui::RichText::new(controller_status)
                .small()
                .strong()
                .color(theme::ink_soft()),
        );
        if let Some(capture) = &self.media_capture {
            let slot = match capture.slot {
                crate::media_input::CaptureSlot::Keyboard(action) => {
                    format!("press keys for '{}'", action.label())
                }
                crate::media_input::CaptureSlot::Pad(action) => {
                    format!("press a controller input for '{}'", action.label())
                }
            };
            ui.label(
                egui::RichText::new(format!("Listening… {slot} (Esc cancels)"))
                    .small()
                    .color(theme::accent()),
            );
        }
        let mut arm: Option<crate::media_input::CaptureSlot> = None;
        let mut reset = false;
        // The settings shell owns the only ScrollArea. Keeping a second one
        // here reintroduced the available-height/outer-size feedback loop.
        use crate::media_input::MediaAction as A;
        const NAVIGATION: &[A] = &[
            A::MoveLeft,
            A::MoveRight,
            A::MoveUp,
            A::MoveDown,
            A::PageUp,
            A::PageDown,
            A::Home,
            A::End,
            A::FolderUp,
            A::FolderEnter,
            A::FolderPrevSibling,
            A::FolderNextSibling,
            A::ToggleFolderNavigator,
            A::FocusSearch,
            A::Refresh,
        ];
        const SELECTION: &[A] = &[
            A::ToggleSelect,
            A::SelectAll,
            A::SelectNone,
            A::InvertSelection,
        ];
        const FILES: &[A] = &[
            A::OpenFile,
            A::OpenLocation,
            A::Delete,
            A::Copy,
            A::Cut,
            A::Paste,
            A::Rename,
        ];
        const VIEW: &[A] = &[
            A::ToggleFavoritesPanel,
            A::TogglePointerMode,
            A::ToggleSettingsPanel,
            A::ToggleViewMode,
            A::ToggleChromeHide,
            A::ThumbZoomIn,
            A::ThumbZoomOut,
        ];
        const PLAYBACK: &[A] = &[
            A::VideoSeekBack,
            A::VideoSeekForward,
            A::VideoVolumeDown,
            A::VideoVolumeUp,
        ];
        const GROUPS: [(&str, &[A]); 5] = [
            ("Navigation", NAVIGATION),
            ("Selection", SELECTION),
            ("Files", FILES),
            ("View", VIEW),
            ("Playback", PLAYBACK),
        ];

        let outer_w = ui.available_width();
        let table_cap = if self.media_explorer.settings_couch_fullscreen {
            1120.0
        } else {
            760.0
        };
        let table_w = outer_w.min(table_cap);
        let left_pad = ((outer_w - table_w) * 0.5).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(left_pad);
            ui.allocate_ui_with_layout(
                egui::vec2(table_w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let couch = self.media_explorer.settings_couch_fullscreen;
                    let row_h = if couch { 48.0 } else { 28.0 };
                    let narrow = table_w < 650.0;
                    if narrow {
                        ui.horizontal_wrapped(|ui| {
                            for heading in ["Action", "Keyboard", "Controller"] {
                                ui.label(egui::RichText::new(heading).strong().color(theme::ink()));
                                if heading != "Controller" {
                                    ui.label(egui::RichText::new("/").color(theme::ink_faint()));
                                }
                            }
                        });
                        theme::hairline(ui);
                    }
                    for (group_index, (group_name, actions)) in GROUPS.iter().enumerate() {
                        if group_index > 0 {
                            ui.add_space(if couch { 16.0 } else { 8.0 });
                        }
                        ui.label(
                            egui::RichText::new(*group_name)
                                .strong()
                                .color(theme::ink()),
                        );
                        theme::hairline(ui);
                        if narrow {
                            for action in *actions {
                                ui.add_space(3.0);
                                ui.label(
                                    egui::RichText::new(action.label())
                                        .strong()
                                        .color(theme::ink_soft()),
                                );
                                let kb = self
                                    .media_bindings
                                    .keyboard
                                    .get(action)
                                    .map(|binding| binding.display())
                                    .filter(|text| !text.is_empty())
                                    .unwrap_or_else(|| "Unassigned".to_string());
                                let pad = self
                                    .media_bindings
                                    .pad
                                    .get(action)
                                    .map(|binding| binding.display())
                                    .filter(|text| !text.is_empty())
                                    .unwrap_or_else(|| "Unassigned".to_string());
                                let label_w = if couch { 190.0 } else { 132.0 };
                                let binding_w = (table_w - label_w - 12.0).max(120.0);
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [label_w, row_h],
                                        egui::Label::new(egui::RichText::new("Keyboard").strong()),
                                    );
                                    if settings_binding_button(
                                        ui,
                                        &kb,
                                        binding_w,
                                        row_h,
                                        "Choose, then press the new keyboard chord",
                                    ) {
                                        arm = Some(crate::media_input::CaptureSlot::Keyboard(
                                            *action,
                                        ));
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [label_w, row_h],
                                        egui::Label::new(
                                            egui::RichText::new("Controller").strong(),
                                        ),
                                    );
                                    if settings_binding_button(
                                        ui,
                                        &pad,
                                        binding_w,
                                        row_h,
                                        "Choose, then press the new controller input",
                                    ) {
                                        arm = Some(crate::media_input::CaptureSlot::Pad(*action));
                                    }
                                });
                                theme::hairline(ui);
                            }
                        } else {
                            let action_w = (table_w * 0.40).clamp(220.0, 430.0);
                            let binding_w = ((table_w - action_w - 20.0) * 0.5).max(150.0);
                            egui::Grid::new(("media_bindings_group", group_index))
                                .num_columns(3)
                                .striped(true)
                                .min_col_width(0.0)
                                .spacing(egui::vec2(8.0, 5.0))
                                .show(ui, |ui| {
                                    for (label, width) in [
                                        ("Action", action_w),
                                        ("Keyboard", binding_w),
                                        ("Controller", binding_w),
                                    ] {
                                        ui.add_sized(
                                            [width, row_h],
                                            egui::Label::new(
                                                egui::RichText::new(label)
                                                    .strong()
                                                    .color(theme::ink()),
                                            ),
                                        );
                                    }
                                    ui.end_row();
                                    for action in *actions {
                                        ui.add_sized(
                                            [action_w, row_h],
                                            egui::Label::new(
                                                egui::RichText::new(action.label())
                                                    .color(theme::ink_soft()),
                                            ),
                                        );
                                        let kb = self
                                            .media_bindings
                                            .keyboard
                                            .get(action)
                                            .map(|binding| binding.display())
                                            .filter(|text| !text.is_empty())
                                            .unwrap_or_else(|| "Unassigned".to_string());
                                        if settings_binding_button(
                                            ui,
                                            &kb,
                                            binding_w,
                                            row_h,
                                            "Choose, then press the new keyboard chord",
                                        ) {
                                            arm = Some(crate::media_input::CaptureSlot::Keyboard(
                                                *action,
                                            ));
                                        }
                                        let pad = self
                                            .media_bindings
                                            .pad
                                            .get(action)
                                            .map(|binding| binding.display())
                                            .filter(|text| !text.is_empty())
                                            .unwrap_or_else(|| "Unassigned".to_string());
                                        if settings_binding_button(
                                            ui,
                                            &pad,
                                            binding_w,
                                            row_h,
                                            "Choose, then press the new controller input",
                                        ) {
                                            arm =
                                                Some(crate::media_input::CaptureSlot::Pad(*action));
                                        }
                                        ui.end_row();
                                    }
                                });
                        }
                    }
                },
            );
        });
        ui.add_space(4.0);
        if ui.small_button("Reset all bindings to defaults").clicked() {
            reset = true;
        }
        if let Some(slot) = arm {
            self.media_capture = Some(crate::media_input::Capture {
                slot,
                armed_at_ms: self.input_epoch.elapsed().as_millis() as u64,
            });
        }
        if reset {
            self.media_bindings = crate::media_input::BindingTable::default();
            self.save_media_bindings();
            self.compare_action_message = "Bindings reset to defaults".to_string();
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(if self.controller_pointer_mode {
                "Controller cursor active — right stick moves, A left-clicks, B right-clicks, R3 exits. Start switches apps and releases control."
            } else {
                "Controller defaults: D-pad/left stick navigate, R3 enters cursor mode, and Start switches apps."
            })
            .small()
            .color(theme::ink_faint()),
        );
    }

    fn media_handle_input(
        &mut self,
        ui: &mut egui::Ui,
        lane_id: usize,
        display: &[usize],
        request: &mut CompareLaneRenderRequest,
    ) {
        use crate::media_input::{CaptureSlot, KeyChord, MediaAction};
        let focus_free = ui.ctx().memory(|m| m.focused().is_none());
        let columns = self.media_explorer.last_grid_columns.max(1);
        let now_ms = self.input_epoch.elapsed().as_millis() as u64;

        // Expire a stale capture; Esc cancels it.
        if let Some(capture) = &self.media_capture {
            if capture.expired(now_ms) {
                self.media_capture = None;
                self.compare_action_message = "Rebind timed out".to_string();
            }
        }

        let escape = ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            if self.media_capture.take().is_some() {
                self.compare_action_message = "Rebind cancelled".to_string();
            } else if self.media_explorer.show_folder_navigator {
                self.close_media_folder_navigator();
            } else if self.media_explorer.show_settings {
                if self.media_explorer.settings_couch_fullscreen {
                    // First Escape returns to the compact Settings surface;
                    // Settings itself remains open. A later Escape closes it.
                    self.exit_settings_couch_fullscreen(ui.ctx());
                } else {
                    self.close_media_settings(ui.ctx());
                }
            } else if self.media_explorer.chrome_hidden {
                self.media_explorer.chrome_hidden = false;
                self.media_explorer.chrome_hidden_at = None;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        }

        // ---- keyboard: events -> chords -> bound actions ----
        // Fired actions carry their SOURCE: keyboard-sourced actions must
        // never fire while a text field has focus (typing Space/Backspace/
        // arrows into the search box was navigating folders when a pad was
        // merely plugged in — round 3, finding 1); pad-sourced actions
        // cannot type and always pass.
        #[derive(Clone, Copy, PartialEq)]
        enum FireSource {
            Keyboard,
            Pad,
        }
        let mut fired: Vec<(MediaAction, bool, FireSource)> = Vec::new();
        let events = ui.ctx().input(|i| i.events.clone());
        for event in &events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if *key == egui::Key::Escape {
                continue; // handled above
            }
            let chord = KeyChord {
                key: format!("{key:?}"),
                ctrl: modifiers.ctrl || modifiers.command,
                shift: modifiers.shift,
                alt: modifiers.alt,
            };
            // Armed keyboard rebind swallows the chord.
            if let Some(capture) = &self.media_capture {
                if let CaptureSlot::Keyboard(action) = capture.slot {
                    self.media_bindings.rebind_keyboard(action, chord.clone());
                    self.save_media_bindings();
                    self.media_capture = None;
                    self.compare_action_message =
                        format!("{} bound to {}", action.label(), chord.display());
                    continue;
                }
            }
            if let Some(action) = self.media_bindings.action_for_chord(&chord) {
                fired.push((action, false, FireSource::Keyboard));
            } else if chord.shift {
                // Shift+<move binding> extends the selection (Explorer).
                let mut base = chord.clone();
                base.shift = false;
                if let Some(action) = self.media_bindings.action_for_chord(&base) {
                    fired.push((action, true, FireSource::Keyboard));
                }
            }
        }

        // ---- controller: bound fires + analog stick scroll ----
        let app_focused = ui
            .ctx()
            .input(|input| input.viewport().focused.unwrap_or(true));
        let (pad_fired, stick_rows) = self.media_poll_controller(app_focused);
        for action in pad_fired {
            fired.push((action, false, FireSource::Pad));
        }
        for _ in 0..stick_rows.abs() {
            fired.push((
                if stick_rows > 0 {
                    MediaAction::MoveDown
                } else {
                    MediaAction::MoveUp
                },
                false,
                FireSource::Pad,
            ));
        }

        for (action, extend, source) in fired {
            // Keyboard-sourced actions never fire while a text editor has
            // focus (the chrome toggle is the one safe exception); pad
            // fires cannot type and always pass.
            let is_chrome = action == MediaAction::ToggleChromeHide;
            if source == FireSource::Keyboard && !focus_free && !is_chrome {
                continue;
            }
            self.media_perform_action(ui.ctx(), action, extend, lane_id, display, columns, request);
        }
    }

    fn set_controller_pointer_mode(&mut self, enabled: bool) {
        if !enabled {
            if self.controller_pointer_left_down {
                let _ = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Left,
                    false,
                );
            }
            if self.controller_pointer_right_down {
                let _ = crate::platform_input::set_pointer_button(
                    crate::platform_input::PointerButton::Right,
                    false,
                );
            }
            self.controller_pointer_left_down = false;
            self.controller_pointer_right_down = false;
            self.controller_pointer_accum = [0.0, 0.0];
        }
        self.controller_pointer_mode = enabled;
        self.media_repeat.clear();
        self.compare_action_message = if enabled {
            "Controller cursor: right stick moves, A/B click, R3 exits".to_string()
        } else {
            "Controller cursor off — native navigation restored".to_string()
        };
    }

    /// Execute one media action (WP-046 dispatcher). `extend` carries the
    /// Shift-range semantic for cursor moves.
    fn media_perform_action(
        &mut self,
        ctx: &egui::Context,
        action: crate::media_input::MediaAction,
        extend: bool,
        lane_id: usize,
        display: &[usize],
        columns: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        use crate::media_input::MediaAction as A;
        // The couch navigator is an explicit controller focus group. While it
        // is open, media-grid actions cannot leak through to hidden content.
        if self.media_explorer.show_folder_navigator {
            match action {
                A::MoveUp => self.media_navigator_move(lane_id, -1),
                A::MoveDown => self.media_navigator_move(lane_id, 1),
                A::PageUp => self.media_navigator_move(lane_id, -8),
                A::PageDown => self.media_navigator_move(lane_id, 8),
                A::Home => {
                    self.media_explorer.folder_cursor =
                        (!self.media_folder_entries(lane_id).is_empty()).then_some(0);
                    self.media_explorer.folder_scroll_to_cursor = true;
                }
                A::End => {
                    self.media_explorer.folder_cursor =
                        self.media_folder_entries(lane_id).len().checked_sub(1);
                    self.media_explorer.folder_scroll_to_cursor = true;
                }
                A::MoveLeft => {
                    if !self.media_navigator_move_drive(lane_id, -1) {
                        self.media_navigator_parent_or_close(lane_id);
                    }
                }
                A::MoveRight => {
                    if !self.media_navigator_move_drive(lane_id, 1) {
                        self.media_navigator_enter(lane_id);
                    }
                }
                A::FolderUp => self.media_navigator_parent_or_close(lane_id),
                A::FolderEnter | A::OpenFile => self.media_navigator_enter(lane_id),
                A::FolderPrevSibling | A::FolderNextSibling => {
                    let delta = if action == A::FolderPrevSibling {
                        -1
                    } else {
                        1
                    };
                    self.media_navigate_sibling(lane_id, delta, request);
                    let entries = self.media_folder_entries(lane_id);
                    self.media_explorer.folder_cursor = entries
                        .iter()
                        .position(|entry| !entry.is_parent && !entry.is_drive)
                        .or_else(|| (!entries.is_empty()).then_some(0));
                    self.media_explorer.folder_scroll_to_cursor = true;
                }
                A::ToggleFolderNavigator => {
                    self.close_media_folder_navigator();
                }
                A::ToggleSettingsPanel => {
                    self.close_media_folder_navigator();
                    self.request_media_settings(ctx, self.media_explorer.settings_category);
                }
                A::ToggleFavoritesPanel => {
                    self.close_media_folder_navigator();
                    self.media_explorer.show_favorites = false;
                    self.media_explorer.show_settings = false;
                    let _ = self.open_media_collection_tab();
                }
                A::TogglePointerMode => {
                    self.set_controller_pointer_mode(!self.controller_pointer_mode)
                }
                A::ToggleChromeHide => {
                    self.media_explorer.chrome_hidden = !self.media_explorer.chrome_hidden;
                    self.media_explorer.chrome_hidden_at = self
                        .media_explorer
                        .chrome_hidden
                        .then(std::time::Instant::now);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                        self.media_explorer.chrome_hidden,
                    ));
                }
                A::Refresh => request.refresh = true,
                _ => {}
            }
            return;
        }
        let move_cursor = |this: &mut Self,
                           dx: isize,
                           dy: isize,
                           extend: bool,
                           request: &mut CompareLaneRenderRequest| {
            let _ = request;
            let next = crate::media_explorer::move_cursor(
                this.media_explorer.cursor,
                dx,
                dy,
                columns,
                display.len(),
            );
            if let Some(next) = next {
                this.media_explorer.cursor = Some(next);
                this.media_apply_tile_click(lane_id, display, next, false, extend);
                this.media_scroll_to_cursor = true;
            }
        };
        match action {
            A::MoveLeft => move_cursor(self, -1, 0, extend, request),
            A::MoveRight => move_cursor(self, 1, 0, extend, request),
            A::MoveUp => move_cursor(self, 0, -1, extend, request),
            A::MoveDown => move_cursor(self, 0, 1, extend, request),
            A::PageUp => move_cursor(self, 0, -4, extend, request),
            A::PageDown => move_cursor(self, 0, 4, extend, request),
            A::Home => {
                if !display.is_empty() {
                    self.media_explorer.cursor = Some(0);
                    self.media_apply_tile_click(lane_id, display, 0, false, extend);
                    self.media_scroll_to_cursor = true;
                }
            }
            A::End => {
                if let Some(last) = display.len().checked_sub(1) {
                    self.media_explorer.cursor = Some(last);
                    self.media_apply_tile_click(lane_id, display, last, false, extend);
                    self.media_scroll_to_cursor = true;
                }
            }
            A::FolderUp => self.media_navigate_parent(lane_id, request),
            A::FolderEnter => self.media_navigate_first_child(lane_id, request),
            A::FolderPrevSibling => self.media_navigate_sibling(lane_id, -1, request),
            A::FolderNextSibling => self.media_navigate_sibling(lane_id, 1, request),
            A::ToggleFolderNavigator => self.media_toggle_folder_navigator(ctx, lane_id),
            A::OpenFile => {
                if !self.media_toggle_selected_video(lane_id) {
                    request.open_file = true;
                }
            }
            A::OpenLocation => request.open_location = true,
            A::ToggleSelect => {
                if let Some(cursor) = self.media_explorer.cursor {
                    self.media_apply_tile_click(lane_id, display, cursor, true, false);
                }
            }
            A::SelectAll => request.select_all = true,
            A::SelectNone => request.select_none = true,
            A::InvertSelection => request.invert_selection = true,
            A::Delete => request.delete_selected = true,
            A::Copy => request.copy_selected = true,
            A::Cut => request.cut_selected = true,
            A::Paste => request.paste = true,
            A::Rename => request.rename_selected = true,
            A::ToggleFavoritesPanel => {
                // WP-067: Ctrl+B opens (or focuses) the favourites collection
                // tab instead of the right-edge overlay. The overlay used to
                // blank the video player while open; a tab does not.
                self.media_explorer.show_favorites = false;
                self.media_explorer.show_settings = false;
                match self.open_media_collection_tab() {
                    Ok(id) => self.compare_action_message = format!("Favorites tab {id}"),
                    Err(error) => self.compare_action_message = error,
                }
            }
            A::TogglePointerMode => self.set_controller_pointer_mode(!self.controller_pointer_mode),
            A::ToggleSettingsPanel => {
                if self.media_explorer.show_settings
                    || self.settings_backdrop_requested_at.is_some()
                {
                    self.close_media_settings(ctx);
                } else {
                    self.request_media_settings(ctx, self.media_explorer.settings_category);
                }
            }
            A::ToggleViewMode => {
                self.media_explorer.view_mode = match self.media_explorer.view_mode {
                    crate::media_explorer::MediaViewMode::TwoPanel => {
                        crate::media_explorer::MediaViewMode::FullGrid
                    }
                    crate::media_explorer::MediaViewMode::FullGrid => {
                        crate::media_explorer::MediaViewMode::TwoPanel
                    }
                };
                self.touch_media_settings();
            }
            A::ToggleChromeHide => {
                self.media_explorer.chrome_hidden = !self.media_explorer.chrome_hidden;
                self.media_explorer.chrome_hidden_at = self
                    .media_explorer
                    .chrome_hidden
                    .then(std::time::Instant::now);
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                    self.media_explorer.chrome_hidden,
                ));
            }
            A::ThumbZoomIn | A::ThumbZoomOut => {
                let factor = if action == A::ThumbZoomIn { 1.1 } else { 0.9 };
                self.media_explorer.tile_edge = (self.media_explorer.tile_edge * factor).clamp(
                    crate::media_explorer::TILE_MIN,
                    crate::media_explorer::TILE_MAX,
                );
                self.touch_media_settings();
            }
            A::FocusSearch => self.media_focus_search = true,
            A::Refresh => request.refresh = true,
            A::VideoSeekBack => self.media_adjust_video(lane_id, -10_000, 0),
            A::VideoSeekForward => self.media_adjust_video(lane_id, 10_000, 0),
            A::VideoVolumeDown => self.media_adjust_video(lane_id, 0, -5),
            A::VideoVolumeUp => self.media_adjust_video(lane_id, 0, 5),
        }
    }

    fn media_navigate_parent(&mut self, lane_id: usize, request: &mut CompareLaneRenderRequest) {
        if let Some(pos) = self.compare_lane_position(lane_id) {
            let current = sanitize_folder_input(&self.compare_lanes[pos].folder);
            if let Some(parent) = Path::new(&current).parent().and_then(|p| p.to_str()) {
                self.compare_lanes[pos].folder = parent.to_string();
                request.scan = true;
                self.media_explorer.cursor = None;
                self.compare_action_message = "Moved to parent folder".to_string();
            }
        }
    }

    fn begin_media_playback_priority(&mut self) {
        if self.media_playback_lease.is_none() {
            if let Some(root) = self.media_root_identity.clone() {
                self.media_playback_lease = Some(self.media_io.begin_playback(root));
            }
        }
    }

    fn play_media_video(&mut self, path: &Path) -> Result<(), String> {
        // Playback must win before LibVLC opens the file. On 20k-video folders,
        // granting priority after `play` allowed active ffmpeg thumbnail reads
        // to delay first frame by seconds.
        crate::video_player::playback_trace_phase("ui.playback_priority.begin", "");
        self.begin_media_playback_priority();
        crate::video_player::playback_trace_phase("ui.playback_priority.end", "ok");
        if let Some(engine) = self.thumb_engine.as_ref() {
            crate::video_player::playback_trace_phase("ui.thumbnail_cancel.begin", "");
            engine.bump_generation();
            crate::video_player::playback_trace_phase("ui.thumbnail_cancel.end", "ok");
        }
        let result = self.video_player.play(path);
        if result.is_err() {
            self.media_playback_lease = None;
        }
        result
    }

    fn sync_media_playback_priority(&mut self, snapshot: Option<&crate::video_player::Snapshot>) {
        let active = snapshot.is_some_and(|state| {
            state.error.is_none()
                && matches!(
                    state.status,
                    crate::video_player::PlaybackStatus::Pending
                        | crate::video_player::PlaybackStatus::Opening
                        | crate::video_player::PlaybackStatus::Buffering
                        | crate::video_player::PlaybackStatus::Playing
                )
        });
        if active {
            self.begin_media_playback_priority();
        } else {
            self.media_playback_lease = None;
        }
    }

    /// Enter / controller A is contextual: images open externally, while a
    /// selected video starts or pauses the embedded couch-friendly player.
    fn media_toggle_selected_video(&mut self, lane_id: usize) -> bool {
        let Some(path) = self.media_selected_path(lane_id) else {
            return false;
        };
        if !crate::media_explorer::is_video_path(&path) {
            return false;
        }
        let result = if self.video_player.active_path() == Some(path.as_str()) {
            self.video_player.toggle_pause()
        } else {
            self.media_inline_video_path = None;
            self.media_inline_video_requested_at = None;
            self.media_inline_video_pending_target = None;
            self.play_media_video(Path::new(&path))
        };
        match result {
            Ok(()) => {
                self.begin_media_playback_priority();
                self.set_compare_lane_message(lane_id, "Video play/pause".to_string());
            }
            Err(error) => self.set_compare_lane_message(lane_id, error),
        }
        true
    }

    /// Controller-safe transport adjustment for the active selected video.
    /// Right-stick actions are intentionally no-ops until the operator starts
    /// playback, so browser navigation remains predictable.
    fn media_adjust_video(&mut self, lane_id: usize, time_delta_ms: i64, volume_delta: i32) {
        let selected = self.media_selected_path(lane_id);
        let Some(path) = selected
            .as_deref()
            .filter(|path| crate::media_explorer::is_video_path(path))
        else {
            return;
        };
        if self.video_player.active_path() != Some(path) {
            self.set_compare_lane_message(lane_id, "Start the selected video first".to_string());
            return;
        }
        let Some(state) = self.video_player.cached_snapshot() else {
            return;
        };
        let mut interacted = false;
        if time_delta_ms != 0 {
            match self
                .video_player
                .set_time((state.time_ms + time_delta_ms).clamp(0, state.length_ms.max(0)))
            {
                Ok(()) => interacted = true,
                Err(error) => self.set_compare_lane_message(lane_id, error),
            }
        }
        if volume_delta != 0 {
            match self
                .video_player
                .set_volume((state.volume + volume_delta).clamp(0, 125))
            {
                Ok(()) => interacted = true,
                Err(error) => self.set_compare_lane_message(lane_id, error),
            }
        }
        if interacted {
            self.begin_media_playback_priority();
        }
    }

    fn media_navigate_first_child(
        &mut self,
        lane_id: usize,
        request: &mut CompareLaneRenderRequest,
    ) {
        if let Some(pos) = self.compare_lane_position(lane_id) {
            let current = sanitize_folder_input(&self.compare_lanes[pos].folder);
            if let Some(first) = self.media_child_folders(lane_id, &current).first() {
                let next = Path::new(&current).join(first);
                self.compare_lanes[pos].folder = next.to_string_lossy().to_string();
                request.scan = true;
                self.media_explorer.cursor = None;
                self.compare_action_message = "Moved into first subfolder".to_string();
            }
        }
    }

    /// Jump to the previous/next sibling folder (same parent), wrapping.
    fn media_navigate_sibling(
        &mut self,
        lane_id: usize,
        delta: isize,
        request: &mut CompareLaneRenderRequest,
    ) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let current = sanitize_folder_input(&self.compare_lanes[pos].folder);
        let current_path = Path::new(&current);
        let (Some(parent), Some(name)) = (
            current_path.parent().and_then(|p| p.to_str()),
            current_path.file_name().and_then(|n| n.to_str()),
        ) else {
            return;
        };
        let siblings = self.media_child_folders(lane_id, parent);
        if siblings.is_empty() {
            return;
        }
        let index = siblings
            .iter()
            .position(|s| s.eq_ignore_ascii_case(name))
            .unwrap_or(0);
        let next_index = wrap_relative_index(index, delta, siblings.len());
        if next_index == index {
            return;
        }
        let next = Path::new(parent).join(&siblings[next_index]);
        self.compare_lanes[pos].folder = next.to_string_lossy().to_string();
        request.scan = true;
        self.media_explorer.cursor = None;
        self.compare_action_message = format!("Sibling folder: {}", siblings[next_index]);
    }

    /// Kick the CLIP engine load off-thread when the models are provisioned
    /// (WP-047). Absent models set the fallback status line instead.
    fn start_clip_engine_load(&mut self) {
        if self.clip_engine.is_some() || self.clip_loading {
            return;
        }
        let status = crate::media_clip::resolve(&self.config);
        if !status.ready() {
            self.clip_status = status.detail;
            return;
        }
        self.clip_loading = true;
        self.clip_status = "semantic search: loading CLIP models…".to_string();
        let tx = self.compare_work_tx.clone();
        thread::spawn(move || {
            let result = crate::media_clip::ClipEngine::load(&status).map(std::sync::Arc::new);
            let _ = tx.send(CompareWorkEvent::ClipReady(result));
        });
    }

    /// Kick a background embedding-index build for the current media folder
    /// when Semantic mode needs it. One build at a time; the separate
    /// clip_index.redb is only ever opened inside worker tasks.
    fn maybe_start_clip_index(&mut self, lane_id: usize) {
        if self.media_search_mode != 2 || self.clip_indexing {
            return;
        }
        let Some(engine) = self.clip_engine.clone() else {
            return;
        };
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        if self.compare_lanes[pos].scanning && !self.compare_lanes[pos].scan_using_cached_inventory
        {
            return;
        }
        let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
        if folder.is_empty() || self.compare_lanes[pos].files.is_empty() {
            return;
        }
        if self.clip_indexed_folder.as_deref() == Some(folder.as_str()) {
            return;
        }
        let files = Arc::clone(&self.compare_lanes[pos].files);
        let request_key = ClipIndexRequestKey {
            lane_id,
            scan_id: self.compare_lanes[pos].scan_id,
            content_generation: self.media_content_generation,
            folder,
        };
        if let Some(cancel) = self.clip_index_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.clip_index_cancel = Some(Arc::clone(&cancelled));
        self.clip_index_request = Some(request_key.clone());
        self.clip_indexing = true;
        self.clip_status = "semantic search: indexing folder…".to_string();
        let tx = self.compare_work_tx.clone();
        let workspace = self.config.workspace_root.clone();
        let root_identity = self.media_root_identity.clone().unwrap_or_else(|| {
            crate::media_io::RootIdentity::new(
                request_key.folder.clone(),
                0,
                crate::media_io::RootKind::Unknown,
            )
        });
        let media_io = Arc::clone(&self.media_io);
        thread::spawn(move || {
            let index = match crate::media_clip::ClipIndex::open(&workspace) {
                Ok(index) => index,
                Err(_) => {
                    let _ = tx.send(CompareWorkEvent::ClipIndexDone {
                        key: request_key,
                        indexed: 0,
                        failed: 0,
                        ok: false,
                    });
                    return;
                }
            };
            let total = files
                .iter()
                .filter(|path| !crate::media_explorer::is_video_path(path))
                .count();
            let mut indexed = 0usize;
            let mut failed = 0usize;
            let mut done = 0usize;
            for path in files.iter() {
                if crate::media_explorer::is_video_path(path) {
                    continue;
                }
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let io_request =
                    media_io.enqueue(root_identity.clone(), crate::media_io::WorkClass::Prefetch);
                let io_permit = loop {
                    if cancelled.load(Ordering::Acquire) {
                        io_request.cancel();
                        return;
                    }
                    match io_request.try_acquire() {
                        Ok(Some(permit)) => break permit,
                        Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                        Err(_) => return,
                    }
                };
                let io_started = std::time::Instant::now();
                let key = crate::media_db::canonical_key(&workspace, path);
                let (mtime, size) = file_stat_pair(path);
                let mut item_failed = false;
                if index.get(&key, mtime, size).is_none() {
                    match engine.embed_image_path(path) {
                        Ok(embedding) => {
                            if index.put(&key, mtime, size, &embedding).is_ok() {
                                indexed += 1;
                            } else {
                                failed += 1;
                                item_failed = true;
                            }
                        }
                        Err(_) => {
                            failed += 1;
                            item_failed = true;
                        }
                    }
                }
                media_io.record_filesystem_duration(
                    &root_identity,
                    crate::media_io::WorkClass::Prefetch,
                    io_started.elapsed(),
                );
                io_permit.finish(if item_failed {
                    crate::media_io::PermitOutcome::Error
                } else {
                    crate::media_io::PermitOutcome::Success
                });
                done += 1;
                if done == 1 || done % 16 == 0 {
                    let _ = tx.send(CompareWorkEvent::ClipIndexProgress {
                        key: request_key.clone(),
                        done,
                        total,
                    });
                }
            }
            if !cancelled.load(Ordering::Acquire) {
                let _ = tx.send(CompareWorkEvent::ClipIndexDone {
                    key: request_key,
                    indexed,
                    failed,
                    ok: true,
                });
            }
        });
    }

    /// Kick a semantic query when the ranked cache is stale for the current
    /// (folder, query). Embedding + cosine ranking run off-thread.
    fn maybe_start_clip_query(&mut self, lane_id: usize) {
        if self.media_search_mode != 2 || self.clip_indexing {
            return;
        }
        // Embed only the FREE TEXT: chip tokens like "tag:hero" are filters,
        // not content, and would pollute the CLIP embedding (round 3, f.6).
        let query = crate::media_search::parse_query(&self.media_search_query).text;
        if query.is_empty() {
            if let Some(cancel) = self.clip_query_cancel.take() {
                cancel.store(true, Ordering::Release);
            }
            self.clip_query_request = None;
            self.media_semantic_inflight = None;
            if self.media_semantic.take().is_some() {
                self.media_semantic_generation = self.media_semantic_generation.wrapping_add(1);
            }
            return;
        }
        // Back off after a failure (e.g. index locked by a CLI process)
        // instead of respawning a failing thread every frame.
        if self
            .clip_query_backoff
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2))
        {
            return;
        }
        let Some(engine) = self.clip_engine.clone() else {
            return;
        };
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        if self.compare_lanes[pos].scanning && !self.compare_lanes[pos].scan_using_cached_inventory
        {
            return;
        }
        let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
        if self.clip_indexed_folder.as_deref() != Some(folder.as_str()) {
            return; // index first (maybe_start_clip_index runs this frame)
        }
        let fresh = self
            .media_semantic
            .as_ref()
            .is_some_and(|(f, q, _)| f == &folder && q == &query);
        let request_key = ClipQueryRequestKey {
            lane_id,
            scan_id: self.compare_lanes[pos].scan_id,
            content_generation: self.media_content_generation,
            folder,
            query: query.clone(),
        };
        if fresh || self.clip_query_request.as_ref() == Some(&request_key) {
            return;
        }
        if let Some(cancel) = self.clip_query_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.clip_query_cancel = Some(Arc::clone(&cancelled));
        self.clip_query_request = Some(request_key.clone());
        self.media_semantic_inflight = Some(query.clone());
        let files = Arc::clone(&self.compare_lanes[pos].files);
        let tx = self.compare_work_tx.clone();
        let workspace = self.config.workspace_root.clone();
        let root_identity = self.media_root_identity.clone().unwrap_or_else(|| {
            crate::media_io::RootIdentity::new(
                request_key.folder.clone(),
                0,
                crate::media_io::RootKind::Unknown,
            )
        });
        let media_io = Arc::clone(&self.media_io);
        thread::spawn(move || {
            let result = (|| -> Result<(Vec<usize>, usize), String> {
                let index = crate::media_clip::ClipIndex::open(&workspace)?;
                let query_vec = engine.embed_text(&query)?;
                let mut ranked: Vec<(usize, f32)> = Vec::new();
                let mut missing = 0usize;
                for (slice_index, slice) in files.chunks(MEDIA_BACKGROUND_IO_SLICE).enumerate() {
                    let io_request = media_io
                        .enqueue(root_identity.clone(), crate::media_io::WorkClass::Metadata);
                    let io_permit = loop {
                        if cancelled.load(Ordering::Acquire) {
                            io_request.cancel();
                            return Err("semantic query cancelled".to_string());
                        }
                        match io_request.try_acquire() {
                            Ok(Some(permit)) => break permit,
                            Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                            Err(error) => return Err(error.to_string()),
                        }
                    };
                    let io_started = std::time::Instant::now();
                    let source_offset = slice_index * MEDIA_BACKGROUND_IO_SLICE;
                    for (relative_index, path) in slice.iter().enumerate() {
                        if cancelled.load(Ordering::Acquire) {
                            media_io.record_filesystem_duration(
                                &root_identity,
                                crate::media_io::WorkClass::Metadata,
                                io_started.elapsed(),
                            );
                            io_permit.finish(crate::media_io::PermitOutcome::Cancelled);
                            return Err("semantic query cancelled".to_string());
                        }
                        if crate::media_explorer::is_video_path(path) {
                            continue;
                        }
                        let key = crate::media_db::canonical_key(&workspace, path);
                        let (mtime, size) = file_stat_pair(path);
                        match index.get(&key, mtime, size) {
                            Some(embedding) => ranked.push((
                                source_offset + relative_index,
                                crate::media_clip::cosine(&query_vec, &embedding),
                            )),
                            None => missing += 1,
                        }
                    }
                    media_io.record_filesystem_duration(
                        &root_identity,
                        crate::media_io::WorkClass::Metadata,
                        io_started.elapsed(),
                    );
                    io_permit.finish(crate::media_io::PermitOutcome::Success);
                }
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                Ok((
                    ranked.into_iter().map(|(index, _)| index).collect(),
                    missing,
                ))
            })();
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let (indices, missing, error) = match result {
                Ok((indices, missing)) => (indices, missing, None),
                Err(err) => (Vec::new(), 0, Some(err)),
            };
            let _ = tx.send(CompareWorkEvent::ClipQueryDone {
                key: request_key,
                indices,
                missing,
                error,
            });
        });
    }

    /// Load persisted bindings (or defaults) from the media DB.
    fn load_media_bindings(&mut self) {
        if let Some(raw) = self
            .media_db
            .setting(crate::media_input::BINDINGS_SETTING_KEY)
        {
            self.media_bindings = crate::media_input::BindingTable::from_json(&raw);
            // Persist one-time default migrations immediately so every later
            // model/operator session observes the same shortcut contract.
            let migrated = self.media_bindings.to_json();
            if migrated != raw {
                let _ = self
                    .media_db
                    .set_setting(crate::media_input::BINDINGS_SETTING_KEY, &migrated);
            }
        } else {
            self.media_bindings = crate::media_input::BindingTable::default();
        }
    }

    /// Persist bindings immediately (rebinds are rare, commits cheap).
    fn save_media_bindings(&mut self) {
        if let Err(err) = self.media_db.set_setting(
            crate::media_input::BINDINGS_SETTING_KEY,
            &self.media_bindings.to_json(),
        ) {
            self.compare_action_message = format!("bindings not saved: {err}");
        }
    }

    /// Upload up to 8 finished thumbnails per frame into the texture LRU.
    fn drain_thumbnails(&mut self, ctx: &egui::Context) {
        let Some(engine) = self.thumb_engine.as_mut() else {
            return;
        };
        let mut uploads: Vec<(crate::media_thumbs::ThumbKey, egui::TextureHandle)> = Vec::new();
        engine.drain_ready(8, |pixels| {
            let image =
                ColorImage::from_rgba_unmultiplied([pixels.width, pixels.height], &pixels.rgba);
            let texture = ctx.load_texture(
                format!("thumb:{}:{}", pixels.key.edge, pixels.key.path),
                image,
                TextureOptions::LINEAR,
            );
            uploads.push((pixels.key, texture));
        });
        for (key, texture) in uploads {
            // Evicted handles drop here, freeing their GPU memory.
            let _ = self.thumb_textures.insert(key, texture);
        }
    }

    /// Kick a background stat sweep when Modified/Size sort needs it.
    fn media_maybe_spawn_stat_sweep(&mut self, ctx: &egui::Context, lane_id: usize) {
        // WP-068: every stat-dependent key (Modified, Size, Created) needs the
        // sidecar; only Name can be ordered from the path alone.
        if !self.media_explorer.sort.needs_stat() {
            if let Some(cancel) = self.media_stat_cancel.take() {
                cancel.store(true, Ordering::Release);
                self.media_query_diagnostics.cancellations =
                    self.media_query_diagnostics.cancellations.saturating_add(1);
            }
            self.media_stat_request = None;
            self.media_explorer.stats_loading = false;
            return;
        }
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let lane = &self.compare_lanes[pos];
        if lane.scanning && !lane.scan_using_cached_inventory {
            return;
        }
        let files = &lane.files;
        if files.is_empty() {
            return;
        }
        let key = MediaStatRequestKey {
            lane_id,
            scan_id: lane.scan_id,
            content_generation: self.media_content_generation,
            inventory_generation: lane.inventory_generation,
            folder: sanitize_folder_input(&lane.folder),
        };
        if self.media_stat_complete_key.as_ref() == Some(&key) {
            return;
        }
        if self.media_explorer.stats_loading && self.media_stat_request.as_ref() == Some(&key) {
            return;
        }
        if let Some(cancel) = self.media_stat_cancel.take() {
            cancel.store(true, Ordering::Release);
            self.media_query_diagnostics.cancellations =
                self.media_query_diagnostics.cancellations.saturating_add(1);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.media_stat_cancel = Some(Arc::clone(&cancelled));
        self.media_stat_request = Some(key.clone());
        self.media_explorer.stats_loading = true;
        let files = Arc::clone(files);
        let root_identity = self.media_root_identity.clone().unwrap_or_else(|| {
            crate::media_io::RootIdentity::new(
                key.folder.clone(),
                0,
                crate::media_io::RootKind::Unknown,
            )
        });
        let media_io = Arc::clone(&self.media_io);
        let tx = self.compare_work_tx.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stats = std::collections::HashMap::new();
            let mut failures = 0usize;
            for slice in files.chunks(MEDIA_BACKGROUND_IO_SLICE) {
                let io_request =
                    media_io.enqueue(root_identity.clone(), crate::media_io::WorkClass::Metadata);
                let io_permit = loop {
                    if cancelled.load(Ordering::Acquire) {
                        io_request.cancel();
                        return;
                    }
                    match io_request.try_acquire() {
                        Ok(Some(permit)) => break permit,
                        Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                        Err(_) => return,
                    }
                };
                let slice_started = std::time::Instant::now();
                let failures_before_slice = failures;
                for path in slice {
                    if cancelled.load(Ordering::Acquire) {
                        media_io.record_filesystem_duration(
                            &root_identity,
                            crate::media_io::WorkClass::Metadata,
                            slice_started.elapsed(),
                        );
                        io_permit.finish(crate::media_io::PermitOutcome::Cancelled);
                        return;
                    }
                    // Unreadable/deleted files get a default row: EVERY listed
                    // file must land in the map or the missing-entry guard
                    // respawns a full sweep every completion, forever (review B5).
                    let stat = match std::fs::metadata(path) {
                        Ok(meta) => crate::media_explorer::FileStat::Known {
                            mtime: meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs()),
                            size: meta.len(),
                            // WP-068: same metadata call, no extra filesystem
                            // round trip. `created` is unsupported on some
                            // volumes and policies; None sorts last.
                            created: meta
                                .created()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs()),
                        },
                        Err(error) => {
                            failures += 1;
                            crate::media_explorer::FileStat::Error(match error.kind() {
                                std::io::ErrorKind::NotFound => {
                                    crate::media_explorer::StatFailure::NotFound
                                }
                                std::io::ErrorKind::PermissionDenied => {
                                    crate::media_explorer::StatFailure::PermissionDenied
                                }
                                std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::NotConnected
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset => {
                                    crate::media_explorer::StatFailure::Unavailable
                                }
                                _ => crate::media_explorer::StatFailure::Other,
                            })
                        }
                    };
                    stats.insert(path.clone(), stat);
                }
                media_io.record_filesystem_duration(
                    &root_identity,
                    crate::media_io::WorkClass::Metadata,
                    slice_started.elapsed(),
                );
                io_permit.finish(if failures == failures_before_slice {
                    crate::media_io::PermitOutcome::Success
                } else {
                    crate::media_io::PermitOutcome::Error
                });
            }
            if !cancelled.load(Ordering::Acquire) {
                let _ = tx.send(CompareWorkEvent::MediaStatsDone {
                    key,
                    stats: Arc::new(stats),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    failures,
                });
                repaint.request_repaint();
            }
        });
    }

    /// Mark layout settings dirty for the debounced flush cycle.
    fn touch_media_settings(&mut self) {
        self.media_explorer.settings_dirty = true;
        self.media_meta_last_edit = Some(std::time::Instant::now());
    }

    /// Capture the unobscured viewport before opening unified Settings. The
    /// screenshot reply is asynchronous and consumed at the start of a later
    /// frame so the modal can never appear inside its own blurred backdrop.
    fn request_media_settings(&mut self, ctx: &egui::Context, category: u8) {
        // Refresh usage once per opening, never from the immediate-mode label
        // manager frame. This keeps a 50k-row catalog scan out of painting.
        self.media_label_usage_counts = self.media_db.color_label_usage_counts();
        // Every fresh open starts in compact mode. Couch fullscreen is an
        // explicit, transient choice made from inside Settings.
        self.media_explorer.settings_couch_fullscreen = false;
        self.media_explorer.settings_couch_prior_fullscreen = self.media_explorer.chrome_hidden;
        self.media_explorer.settings_category = category.min(3);
        self.media_explorer.show_settings = false;
        self.media_explorer.show_favorites = false;
        self.close_media_folder_navigator();
        self.settings_backdrop = None;
        if self.pending_model_snapshot.is_some() {
            // Screenshot replies carry no request ID. Never overlap the
            // Settings backdrop request with a receipt-backed model capture;
            // use the existing neutral Settings fallback for this rare race.
            self.settings_backdrop_requested_at = None;
            self.media_explorer.show_settings = true;
            ctx.request_repaint();
            return;
        }
        self.settings_backdrop_requested_at = Some(std::time::Instant::now());
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        ctx.request_repaint();
    }

    fn enter_settings_couch_fullscreen(&mut self, ctx: &egui::Context) {
        if self.media_explorer.settings_couch_fullscreen {
            return;
        }
        self.media_explorer.enter_settings_couch();
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        ctx.request_repaint();
    }

    fn exit_settings_couch_fullscreen(&mut self, ctx: &egui::Context) {
        let Some(restore) = self.media_explorer.exit_settings_couch() else {
            return;
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(restore));
        ctx.request_repaint();
    }

    fn handle_model_snapshot_capture(&mut self, ctx: &egui::Context) {
        let request_started = self
            .pending_model_snapshot
            .as_ref()
            .is_some_and(|pending| pending.requested_at.is_some());
        let settings_capture_pending = self.settings_backdrop_requested_at.is_some();
        let folder_navigator_capture_pending =
            self.folder_navigator_backdrop_requested_at.is_some();
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });

        if model_snapshot_owns_screenshot(
            request_started,
            settings_capture_pending,
            folder_navigator_capture_pending,
        ) {
            if let Some(frame) = screenshot {
                if let Some(pending) = self.pending_model_snapshot.take() {
                    let result = self.write_model_snapshot(&pending.path, &frame);
                    self.finish_model_snapshot(pending, result);
                }
                return;
            }
        }

        if self.pending_model_snapshot.is_none() {
            return;
        }

        let timed_out = self
            .pending_model_snapshot
            .as_ref()
            .and_then(|pending| pending.requested_at)
            .is_some_and(|started| started.elapsed() >= std::time::Duration::from_secs(5));
        if timed_out {
            if let Some(pending) = self.pending_model_snapshot.take() {
                self.finish_model_snapshot(
                    pending,
                    Err(
                        "renderer did not return the requested live screenshot within 5 seconds"
                            .to_string(),
                    ),
                );
            }
            return;
        }

        // A Settings screenshot already owns the sole unlabelled renderer
        // reply. Wait until it completes or times out before issuing ours.
        if settings_capture_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
            return;
        }

        if let Some(pending) = self.pending_model_snapshot.as_mut() {
            if pending.requested_at.is_none() {
                pending.requested_at = Some(std::time::Instant::now());
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn write_model_snapshot(
        &mut self,
        path: &Path,
        frame: &ColorImage,
    ) -> Result<serde_json::Value, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create live snapshot directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let width = u32::try_from(frame.size[0]).map_err(|_| "snapshot width overflow")?;
        let height = u32::try_from(frame.size[1]).map_err(|_| "snapshot height overflow")?;
        let mut rgba = image::RgbaImage::new(width, height);
        for (target, source) in rgba.pixels_mut().zip(frame.pixels.iter()) {
            *target = image::Rgba(source.to_array());
        }

        let surface = self.video_player.diagnostics().surface;
        let surface_owner = self.media_video_surface_owner();
        let mut video_capture_path = None;
        let mut video_capture_source = None;
        let mut video_composited = false;
        let mut video_capture_error = None;
        let mut framebuffer_rgb_range = None;
        if self.video_player.active_path().is_some() {
            match current_surface_capture_region(&surface, [width, height]) {
                Ok(Some(region)) => {
                    let [_x, _y, target_width, target_height] = region.full;
                    let [left, top, fit_width, fit_height] = region.visible;
                    let [source_x, source_y] = region.source_offset;
                    let stem = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("live-ui");
                    let sidecar = path.with_file_name(format!("{stem}-video.png"));
                    match self.video_player.capture_frame(&sidecar) {
                        Ok(()) => match image::open(&sidecar) {
                            Ok(decoded) => {
                                let fitted = decoded
                                    .resize_exact(
                                        target_width as u32,
                                        target_height as u32,
                                        image::imageops::FilterType::Triangle,
                                    )
                                    .to_rgba8();
                                let visible = image::imageops::crop_imm(
                                    &fitted, source_x, source_y, fit_width, fit_height,
                                )
                                .to_image();
                                image::imageops::overlay(
                                    &mut rgba,
                                    &visible,
                                    i64::from(left),
                                    i64::from(top),
                                );
                                video_composited = true;
                                video_capture_path = Some(sidecar.to_string_lossy().to_string());
                                video_capture_source = Some("libvlc");
                            }
                            Err(error) => {
                                video_capture_error = Some(format!(
                                    "decode video snapshot {}: {error}",
                                    sidecar.display()
                                ));
                            }
                        },
                        Err(error) => video_capture_error = Some(error),
                    }

                    // Some LibVLC vouts reject `video_take_snapshot` even
                    // though their GDI child is visibly present in eframe's
                    // returned live framebuffer. Preserve that exact visible
                    // region as the independent sidecar instead of losing the
                    // model-safe proof path or falling back to desktop capture.
                    if video_capture_path.is_none() {
                        let visible =
                            image::imageops::crop_imm(&rgba, left, top, fit_width, fit_height)
                                .to_image();
                        let mut minimum = u8::MAX;
                        let mut maximum = u8::MIN;
                        for pixel in visible.pixels() {
                            for channel in &pixel.0[..3] {
                                minimum = minimum.min(*channel);
                                maximum = maximum.max(*channel);
                            }
                        }
                        framebuffer_rgb_range = Some(maximum.saturating_sub(minimum));
                        match visible.save(&sidecar) {
                            Ok(()) => {
                                video_capture_path = Some(sidecar.to_string_lossy().to_string());
                                video_capture_source = Some("live_framebuffer_crop");
                            }
                            Err(error) => {
                                video_capture_error = Some(format!(
                                    "{}; save visible video crop {}: {error}",
                                    video_capture_error
                                        .as_deref()
                                        .unwrap_or("LibVLC snapshot unavailable"),
                                    sidecar.display()
                                ));
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => video_capture_error = Some(error),
            }
        }

        rgba.save(path)
            .map_err(|error| format!("save live UI snapshot {}: {error}", path.display()))?;
        Ok(serde_json::json!({
            "capture_path": path.to_string_lossy(),
            "capture_exists": path.metadata().is_ok_and(|metadata| metadata.len() > 0),
            "width_px": width,
            "height_px": height,
            "foreground_activation": false,
            "video_composited": video_composited,
            "video_capture_path": video_capture_path,
            "video_capture_source": video_capture_source,
            "video_capture_error": video_capture_error,
            "framebuffer_rgb_range": framebuffer_rgb_range,
            "surface_owner": surface_owner,
            "surface": surface,
        }))
    }

    fn finish_model_snapshot(
        &mut self,
        pending: PendingModelSnapshot,
        result: Result<serde_json::Value, String>,
    ) {
        let applied = result.is_ok();
        let message = result
            .as_ref()
            .map(|_| format!("live UI snapshot saved to {}", pending.path.display()))
            .unwrap_or_else(|error| error.clone());
        let now = chrono::Utc::now().to_rfc3339();
        let receipt = api::Receipt {
            action_id: pending.command.action_id.clone(),
            kind: pending.command.command.id_str().to_string(),
            status: if applied {
                api::ActionStatus::Applied
            } else {
                api::ActionStatus::Rejected
            },
            actor: pending.command.actor.clone(),
            protocol_version: pending.command.protocol_version,
            started_at: now.clone(),
            finished_at: now,
            result: result.unwrap_or_else(|_| serde_json::Value::Null),
            error: (!applied).then(|| message.clone()),
            note: Some(message.clone()),
        };
        let state =
            serde_json::to_value(self.current_state_snapshot()).unwrap_or(serde_json::Value::Null);
        let persistence_error = match self.service.lock() {
            Ok(mut service) => {
                let result = api::mark_intent_applied(&mut service, &self.api_paths, &receipt);
                service.record_applied_action(
                    &pending.command.action_id,
                    pending.command.command.id_str(),
                    applied,
                    &message,
                    state,
                );
                result.err().map(|error| error.to_string())
            }
            Err(_) => Some("service lock unavailable while finalizing UI snapshot".to_string()),
        };
        self.last_applied_action = Some(match persistence_error.as_deref() {
            Some(error) => {
                eprintln!(
                    "UI snapshot intent {} finalization failed: {error}",
                    pending.command.action_id
                );
                format!(
                    "{} intent=ui_snapshot applied={} persistence_error={} :: {}",
                    pending.command.action_id, applied, error, message
                )
            }
            None => format!(
                "{} intent=ui_snapshot applied={} :: {}",
                pending.command.action_id, applied, message
            ),
        });
        self.last_receipt = serde_json::to_string_pretty(&receipt).ok();
    }

    fn handle_settings_backdrop_capture(&mut self, ctx: &egui::Context) {
        let Some(requested_at) = self.settings_backdrop_requested_at else {
            return;
        };
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = screenshot {
            let blurred = gaussian_settings_backdrop(&image, 640);
            self.settings_backdrop = Some(ctx.load_texture(
                "settings-gaussian-backdrop",
                blurred,
                TextureOptions::LINEAR,
            ));
            self.settings_backdrop_requested_at = None;
            self.media_explorer.show_settings = true;
            ctx.request_repaint();
        } else if requested_at.elapsed() >= std::time::Duration::from_millis(500) {
            // Headless or alternate renderers may not implement screenshot
            // replies. Never leave Settings stuck waiting on that capability.
            self.settings_backdrop_requested_at = None;
            self.media_explorer.show_settings = true;
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn handle_folder_navigator_backdrop_capture(&mut self, ctx: &egui::Context) {
        let Some(requested_at) = self.folder_navigator_backdrop_requested_at else {
            return;
        };
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = screenshot {
            let blurred = gaussian_settings_backdrop(&image, 640);
            self.folder_navigator_backdrop = Some(ctx.load_texture(
                "folder-navigator-gaussian-backdrop",
                blurred,
                TextureOptions::LINEAR,
            ));
            self.folder_navigator_backdrop_requested_at = None;
            let lane_id = self.compare_lanes.first().map(|lane| lane.id).unwrap_or(0);
            self.open_media_folder_navigator_without_capture(lane_id);
            ctx.request_repaint();
        } else if requested_at.elapsed() >= std::time::Duration::from_millis(500) {
            // Preserve operability on headless/alternate renderers exactly as
            // Settings does: open over the shared neutral fallback.
            self.folder_navigator_backdrop_requested_at = None;
            let lane_id = self.compare_lanes.first().map(|lane| lane.id).unwrap_or(0);
            self.open_media_folder_navigator_without_capture(lane_id);
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    /// Close Settings through its existing live-save path. A failed settings
    /// commit leaves the modal open and dirty so the operator can see the
    /// retryable error instead of receiving a false Saved state (WP-055).
    fn close_media_settings(&mut self, ctx: &egui::Context) {
        self.exit_settings_couch_fullscreen(ctx);
        let _ = self.flush_media_metadata(true);
        if self.media_explorer.settings_dirty {
            self.media_explorer.show_settings = true;
        } else {
            self.media_explorer.show_settings = false;
            self.settings_backdrop = None;
            self.settings_backdrop_requested_at = None;
        }
    }

    /// App paths are deliberately staged behind their existing Set buttons;
    /// closing Settings retains these drafts in memory but does not apply them.
    fn app_path_draft_staged(&self) -> bool {
        let configured_workspace = self.config.workspace_root.to_string_lossy();
        let configured_copy = self
            .config
            .copy_location
            .as_ref()
            .map(|path| path.to_string_lossy())
            .unwrap_or_default();
        self.workspace_root.trim() != configured_workspace
            || self.copy_location.trim() != configured_copy
    }

    /// Hydrate the in-memory metadata cache from the workspace media DB
    /// (WP-042). Cache keys are CANONICAL DB KEYS. Called at startup and
    /// after a workspace switch.
    fn load_media_metadata(&mut self) {
        self.media_label_definitions = self.media_db.color_label_definitions();
        self.refresh_media_label_colors();
        self.media_label_usage_counts = self.media_db.color_label_usage_counts();
        self.media_notes = Arc::new(BTreeMap::new());
        self.media_tags = Arc::new(BTreeMap::new());
        self.media_color_labels = Arc::new(BTreeMap::new());
        self.media_favorites.clear();
        self.media_favorite_keys.clear();
        for (key, meta) in self.media_db.list_meta_by_key(None, None) {
            if !meta.notes.is_empty() {
                Arc::make_mut(&mut self.media_notes).insert(key.clone(), meta.notes);
            }
            if !meta.tags.is_empty() {
                Arc::make_mut(&mut self.media_tags).insert(key.clone(), meta.tags);
            }
            if !meta.labels.is_empty() {
                Arc::make_mut(&mut self.media_color_labels).insert(key.clone(), meta.labels);
            }
        }
        self.media_favorites = self.media_db.favorites_keyed();
        self.media_favorite_keys = self
            .media_favorites
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        self.media_dirty_meta.clear();
        self.media_meta_last_edit = None;
    }

    /// Canonical cache key for a scan-produced path (separator/casing safe).
    fn media_key(&self, path: &str) -> String {
        self.media_db.key_for(path)
    }

    fn cache_active_media_tab_inventory(&mut self) {
        let Some(lane) = self.compare_lanes.first() else {
            return;
        };
        // WP-064: cache whenever rows exist. Requiring a committed
        // `inventory_generation` made the cache miss for every folder whose scan
        // was interrupted or hit a single unreadable subdirectory, so switching
        // back to those tabs always paid a cold rescan. A generation-less
        // inventory is still a restorable viewport; reconciliation corrects it.
        if lane.files.is_empty() {
            return;
        }
        let id = self.media_tabs.active_id().as_str().to_string();
        // Only retain a display order that is valid for this exact row vector.
        let file_count = lane.files.len();
        let display = if self
            .media_display_cache
            .iter()
            .all(|index| *index < file_count)
        {
            Arc::clone(&self.media_display_cache)
        } else {
            Arc::new(Vec::new())
        };
        self.media_tab_runtime_inventories.insert(
            id.clone(),
            MediaTabRuntimeInventory {
                files: Arc::clone(&lane.files),
                inventory_generation: lane.inventory_generation,
                display,
            },
        );
        self.media_tab_runtime_inventory_lru
            .retain(|candidate| candidate != &id);
        self.media_tab_runtime_inventory_lru.push_back(id);

        loop {
            let total_items = self
                .media_tab_runtime_inventories
                .values()
                .map(|inventory| inventory.files.len())
                .sum::<usize>();
            let over_limit = self.media_tab_runtime_inventories.len()
                > MAX_MEDIA_TAB_RUNTIME_INVENTORIES
                || total_items > MAX_MEDIA_TAB_RUNTIME_ITEMS;
            if !over_limit || self.media_tab_runtime_inventories.len() <= 1 {
                break;
            }
            let Some(evicted) = self.media_tab_runtime_inventory_lru.pop_front() else {
                break;
            };
            self.media_tab_runtime_inventories.remove(&evicted);
        }
    }

    fn snapshot_active_media_tab(&mut self) {
        let Some(lane) = self.compare_lanes.first() else {
            return;
        };
        let selected_key = lane
            .files
            .get(lane.index)
            .map(|path| self.media_db.key_for(path));
        let mut selected_keys = lane
            .selected_files
            .iter()
            .filter_map(|index| lane.files.get(*index))
            .map(|path| self.media_db.key_for(path))
            .collect::<Vec<_>>();
        selected_keys.sort();
        selected_keys.dedup();
        let viewport = &mut self.media_tabs.active_mut().viewport;
        // WP-067: a collection tab has no folder; leave its kind/sub-view
        // untouched and never overwrite its folder key with an empty lane path.
        if viewport.kind == crate::media_tabs::MediaTabKind::Collection {
            viewport.cursor_key = self
                .media_explorer
                .cursor
                .and_then(|index| lane.files.get(index))
                .map(|path| self.media_db.key_for(path));
            viewport.selected_key = selected_key;
            viewport.selected_keys = selected_keys;
            return;
        }
        viewport.folder_key = if lane.folder.trim().is_empty() {
            String::new()
        } else {
            self.media_db.key_for(&lane.folder)
        };
        viewport.cursor_key = self
            .media_explorer
            .cursor
            .and_then(|index| lane.files.get(index))
            .map(|path| self.media_db.key_for(path));
        viewport.selected_key = selected_key;
        viewport.selected_keys = selected_keys;
        viewport.recursive = lane.recursive;
        viewport.filter = match lane.media_filter {
            MediaFilterMode::All => MediaTabFilter::All,
            MediaFilterMode::ImagesOnly => MediaTabFilter::Images,
            MediaFilterMode::VideosOnly => MediaTabFilter::Videos,
        };
        viewport.search_query = self.media_search_query.clone();
        viewport.query_mode = match self.media_search_mode {
            1 => MediaTabQueryMode::Fuzzy,
            2 => MediaTabQueryMode::Semantic,
            _ => MediaTabQueryMode::Name,
        };
        viewport.view_mode = match self.media_explorer.view_mode {
            crate::media_explorer::MediaViewMode::TwoPanel => MediaTabViewMode::LibraryViewer,
            crate::media_explorer::MediaViewMode::FullGrid => MediaTabViewMode::FullGrid,
        };
        viewport.split_ratio = self.media_explorer.split_ratio;
        viewport.tile_edge = self.media_explorer.tile_edge;
        viewport.show_names = self.media_explorer.show_names;
        viewport.strip_height = self.media_explorer.strip_height;
        viewport.sort = match self.media_explorer.sort {
            crate::media_explorer::MediaSort::Name => MediaTabSort::Name,
            crate::media_explorer::MediaSort::Modified => MediaTabSort::Modified,
            crate::media_explorer::MediaSort::Size => MediaTabSort::Size,
            crate::media_explorer::MediaSort::Created => MediaTabSort::Created,
        };
        viewport.sort_descending = self.media_explorer.sort_desc;
        viewport.library_scroll_top = self.media_explorer.last_scroll_top;
        viewport.folder_navigator_key = if self
            .media_explorer
            .folder_navigator_location
            .trim()
            .is_empty()
        {
            String::new()
        } else {
            self.media_db
                .key_for(&self.media_explorer.folder_navigator_location)
        };
        viewport.folder_location_input = self.media_explorer.folder_location_input.clone();
        viewport.search_folder_only = self.media_search_folder_only;
        self.cache_active_media_tab_inventory();
    }

    fn persist_media_tabs_state(&self, state: &MediaTabsState) -> Result<(), String> {
        if self.media_tabs_persistence_blocked {
            return Err(
                "tab persistence is disabled because the rejected session could not be copied to the recovery key"
                    .to_string(),
            );
        }
        let encoded = state.encode()?;
        self.media_db.set_setting(MEDIA_TABS_SETTING_KEY, &encoded)
    }

    fn write_media_tabs(&mut self) -> Result<(), String> {
        self.persist_media_tabs_state(&self.media_tabs)
    }

    fn cancel_active_media_runtime(&mut self) {
        if let Some(lane) = self.compare_lanes.first_mut() {
            lane.scan_id = lane.scan_id.saturating_add(1);
            lane.load_id = lane.load_id.saturating_add(1);
            lane.scanning = false;
            lane.loading_image = false;
            lane.loading_image_inflight = false;
        }
        if let Some(lane_id) = self.compare_lanes.first().map(|lane| lane.id) {
            if let Some(cancel) = self.compare_scan_cancellations.remove(&lane_id) {
                cancel.store(true, Ordering::Release);
            }
        }
        self.media_search_requests.cancel_current();
        for cancel in [
            self.media_search_index_cancel.take(),
            self.media_suggestion_cancel.take(),
            self.media_stat_cancel.take(),
            self.clip_index_cancel.take(),
            self.clip_query_cancel.take(),
        ]
        .into_iter()
        .flatten()
        {
            cancel.store(true, Ordering::Release);
        }
        for cancel in self.media_child_folder_cancel.values() {
            cancel.store(true, Ordering::Release);
        }
        self.media_child_folder_cancel.clear();
        self.media_child_folder_inflight.clear();
        self.video_player.stop();
        self.media_inline_video_path = None;
        self.media_inline_video_requested_at = None;
        self.media_inline_video_pending_target = None;
        self.media_playback_lease = None;
    }

    fn materialize_active_media_tab(&mut self) {
        self.cancel_active_media_runtime();
        let viewport = self.media_tabs.active().viewport.clone();
        let viewport_kind = viewport.kind;
        let runtime_inventory = self
            .media_tab_runtime_inventories
            .get(self.media_tabs.active_id().as_str())
            .cloned();
        let folder = if viewport.folder_key.is_empty() {
            String::new()
        } else {
            self.media_db.path_for_key(&viewport.folder_key)
        };
        let lane_id = if self.compare_lanes.is_empty() {
            self.compare_lanes.push(CompareLane::new(0));
            0
        } else {
            self.compare_lanes[0].id
        };
        let lane = &mut self.compare_lanes[0];
        lane.folder = folder.clone();
        lane.name = crate::media_tabs::folder_tab_title(&folder);
        lane.files = runtime_inventory
            .as_ref()
            .map(|inventory| Arc::clone(&inventory.files))
            .unwrap_or_else(|| Arc::new(Vec::new()));
        lane.inventory_generation = runtime_inventory
            .as_ref()
            .and_then(|inventory| inventory.inventory_generation);
        lane.index = 0;
        lane.image_path.clear();
        lane.pending_image_index = None;
        lane.selected_files.clear();
        lane.selection_anchor = None;
        lane.texture = None;
        lane.texture_size = None;
        lane.recursive = viewport.recursive;
        lane.media_filter = match viewport.filter {
            MediaTabFilter::All => MediaFilterMode::All,
            MediaTabFilter::Images => MediaFilterMode::ImagesOnly,
            MediaTabFilter::Videos => MediaFilterMode::VideosOnly,
        };
        self.media_search_query = viewport.search_query;
        self.media_search_mode = match viewport.query_mode {
            MediaTabQueryMode::Name => 0,
            MediaTabQueryMode::Fuzzy => 1,
            MediaTabQueryMode::Semantic => 2,
        };
        self.media_explorer.view_mode = match viewport.view_mode {
            MediaTabViewMode::LibraryViewer => crate::media_explorer::MediaViewMode::TwoPanel,
            MediaTabViewMode::FullGrid => crate::media_explorer::MediaViewMode::FullGrid,
        };
        self.media_explorer.split_ratio = viewport.split_ratio;
        self.media_explorer.tile_edge = viewport.tile_edge;
        self.media_explorer.show_names = viewport.show_names;
        self.media_explorer.strip_height = viewport.strip_height;
        self.media_explorer.sort = match viewport.sort {
            MediaTabSort::Name => crate::media_explorer::MediaSort::Name,
            MediaTabSort::Modified => crate::media_explorer::MediaSort::Modified,
            MediaTabSort::Size => crate::media_explorer::MediaSort::Size,
            MediaTabSort::Created => crate::media_explorer::MediaSort::Created,
        };
        self.media_explorer.sort_desc = viewport.sort_descending;
        self.media_explorer.last_scroll_top = viewport.library_scroll_top;
        self.media_explorer.folder_navigator_location = if viewport.folder_navigator_key.is_empty()
        {
            folder.clone()
        } else {
            self.media_db.path_for_key(&viewport.folder_navigator_key)
        };
        self.media_explorer.folder_location_input = viewport.folder_location_input;
        self.media_search_folder_only = viewport.search_folder_only;
        self.close_media_folder_navigator();
        self.media_explorer.cursor = None;
        self.media_tab_pending_cursor_key = viewport.cursor_key;
        self.media_tab_pending_selection_keys = viewport.selected_key.into_iter().collect();
        for key in viewport.selected_keys {
            if !self.media_tab_pending_selection_keys.contains(&key) {
                self.media_tab_pending_selection_keys.push(key);
            }
        }
        // WP-064: republish the tab's last display order so the Library grid
        // paints in this frame. The cache *key* stays `None`, so the normal
        // display worker still recomputes the authoritative order and swaps it
        // in; this only removes the blank viewport during that round trip.
        self.media_display_cache = runtime_inventory
            .as_ref()
            .map(|inventory| Arc::clone(&inventory.display))
            .unwrap_or_else(|| Arc::new(Vec::new()));
        self.media_display_cache_key = None;
        self.media_search_index = None;
        self.media_semantic = None;
        self.media_scan_diagnostics = MediaScanDiagnostics::default();
        self.media_query_diagnostics = MediaQueryDiagnostics::default();
        // WP-067: a collection tab is not a folder. It must never reach the
        // scan path — its rows come from the metadata cache and publish in this
        // frame, so activation performs no filesystem enumeration at all.
        if viewport_kind == crate::media_tabs::MediaTabKind::Collection {
            self.materialize_media_collection_tab(lane_id);
            return;
        }
        if !folder.is_empty() {
            let has_runtime_inventory = runtime_inventory.is_some();
            self.start_compare_scan_internal(lane_id, has_runtime_inventory);
            if has_runtime_inventory {
                self.restore_media_tab_selection(lane_id);
                if self
                    .compare_lane_position(lane_id)
                    .is_some_and(|pos| self.compare_lanes[pos].pending_image_index.is_some())
                {
                    self.start_compare_image_load(lane_id);
                }
            }
        }
    }

    fn activate_media_tab(&mut self, id: &str) -> Result<(), String> {
        if self.media_tabs.active_id().as_str() == id {
            return Ok(());
        }
        self.snapshot_active_media_tab();
        let mut candidate = self.media_tabs.clone();
        candidate.activate_by_str(id)?;
        self.persist_media_tabs_state(&candidate)?;
        self.media_tabs = candidate;
        self.materialize_active_media_tab();
        Ok(())
    }

    fn open_media_folder_in_new_tab(&mut self, path: &str) -> Result<String, String> {
        let path = sanitize_folder_input(path);
        if path.is_empty() {
            return Err("folder path is empty".to_string());
        }
        self.snapshot_active_media_tab();
        let key = self.media_db.key_for(&path);
        let mut candidate = self.media_tabs.clone();
        let id = candidate.open_folder_in_new_tab(key)?;
        self.persist_media_tabs_state(&candidate)?;
        self.media_tabs = candidate;
        self.materialize_active_media_tab();
        Ok(id.as_str().to_string())
    }

    fn close_media_tab(&mut self, id: &str) -> Result<String, String> {
        self.snapshot_active_media_tab();
        let was_active = self.media_tabs.active_id().as_str() == id;
        let mut candidate = self.media_tabs.clone();
        let active = candidate.close_by_str(id)?;
        self.persist_media_tabs_state(&candidate)?;
        self.media_tabs = candidate;
        self.media_tab_runtime_inventories.remove(id);
        self.media_tab_runtime_inventory_lru
            .retain(|candidate| candidate != id);
        if was_active {
            self.materialize_active_media_tab();
        }
        Ok(active.as_str().to_string())
    }

    fn restore_media_tab_selection(&mut self, lane_id: usize) {
        if self.media_tab_pending_selection_keys.is_empty()
            && self.media_tab_pending_cursor_key.is_none()
        {
            return;
        }
        let wanted = self
            .media_tab_pending_selection_keys
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let primary = self.media_tab_pending_selection_keys.first().cloned();
        let cursor = self.media_tab_pending_cursor_key.clone();
        let keys = self
            .compare_lane_position(lane_id)
            .map(|pos| {
                self.compare_lanes[pos]
                    .files
                    .iter()
                    .map(|path| self.media_db.key_for(path))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(pos) = self.compare_lane_position(lane_id) {
            let lane = &mut self.compare_lanes[pos];
            lane.selected_files = keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| wanted.contains(key).then_some(index))
                .collect();
            if let Some(index) = primary.and_then(|key| keys.iter().position(|item| item == &key)) {
                lane.index = index;
                lane.image_path = lane.files[index].clone();
                lane.pending_image_index = Some(index);
                lane.loading_image = true;
            }
        }
        self.media_explorer.cursor =
            cursor.and_then(|key| keys.iter().position(|item| item == &key));
        self.media_tab_pending_cursor_key = None;
        self.media_tab_pending_selection_keys.clear();
    }

    /// Reopen the media DB against the current workspace root (after a
    /// workspace switch), recreate the thumbnail engine for the new cache
    /// root, and rehydrate caches + layout settings.
    fn reopen_media_db(&mut self, ctx: &egui::Context) {
        // The sole caller persists the old workspace before mutating service
        // and config state. Never write the old DB after that transaction
        // boundary: a failed late write would lose the retryable dirty set
        // when the new workspace cache is hydrated.
        self.media_db = MediaDb::open(&self.config.workspace_root);
        self.load_media_metadata();
        self.load_media_bindings();
        self.media_explorer = crate::media_explorer::MediaExplorerState::load(&self.media_db);
        let (media_tabs, load_status, persistence_blocked) =
            load_media_tabs_with_recovery(&self.media_db);
        self.media_tabs = media_tabs;
        self.media_tabs_load_status = load_status;
        self.media_tabs_persistence_blocked = persistence_blocked;
        self.media_tab_runtime_inventories.clear();
        self.media_tab_runtime_inventory_lru.clear();
        let _ = self.video_player.set_loop(self.media_explorer.video_loop);
        let repaint_ctx = ctx.clone();
        self.thumb_engine = Some(if let Some(identity) = self.media_root_identity.clone() {
            let source_root = self
                .compare_lanes
                .first()
                .map(|lane| sanitize_folder_input(&lane.folder))
                .unwrap_or_default();
            crate::media_thumbs::ThumbnailEngine::new_with_cache_cap_and_io(
                &self.config.workspace_root,
                self.config.media_thumb_cache_mb,
                Arc::clone(&self.media_io),
                identity,
                Path::new(&source_root),
                Box::new(move || repaint_ctx.request_repaint()),
            )
        } else {
            crate::media_thumbs::ThumbnailEngine::new_with_cache_cap(
                &self.config.workspace_root,
                self.config.media_thumb_cache_mb,
                Box::new(move || repaint_ctx.request_repaint()),
            )
        });
        let _ = self.thumb_textures.clear();
        self.materialize_active_media_tab();
    }

    /// Write dirty note/tag/label edits (and layout settings) through to the
    /// media DB. `force` flushes immediately (shutdown, workspace switch);
    /// otherwise edits debounce ~800ms so per-keystroke commits don't stall
    /// typing. Failed keys are RE-QUEUED so a transient write error never
    /// silently drops operator edits.
    fn flush_media_metadata(&mut self, force: bool) -> Result<(), String> {
        let has_meta = !self.media_dirty_meta.is_empty();
        let has_settings = self.media_explorer.settings_dirty;
        if !has_meta && !has_settings {
            return Ok(());
        }
        if !force {
            let settled = self
                .media_meta_last_edit
                .is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(800));
            if !settled {
                return Ok(());
            }
        }
        let keys: Vec<String> = self.media_dirty_meta.drain().collect();
        let mut failed: Vec<String> = Vec::new();
        let mut first_error: Option<String> = None;
        for key in keys {
            let notes = self.media_notes.get(&key).cloned().unwrap_or_default();
            let tags = self.media_tags.get(&key).cloned().unwrap_or_default();
            let labels = self
                .media_color_labels
                .get(&key)
                .cloned()
                .unwrap_or_default();
            if let Err(err) =
                self.media_db
                    .set_meta_labels(&key, Some(&notes), Some(&tags), Some(&labels))
            {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                failed.push(key);
            }
        }
        if self.media_explorer.settings_dirty {
            match self.media_explorer.save(&self.media_db) {
                Ok(()) => self.media_explorer.settings_dirty = false,
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        if let Some(err) = first_error {
            self.compare_action_message = format!("media metadata not saved: {err}");
            // Re-queue failures and back off to the next debounce window.
            for key in failed {
                self.media_dirty_meta.insert(key);
            }
            self.media_meta_last_edit = Some(std::time::Instant::now());
            Err(err)
        } else {
            self.media_meta_last_edit = None;
            Ok(())
        }
    }

    /// Mark a canonical key dirty for the debounced flush.
    fn touch_media_meta(&mut self, key: &str) {
        self.media_dirty_meta.insert(key.to_string());
        self.media_meta_last_edit = Some(std::time::Instant::now());
        self.media_meta_generation = self.media_meta_generation.wrapping_add(1);
    }

    /// Move a lane's index by `delta`, clamped to the lane's file range.
    fn nav_lane_relative(&mut self, lane_id: usize, delta: isize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let lane = &self.compare_lanes[pos];
        let total = lane.total();
        if total == 0 {
            return;
        }
        let target = wrap_relative_index(lane.index, delta, total);
        if target != lane.index || lane.texture.is_none() {
            self.request_compare_image(lane_id, target);
        }
    }

    fn compare_lane_selected_indices(&self, lane_id: usize) -> Vec<usize> {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return Vec::new();
        };
        let mut indices = self.compare_lanes[pos]
            .selected_files
            .iter()
            .copied()
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices
    }

    fn media_child_folders(&mut self, lane_id: usize, folder: &str) -> Arc<Vec<String>> {
        if let Some(cached) = self.media_child_folder_cache.get(folder) {
            return Arc::clone(cached);
        }
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return Arc::new(Vec::new());
        };
        let scan_id = self.compare_lanes[pos].scan_id;
        let already_inflight = self.media_child_folder_inflight.iter().any(|request| {
            request.lane_id == lane_id && request.scan_id == scan_id && request.folder == folder
        });
        if folder.is_empty() || already_inflight || self.media_child_folder_inflight.len() >= 2 {
            return Arc::new(Vec::new());
        }
        let requested_root = media_io_staged_root_identity_for_path(folder, scan_id);
        let key = MediaChildFolderRequestKey {
            lane_id,
            scan_id,
            root_identity: Some(requested_root),
            folder: folder.to_string(),
        };
        self.media_child_folder_inflight.insert(key.clone());
        let cancelled = Arc::new(AtomicBool::new(false));
        self.media_child_folder_cancel
            .insert(key.clone(), Arc::clone(&cancelled));

        let requested = folder.to_string();
        let root_identity = key.root_identity.clone().unwrap_or_else(|| {
            crate::media_io::RootIdentity::new(
                requested.clone(),
                0,
                crate::media_io::RootKind::Unknown,
            )
        });
        let media_io = Arc::clone(&self.media_io);
        let tx = self.compare_work_tx.clone();
        thread::spawn(move || {
            let io_request =
                media_io.enqueue(root_identity.clone(), crate::media_io::WorkClass::Visible);
            let io_permit = loop {
                if cancelled.load(Ordering::Acquire) {
                    io_request.cancel();
                    return;
                }
                match io_request.try_acquire() {
                    Ok(Some(permit)) => break permit,
                    Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            let started = std::time::Instant::now();
            let mut folders = Vec::new();
            let mut error = None;
            match fs::read_dir(Path::new(&requested)) {
                Ok(entries) => {
                    for entry in entries {
                        if cancelled.load(Ordering::Acquire) {
                            io_permit.finish(crate::media_io::PermitOutcome::Cancelled);
                            return;
                        }
                        let Ok(entry) = entry else {
                            continue;
                        };
                        // DirEntry::file_type reuses the Windows directory
                        // enumeration result instead of issuing one stat per
                        // child across SMB.
                        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                            if let Some(name) = entry.file_name().to_str() {
                                folders.push(name.to_string());
                            }
                        }
                    }
                    folders.sort_unstable();
                }
                Err(cause) => {
                    error = Some(format!("Could not list folders in {requested}: {cause}"));
                }
            }
            media_io.record_filesystem_duration(
                &root_identity,
                crate::media_io::WorkClass::Visible,
                started.elapsed(),
            );
            io_permit.finish(if error.is_none() {
                crate::media_io::PermitOutcome::Success
            } else {
                crate::media_io::PermitOutcome::Error
            });
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let _ = tx.send(CompareWorkEvent::MediaChildFoldersDone {
                prepared: Arc::new(crate::media_explorer::prepare_folder_entries(
                    &requested, &folders,
                )),
                key,
                folders: Arc::new(folders),
                error,
            });
        });
        Arc::new(Vec::new())
    }

    fn media_selected_path(&self, lane_id: usize) -> Option<String> {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return None;
        };
        let lane = &self.compare_lanes[pos];
        if lane.files.is_empty() {
            return None;
        }
        if let Some(index) = lane.selected_files.iter().min() {
            return lane.files.get(*index).cloned();
        }
        Some(lane.files[lane.index].clone())
    }

    fn media_video_capture_path(&self, output: Option<&str>, action_id: &str) -> PathBuf {
        let mut path = match output.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => {
                let candidate = PathBuf::from(value);
                if candidate.is_absolute() {
                    candidate
                } else {
                    self.config.workspace_root.join(candidate)
                }
            }
            None => self
                .config
                .workspace_root
                .join(".facial")
                .join("ui-snapshots")
                .join("live-video")
                .join(format!("{action_id}.png")),
        };
        if path
            .extension()
            .is_none_or(|extension| !extension.to_string_lossy().eq_ignore_ascii_case("png"))
        {
            path.set_extension("png");
        }
        path
    }

    fn media_video_surface_owner(&self) -> Option<String> {
        video_surface_owner(
            self.video_player.active_path(),
            self.media_inline_video_path.as_deref(),
        )
        .map(str::to_string)
    }

    fn ui_snapshot_path(&self, output: Option<&str>, action_id: &str) -> PathBuf {
        let mut path = match output.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => {
                let candidate = PathBuf::from(value);
                if candidate.is_absolute() {
                    candidate
                } else {
                    self.config.workspace_root.join(candidate)
                }
            }
            None => self
                .config
                .workspace_root
                .join(".facial")
                .join("ui-snapshots")
                .join("live-ui")
                .join(format!("{action_id}.png")),
        };
        if path
            .extension()
            .is_none_or(|extension| !extension.to_string_lossy().eq_ignore_ascii_case("png"))
        {
            path.set_extension("png");
        }
        path
    }

    /// Toggle a favorite with immediate write-through to the media DB;
    /// the caches mirror the DB afterwards (clicks are rare, commits cheap).
    fn media_toggle_favorite(&mut self, path: &str) {
        match self.media_db.toggle_favorite(path) {
            Ok(_) => {
                self.media_favorites = self.media_db.favorites_keyed();
                self.media_favorite_keys = self
                    .media_favorites
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect();
            }
            Err(err) => {
                self.compare_action_message = format!("favorite not saved: {err}");
            }
        }
    }

    fn refresh_media_label_colors(&mut self) {
        self.media_label_colors = build_media_label_color_cache(&self.media_label_definitions);
    }

    fn set_compare_lane_message(&mut self, lane_id: usize, message: String) {
        if let Some(pos) = self.compare_lane_position(lane_id) {
            self.compare_lanes[pos].action_message = message.clone();
        }
        self.compare_action_message = message;
    }

    fn compare_lane_select_all(&mut self, lane_id: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let mut message = String::new();
        {
            let lane = &mut self.compare_lanes[pos];
            lane.selected_files = (0..lane.total()).collect();
            if lane.selected_files.is_empty() {
                lane.selection_anchor = None;
                message = "No files to select".to_string();
            } else {
                lane.selection_anchor = Some(0);
                message = format!(
                    "Selected {} file{}",
                    lane.total(),
                    if lane.total() == 1 { "" } else { "s" }
                );
            }
        }
        self.set_compare_lane_message(lane_id, message);
    }

    fn compare_lane_select_none(&mut self, lane_id: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        {
            let lane = &mut self.compare_lanes[pos];
            lane.selected_files.clear();
            lane.selection_anchor = None;
        }
        self.set_compare_lane_message(lane_id, "Selection cleared".to_string());
    }

    fn compare_lane_invert_selection(&mut self, lane_id: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let total = self.compare_lanes[pos].total();
        let selected = self.compare_lanes[pos].selected_files.clone();
        let mut next = HashSet::new();
        for idx in 0..total {
            if !selected.contains(&idx) {
                next.insert(idx);
            }
        }
        let mut message = "No files available".to_string();
        {
            let lane = &mut self.compare_lanes[pos];
            lane.selected_files = next;
            if lane.total() > 0 {
                lane.selection_anchor = Some(lane.index);
                message = format!(
                    "Selection inverted ({} selected)",
                    lane.selected_files.len()
                );
            }
        }
        self.set_compare_lane_message(lane_id, message);
    }

    fn compare_lane_copy_selected(&mut self, lane_id: usize) {
        let indices = self.compare_lane_selected_indices(lane_id);
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let lane = &self.compare_lanes[pos];
        let paths: Vec<String> = indices
            .into_iter()
            .filter_map(|idx| lane.files.get(idx).cloned())
            .collect();

        if paths.is_empty() {
            self.set_compare_lane_message(lane_id, "No files selected to copy".to_string());
            return;
        }
        self.compare_clipboard = paths.clone();
        self.set_compare_lane_message(
            lane_id,
            format!(
                "Copied {} file{} to compare clipboard",
                paths.len(),
                if paths.len() == 1 { "" } else { "s" }
            ),
        );
    }

    /// Copy selected paths as text instead of adding files to Facial's internal
    /// file-operation clipboard. Portable paths are workspace-relative when
    /// possible; external/NAS media is relative to the currently selected
    /// browse folder so it remains independent of the drive letter.
    fn media_copy_path_text(&mut self, lane_id: usize, absolute: bool) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let lane = &self.compare_lanes[pos];
        let root = PathBuf::from(sanitize_folder_input(&lane.folder));
        let mut paths: Vec<String> = if lane.selected_files.is_empty() {
            lane.files.get(lane.index).cloned().into_iter().collect()
        } else {
            let mut selected: Vec<usize> = lane.selected_files.iter().copied().collect();
            selected.sort_unstable();
            selected
                .into_iter()
                .filter_map(|index| lane.files.get(index).cloned())
                .collect()
        };
        if paths.is_empty() && !root.as_os_str().is_empty() {
            paths.push(root.to_string_lossy().to_string());
        }
        if paths.is_empty() {
            self.set_compare_lane_message(lane_id, "No file or folder path to copy".to_string());
            return;
        }
        let text = paths
            .iter()
            .map(|raw| {
                let path = Path::new(raw);
                if absolute {
                    path.canonicalize()
                        .unwrap_or_else(|_| path.to_path_buf())
                        .to_string_lossy()
                        .to_string()
                } else if let Ok(relative) = path.strip_prefix(&self.config.workspace_root) {
                    format!("./{}", relative.to_string_lossy().replace('\\', "/"))
                } else if let Ok(relative) = path.strip_prefix(&root) {
                    format!("./{}", relative.to_string_lossy().replace('\\', "/"))
                } else {
                    path.to_string_lossy().replace('\\', "/")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        // egui forwards this platform output to the operating-system clipboard
        // without launching another window or injecting input.
        self.pending_system_clipboard = Some(text);
        self.set_compare_lane_message(
            lane_id,
            format!(
                "Copied {} {} path{}",
                paths.len(),
                if absolute { "absolute" } else { "portable" },
                if paths.len() == 1 { "" } else { "s" }
            ),
        );
    }

    fn compare_lane_make_unique_copy_target(&self, destination: &Path, source: &Path) -> PathBuf {
        let source_name = source
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        let source_stem = source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("image");
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");

        let first = destination.join(source_name);
        if !first.exists() {
            return first;
        }
        for copy_index in 1..=9999 {
            let name = if copy_index == 1 {
                format!("{} (copy)", source_stem)
            } else {
                format!("{} (copy {copy_index})", source_stem)
            };
            let candidate = if extension.is_empty() {
                destination.join(name)
            } else {
                destination.join(format!("{name}.{extension}"))
            };
            if !candidate.exists() {
                return candidate;
            }
        }
        let fallback = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map_or(0, |value| value.as_millis());
        if extension.is_empty() {
            destination.join(format!("{source_stem} (copy {fallback:06})"))
        } else {
            destination.join(format!("{source_stem} (copy {fallback:06}).{extension}"))
        }
    }

    fn compare_lane_paste(&mut self, lane_id: usize) {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let destination = sanitize_folder_input(&self.compare_lanes[pos].folder);
        if self.compare_clipboard.is_empty() {
            self.set_compare_lane_message(lane_id, "Clipboard empty".to_string());
            return;
        }
        if destination.trim().is_empty() {
            self.set_compare_lane_message(
                lane_id,
                "Set a destination folder before paste".to_string(),
            );
            return;
        }
        let destination_path = Path::new(&destination);
        if !destination_path.is_dir() {
            self.set_compare_lane_message(lane_id, "Destination folder does not exist".to_string());
            return;
        }

        let mut copied: usize = 0;
        let mut skipped: usize = 0;
        let mut pasted = Vec::new();
        for source in &self.compare_clipboard {
            if !Path::new(source).is_file() {
                skipped += 1;
                continue;
            }
            let target =
                self.compare_lane_make_unique_copy_target(destination_path, Path::new(source));
            match fs::copy(source, &target) {
                Ok(_) => {
                    copied += 1;
                    if let Some(text) = target.to_str() {
                        pasted.push(text.to_string());
                    }
                }
                Err(_) => skipped += 1,
            }
        }

        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let open_index = {
            let lane = &mut self.compare_lanes[pos];
            for target in pasted {
                if !lane.files.iter().any(|path| path == &target) {
                    Arc::make_mut(&mut lane.files).push(target);
                }
            }
            if lane.files.is_empty() {
                lane.image_error = "No supported images in folder".to_string();
                lane.loading_image = false;
                lane.loading_image_inflight = false;
                lane.pending_image_index = None;
                lane.image_path.clear();
                lane.texture = None;
                lane.texture_size = None;
                lane.selected_files.clear();
                lane.selection_anchor = None;
                0
            } else {
                Arc::make_mut(&mut lane.files).sort();
                lane.index = lane.index.min(lane.total().saturating_sub(1));
                lane.loading_image = true;
                lane.loading_image_inflight = false;
                lane.pending_image_index = None;
                lane.selected_files.clear();
                lane.selection_anchor = None;
                lane.image_error.clear();
                lane.image_path = lane.files[lane.index].clone();
                lane.index
            }
        };
        if copied > 0 {
            self.request_compare_image(lane_id, open_index);
        }
        self.set_compare_lane_message(
            lane_id,
            if copied == 0 && skipped > 0 {
                format!(
                    "Paste skipped: {} file{} unavailable",
                    skipped,
                    if skipped == 1 { "" } else { "s" }
                )
            } else if skipped == 0 {
                format!(
                    "Pasted {} file{}",
                    copied,
                    if copied == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "Pasted {} file{}, {} skipped",
                    copied,
                    if copied == 1 { "" } else { "s" },
                    skipped
                )
            },
        );
    }

    fn compare_lane_delete_selected(&mut self, lane_id: usize) {
        let indices = self.compare_lane_selected_indices(lane_id);
        if indices.is_empty() {
            self.set_compare_lane_message(lane_id, "No files selected to delete".to_string());
            return;
        }
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let deletable: Vec<(usize, String)> = {
            let lane = &self.compare_lanes[pos];
            indices
                .into_iter()
                .filter_map(|idx| lane.files.get(idx).map(|path| (idx, path.clone())))
                .collect()
        };
        let mut deleted = 0usize;
        let mut failed = 0usize;
        let mut removed_indices = HashSet::new();
        for (index, path) in deletable {
            if fs::remove_file(&path).is_ok() {
                deleted += 1;
                removed_indices.insert(index);
            } else {
                failed += 1;
            }
        }

        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let open_index = {
            let lane = &mut self.compare_lanes[pos];
            let mut next_files: Vec<String> = Vec::new();
            for (index, path) in lane.files.iter().enumerate() {
                if !removed_indices.contains(&index) {
                    next_files.push(path.clone());
                }
            }
            lane.files = Arc::new(next_files);
            if lane.files.is_empty() {
                lane.loading_image = false;
                lane.loading_image_inflight = false;
                lane.pending_image_index = None;
                lane.image_error = "No supported images in folder".to_string();
                lane.image_path.clear();
                lane.texture = None;
                lane.texture_size = None;
                lane.selected_files.clear();
                lane.selection_anchor = None;
                0
            } else {
                lane.selected_files.clear();
                lane.selection_anchor = None;
                lane.index = lane.index.min(lane.total().saturating_sub(1));
                lane.loading_image = true;
                lane.loading_image_inflight = false;
                lane.image_error.clear();
                lane.image_path = lane.files[lane.index].clone();
                lane.index
            }
        };
        if deleted > 0 && !self.compare_lanes[pos].files.is_empty() {
            self.request_compare_image(lane_id, open_index);
        }
        if deleted == 0 {
            if failed > 0 {
                self.set_compare_lane_message(
                    lane_id,
                    format!(
                        "Delete failed for {} file{}",
                        failed,
                        if failed == 1 { "" } else { "s" }
                    ),
                );
            } else {
                self.set_compare_lane_message(lane_id, "No files deleted".to_string());
            }
        } else if failed == 0 {
            self.set_compare_lane_message(
                lane_id,
                format!(
                    "Deleted {} file{}",
                    deleted,
                    if deleted == 1 { "" } else { "s" }
                ),
            );
        } else {
            self.set_compare_lane_message(
                lane_id,
                format!(
                    "Deleted {} file{}, {} failed",
                    deleted,
                    if deleted == 1 { "" } else { "s" },
                    failed
                ),
            );
        }
    }

    fn compare_lane_open_first_selected(&mut self, lane_id: usize) {
        let indices = self.compare_lane_selected_indices(lane_id);
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return;
        };
        let lane = &self.compare_lanes[pos];
        let target = if let Some(index) = indices.first() {
            *index
        } else if lane.total() > 0 {
            lane.index
        } else {
            self.set_compare_lane_message(lane_id, "No files selected".to_string());
            return;
        };
        if target >= lane.total() {
            self.set_compare_lane_message(lane_id, "Selected file index unavailable".to_string());
            return;
        }
        let file_name = lane
            .files
            .get(target)
            .and_then(|path| Path::new(path).file_name().and_then(|value| value.to_str()))
            .unwrap_or("image")
            .to_string();
        drop(lane);
        self.request_compare_image(lane_id, target);
        self.set_compare_lane_message(lane_id, format!("Opened {file_name}"));
    }

    fn apply_compare_lane_request(
        &mut self,
        lane_id: usize,
        request: CompareLaneRenderRequest,
        lane_ids: &[usize],
        sync_navigation: bool,
    ) {
        if let Some(path) = request.open_folder_in_new_tab.as_deref() {
            match self.open_media_folder_in_new_tab(path) {
                Ok(id) => {
                    self.compare_action_message = format!("Opened folder in {id}");
                    self.close_media_folder_navigator();
                }
                Err(error) => self.compare_action_message = error,
            }
            return;
        }
        if request.browse {
            let start = self
                .compare_lane_position(lane_id)
                .map(|pos| self.compare_lanes[pos].folder.clone())
                .unwrap_or_default();
            self.folder_picker.open(lane_id, &start);
        }
        if request.scan {
            self.start_compare_scan(lane_id);
        }
        if let Some(index) = request.target_index {
            self.request_compare_image(lane_id, index);
        }
        if request.open_file {
            if let Some(index) = request.open_index_in_system {
                self.compare_lane_open_selected_index_with_system(lane_id, index);
            } else {
                let _ = self.compare_lane_open_selected_with_system(lane_id);
            }
        } else if request.open_selected {
            let _ = self.compare_lane_open_selected_with_system(lane_id);
        } else if let Some(index) = request.open_index_in_system {
            self.compare_lane_open_selected_index_with_system(lane_id, index);
        }
        if let Some(index) = request.open_index {
            self.request_compare_image(lane_id, index);
        }
        if let Some(path) = request.open_path_in_system {
            let target = Path::new(&path);
            if !target.exists() {
                self.set_compare_lane_message(
                    lane_id,
                    format!("Open location failed for {}: path not found", path),
                );
            } else if let Err(error) =
                self.open_in_file_manager_with_system_app(target, target.is_file())
            {
                self.set_compare_lane_message(
                    lane_id,
                    format!(
                        "Open location failed for {}: {error}",
                        target.to_string_lossy()
                    ),
                );
            } else if let Some(name) = target.file_name().and_then(|value| value.to_str()) {
                self.set_compare_lane_message(lane_id, format!("Opened location for {name}"));
            } else {
                self.set_compare_lane_message(lane_id, "Opened location".to_string());
            }
        }
        if request.copy_selected {
            self.compare_lane_copy_selected(lane_id);
        }
        if request.paste {
            self.compare_lane_paste(lane_id);
        }
        if request.delete_selected {
            self.compare_lane_delete_selected(lane_id);
        }
        if request.open_location {
            self.compare_lane_open_location_with_system(lane_id, request.open_location_index);
        }
        if request.select_all {
            self.compare_lane_select_all(lane_id);
        }
        if request.select_none {
            self.compare_lane_select_none(lane_id);
        }
        if request.invert_selection {
            self.compare_lane_invert_selection(lane_id);
        }
        if let Some(delta) = request.nav_delta {
            if sync_navigation {
                for id in lane_ids {
                    self.nav_lane_relative(*id, delta);
                }
            } else {
                self.nav_lane_relative(lane_id, delta);
            }
        }
    }

    /// One lane: compact header rows + image viewport + navigation footer, all
    /// inside the same sheet card so controls can never desync from their image.
    fn draw_compare_lane_card(
        &mut self,
        ui: &mut egui::Ui,
        lane_id: usize,
        show_action_bar: bool,
    ) -> CompareLaneRenderRequest {
        let Some(pos) = self.compare_lane_position(lane_id) else {
            return CompareLaneRenderRequest::default();
        };
        let mut request = CompareLaneRenderRequest::default();
        let has_clipboard = !self.compare_clipboard.is_empty();
        let folder = sanitize_folder_input(&self.compare_lanes[pos].folder);
        let has_destination = !folder.trim().is_empty();
        let has_folder = !folder.trim().is_empty();
        let can_paste = has_clipboard && has_destination;
        let has_files = !self.compare_lanes[pos].files.is_empty();
        let has_selection = !self.compare_lanes[pos].selected_files.is_empty();
        // Compare panes always list every scanned file; the media search box
        // never filters this tab (WP-044 removed the old cross-tab leakage).
        let visible_indices = (0..self.compare_lanes[pos].files.len()).collect::<Vec<usize>>();
        let lane = &mut self.compare_lanes[pos];

        // Row 1: pane label + editable name + live status, one line. The name lets the
        // operator tag each pane with its batch/iteration so multiple panes stay
        // distinguishable (auto-filled from the folder leaf on scan; editable).
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Pane {}", lane.id + 1))
                    .strong()
                    .color(theme::ink()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut status = if lane.scanning {
                    "scanning…".to_string()
                } else if lane.loading_image {
                    format!("{} images · loading…", group_thousands(lane.total()))
                } else if lane.total() > 0 {
                    format!("{} images", group_thousands(lane.total()))
                } else {
                    "no folder scanned".to_string()
                };
                if !lane.action_message.is_empty() {
                    status = format!("{status} · {}", lane.action_message);
                }
                if !lane.selected_files.is_empty() {
                    status = format!(
                        "{} · {} selected",
                        status,
                        group_thousands(lane.selected_files.len())
                    );
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(status)
                            .small()
                            .color(theme::ink_faint()),
                    )
                    .wrap(false),
                );
                ui.add(
                    TextEdit::singleline(&mut lane.name)
                        .desired_width((ui.available_width() - 8.0).max(60.0))
                        .hint_text("name this pane (batch / iteration)"),
                );
            });
        });

        // Row 2: in-app browser + editable path (Enter scans) + rescan.
        ui.horizontal(|ui| {
            if ui
                .button(format!("{} Browse…", icons::FOLDER_OPEN))
                .on_hover_text("Pick a folder inside the app (no external window)")
                .clicked()
            {
                request.browse = true;
            }
            // Reserve room for the ⟳ button AND the item spacing after the
            // edit box: over-requesting here makes egui expand the card's
            // region and silently breaks every later rect computation.
            let rescan_w = 44.0 + ui.spacing().item_spacing.x;
            let path_resp = ui.add(
                TextEdit::singleline(&mut lane.folder)
                    .desired_width((ui.available_width() - rescan_w).max(80.0))
                    .hint_text("paste a folder path or Browse…"),
            );
            if path_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                request.scan = true;
            }
            if ui
                .add_enabled(
                    !lane.folder.trim().is_empty(),
                    egui::Button::new(icons::ARROW_CLOCKWISE),
                )
                .on_hover_text("Scan this folder")
                .clicked()
            {
                request.scan = true;
            }
            if ui
                .add_enabled(has_selection || has_files, egui::Button::new("Open file"))
                .on_hover_text("Open selected file(s) in the default app (Enter or Ctrl/Cmd+O)")
                .clicked()
            {
                request.open_file = true;
            }
            if ui
                .add_enabled(
                    has_files || has_folder,
                    egui::Button::new("Open file location"),
                )
                .on_hover_text("Reveal selected file/folder in OS file manager")
                .clicked()
            {
                request.open_location = true;
            }
        });

        if show_action_bar {
            ui.separator();
            ui.horizontal(|ui| {
                // Toolbar variant (no menu section headers, no close_menu):
                // the inline variant rendered menu rows horizontally and ran
                // labels together (WP-048 baseline defect).
                Self::draw_explorer_action_buttons(
                    ui,
                    &mut request,
                    has_selection || has_files,
                    has_selection,
                    has_files,
                    has_folder,
                    can_paste,
                    None,
                    false,
                );
            });
        }

        // Row 3: recursion toggle + scan warning/error on the same line.
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut lane.recursive, "Include subfolders")
                .changed()
                && !lane.folder.trim().is_empty()
            {
                request.scan = true;
            }
            if !lane.scan_error.is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&lane.scan_error)
                            .small()
                            .color(theme::error_ink()),
                    )
                    .truncate(true),
                );
            }
        });

        // Split the rest of the card into viewport + footer by explicit rects,
        // anchored to the card's bottom. Estimating with available_height()
        // here inherits stale cross-pass ScrollArea sizes and overgrows the
        // card, pushing the footer off screen. available_rect_before_wrap can
        // over-report past max_rect, so clamp to the card's real bounds.
        let remaining = ui.available_rect_before_wrap().intersect(ui.max_rect());
        let footer_h = 30.0;
        let gap = 6.0;
        let content_h = (remaining.height() - footer_h - gap).max(0.0);
        let list_h = if content_h > 116.0 {
            (content_h * 0.33).clamp(56.0, 180.0)
        } else {
            0.0
        };
        let image_h = (content_h - list_h).max(60.0);
        let well_height = image_h + list_h;
        let well_rect = egui::Rect::from_min_max(
            remaining.min,
            egui::pos2(
                remaining.right(),
                (remaining.bottom() - footer_h - gap).max(remaining.min.y + well_height),
            ),
        );
        let footer_rect = egui::Rect::from_min_max(
            egui::pos2(remaining.left(), well_rect.bottom() + gap),
            remaining.max,
        );

        let mut wheel_delta: Option<isize> = None;
        ui.allocate_ui_at_rect(well_rect, |ui| {
            theme::well_frame().show(ui, |ui| {
                ui.set_width(well_rect.width() - 10.0);
                ui.set_min_height(well_rect.height() - 10.0);
                let inner = ui.max_rect();
                let has_selection = !lane.selected_files.is_empty();
                {
                    let list_top_h = if list_h > 0.0 { list_h } else { 0.0 };
                    let image_area = egui::Rect::from_min_max(
                        inner.min,
                        egui::pos2(inner.max.x, inner.min.y + image_h),
                    );
                    let list_area = egui::Rect::from_min_max(
                        egui::pos2(inner.min.x, inner.min.y + image_h + 4.0),
                        inner.max,
                    );

                    // Lower area: selectable file list with explorer-like actions.
                    if list_top_h > 0.0 {
                        let list_area_response = ui.allocate_ui_at_rect(list_area, |ui| {
                            ui.horizontal(|ui| {
                                let media_label = match lane.media_filter {
                                    MediaFilterMode::ImagesOnly => "Images",
                                    MediaFilterMode::VideosOnly => "Videos",
                                    MediaFilterMode::All => "Media",
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{media_label} ({})",
                                        group_thousands(lane.total())
                                    ))
                                    .small()
                                    .strong()
                                    .color(theme::ink()),
                                );
                                // (WP-048) The mirrored action row that lived
                                // here duplicated the card's action bar with
                                // reversed labels and clashing widget IDs.
                            });
                            let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                            let shift = ui.input(|i| i.modifiers.shift);
                            // Per-lane id: two panes sharing the auto id
                            // produced on-screen egui duplicate-ID warnings.
                            ScrollArea::vertical()
                                .id_source(("compare_file_list", lane.id))
                                .show(ui, |ui| {
                                    if lane.files.is_empty() {
                                        ui.label("No media found in this lane.");
                                    } else if visible_indices.is_empty() {
                                        ui.label("No media found for this search.");
                                    } else {
                                        for index in visible_indices.iter().copied() {
                                            if index >= lane.files.len() {
                                                continue;
                                            }
                                            let path = &lane.files[index];
                                            let file_name = Path::new(path)
                                                .file_name()
                                                .and_then(|value| value.to_str())
                                                .unwrap_or("image");
                                            let selected = lane.selected_files.contains(&index);
                                            let label = elide_middle(
                                                &format!("{:>4}. {}", index + 1, file_name),
                                                88,
                                            );
                                            let row = ui
                                                .selectable_label(selected, label)
                                                .on_hover_text(path);
                                            if row.clicked() {
                                                if shift {
                                                    let anchor =
                                                        lane.selection_anchor.unwrap_or(index);
                                                    let lo = anchor.min(index);
                                                    let hi = anchor.max(index);
                                                    lane.selected_files.clear();
                                                    for i in lo..=hi {
                                                        lane.selected_files.insert(i);
                                                    }
                                                    lane.selection_anchor = Some(anchor);
                                                } else if ctrl {
                                                    if selected {
                                                        lane.selected_files.remove(&index);
                                                    } else {
                                                        lane.selected_files.insert(index);
                                                    }
                                                    lane.selection_anchor = Some(index);
                                                } else {
                                                    lane.selected_files.clear();
                                                    lane.selected_files.insert(index);
                                                    lane.selection_anchor = Some(index);
                                                }
                                            }
                                            if row.double_clicked() {
                                                request.open_index_in_system = Some(index);
                                            }
                                            row.context_menu(|ui| {
                                                let was_multi_selected =
                                                    lane.selected_files.len() > 1;
                                                let clicked_is_selected =
                                                    lane.selected_files.contains(&index);
                                                if !lane.selected_files.contains(&index) {
                                                    lane.selected_files.clear();
                                                    lane.selected_files.insert(index);
                                                    lane.selection_anchor = Some(index);
                                                }
                                                let open_index_for_menu =
                                                    if was_multi_selected && clicked_is_selected {
                                                        None
                                                    } else {
                                                        Some(index)
                                                    };
                                                let has_selection_in_menu =
                                                    !lane.selected_files.is_empty();
                                                Self::draw_explorer_context_actions(
                                                    ui,
                                                    &mut request,
                                                    has_selection_in_menu || has_files,
                                                    has_selection_in_menu,
                                                    has_files,
                                                    has_folder,
                                                    can_paste,
                                                    open_index_for_menu,
                                                );
                                            });
                                        }
                                    }
                                });
                        });
                        // Explicit per-lane id: the InnerResponse's auto id
                        // collided between panes (on-screen egui warnings).
                        let _ = list_area_response;
                        ui.interact(
                            list_area,
                            ui.id().with(("compare_list_bg", lane.id)),
                            Sense::click(),
                        )
                        .context_menu(|ui| {
                            Self::draw_explorer_context_actions(
                                ui,
                                &mut request,
                                has_selection || has_files,
                                has_selection,
                                has_files,
                                has_folder,
                                can_paste,
                                None,
                            );
                        });
                    }

                    // Upper area: preview.
                    ui.allocate_ui_at_rect(image_area, |ui| {
                        if let Some(texture) = lane.texture.as_ref() {
                            let caption_h = 24.0;
                            let mat_pad = 6.0;
                            let has_selection_in_menu = !lane.selected_files.is_empty();
                            let preview_open_index = if lane.total() > 0 {
                                Some(lane.index.min(lane.total().saturating_sub(1)))
                            } else {
                                None
                            };
                            let preview_open_index = if has_selection_in_menu {
                                None
                            } else {
                                preview_open_index
                            };
                            let fit_space = egui::vec2(
                                (image_area.width() - 2.0 * mat_pad - 8.0).max(40.0),
                                (image_area.height() - 2.0 * mat_pad - 8.0).max(40.0),
                            );
                            let target = fit_for_compare_frame(texture.size_vec2(), fit_space);
                            let img_rect =
                                egui::Rect::from_center_size(image_area.center(), target);
                            ui.painter().rect(
                                img_rect.expand(mat_pad),
                                egui::Rounding::same(2.0),
                                theme::mat(),
                                theme::rule_stroke(),
                            );
                            let response = ui
                                .put(
                                    img_rect,
                                    egui::Image::new((texture.id(), target)).sense(Sense::hover()),
                                )
                                .on_hover_text(&lane.image_path);
                            if response.double_clicked() {
                                request.open_index_in_system = Some(lane.index);
                            }
                            response.context_menu(|ui| {
                                Self::draw_explorer_context_actions(
                                    ui,
                                    &mut request,
                                    true,
                                    has_selection_in_menu,
                                    has_files,
                                    has_folder,
                                    can_paste,
                                    preview_open_index,
                                );
                            });
                            if response.hovered() {
                                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                                if scroll != 0.0 && lane.total() > 0 {
                                    let step = ((scroll.abs() / 120.0).round() as isize).max(1);
                                    wheel_delta = Some(if scroll > 0.0 { -step } else { step });
                                }
                            }
                            if !lane.image_path.is_empty() {
                                let file = Path::new(&lane.image_path)
                                    .file_name()
                                    .and_then(|value| value.to_str())
                                    .unwrap_or("unknown");
                                ui.painter().text(
                                    egui::pos2(
                                        image_area.center().x,
                                        image_area.max.y - caption_h / 2.0,
                                    ),
                                    egui::Align2::CENTER_CENTER,
                                    elide_middle(file, 46),
                                    egui::TextStyle::Small.resolve(ui.style()),
                                    theme::ink_faint(),
                                );
                            }
                        } else {
                            let center = image_area.center();
                            let (icon, message, color) = if !lane.image_error.is_empty() {
                                (icons::WARNING, lane.image_error.clone(), theme::error_ink())
                            } else if lane.scanning || lane.loading_image {
                                (
                                    icons::HOURGLASS_MEDIUM,
                                    "loading…".to_string(),
                                    theme::ink_faint(),
                                )
                            } else {
                                (
                                    icons::IMAGES,
                                    "Browse… or paste a folder path above".to_string(),
                                    theme::ink_faint(),
                                )
                            };
                            ui.painter().text(
                                center - egui::vec2(0.0, 22.0),
                                egui::Align2::CENTER_CENTER,
                                icon,
                                egui::FontId::proportional(46.0),
                                color.gamma_multiply(0.55),
                            );
                            ui.painter().text(
                                center + egui::vec2(0.0, 20.0),
                                egui::Align2::CENTER_CENTER,
                                elide_middle(&message, 60),
                                egui::TextStyle::Body.resolve(ui.style()),
                                color,
                            );
                            ui.allocate_rect(image_area, Sense::hover())
                                .on_hover_cursor(egui::CursorIcon::Default)
                                .context_menu(|ui| {
                                    let has_selection_in_menu = !lane.selected_files.is_empty();
                                    let empty_preview_open_index = if lane.total() > 0 {
                                        Some(lane.index.min(lane.total().saturating_sub(1)))
                                    } else {
                                        None
                                    };
                                    let empty_preview_open_index = if has_selection_in_menu {
                                        None
                                    } else {
                                        empty_preview_open_index
                                    };
                                    Self::draw_explorer_context_actions(
                                        ui,
                                        &mut request,
                                        has_selection || has_files,
                                        has_selection,
                                        has_files,
                                        has_folder,
                                        can_paste,
                                        empty_preview_open_index,
                                    );
                                });
                        }
                    });
                }
            });
        });
        request.nav_delta = request.nav_delta.or(wheel_delta);

        // Footer: prev/next + position + jump box (the filename lives in the
        // caption under the print now).
        ui.allocate_ui_at_rect(footer_rect, |ui| {
            ui.horizontal(|ui| {
                let total = lane.total();
                let can_nav = total > 0;
                if ui
                    .add_enabled(
                        can_nav,
                        egui::Button::new(format!("{} Prev", icons::CARET_LEFT)),
                    )
                    .clicked()
                {
                    request.nav_delta = Some(-1);
                }
                if ui
                    .add_enabled(
                        can_nav,
                        egui::Button::new(format!("Next {}", icons::CARET_RIGHT)),
                    )
                    .clicked()
                {
                    request.nav_delta = Some(1);
                }
                if can_nav {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            group_thousands((lane.index + 1).min(total)),
                            group_thousands(total)
                        ))
                        .color(theme::ink()),
                    );
                    let jump_resp = ui.add(
                        TextEdit::singleline(&mut lane.pending_jump)
                            .desired_width(56.0)
                            .hint_text("go to"),
                    );
                    if jump_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Ok(value) = lane.pending_jump.trim().parse::<usize>() {
                            request.target_index =
                                Some(value.saturating_sub(1).min(total.saturating_sub(1)));
                        }
                    }
                }
            });
        });

        request
    }

    fn draw_manual_tab(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Manual");

        if self.manual_text.trim().is_empty() {
            ui.label("Manual is empty or product/docs/MANUAL.md could not be loaded.");
            return;
        }

        // Section index: (heading label, is_reference) per "## " heading. The synthetic
        // "contents" topic is skipped; topics whose id starts with "ref-" are the
        // automation/reference half, collapsed under their own group. (WP-026)
        let mut sections: Vec<(String, bool)> = Vec::new();
        {
            let mut is_ref = false;
            let mut skip = false;
            for line in self.manual_text.lines() {
                let t = line.trim_start();
                if t.starts_with("<topic") {
                    let id = parse_topic_id(t);
                    skip = id.as_deref() == Some("contents");
                    is_ref = id
                        .as_deref()
                        .map(|s| s.starts_with("ref-"))
                        .unwrap_or(false);
                    continue;
                }
                if t.starts_with("</topic>") {
                    skip = false;
                    continue;
                }
                if skip {
                    continue;
                }
                if line.starts_with("## ") {
                    sections.push((line[3..].trim().to_string(), is_ref));
                }
            }
        }

        let target = self.manual_scroll_target.take();
        let current = self.manual_current_section;
        let mut clicked: Option<usize> = None;

        // Two-pane docs layout: a grouped table-of-contents sidebar on the left, the
        // formatted manual on the right. (WP-026)
        let avail_h = ui.available_height();
        ui.horizontal_top(|ui| {
            let toc_w = 220.0_f32;
            ui.allocate_ui_with_layout(
                egui::vec2(toc_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(toc_w);
                    ui.set_max_width(toc_w);
                    egui::ScrollArea::vertical()
                        .id_source("manual_toc")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new("OPERATOR GUIDE")
                                    .small()
                                    .strong()
                                    .color(theme::ink_faint()),
                            );
                            for (i, (label, is_ref)) in sections.iter().enumerate() {
                                if *is_ref {
                                    continue;
                                }
                                // Small single-line rows: long section titles
                                // wrapped into tall chips that crowded the
                                // narrow sidebar (WP-048).
                                if ui
                                    .selectable_label(
                                        current == i,
                                        egui::RichText::new(label.as_str()).small(),
                                    )
                                    .clicked()
                                {
                                    clicked = Some(i);
                                }
                            }
                            ui.add_space(8.0);
                            egui::CollapsingHeader::new(
                                egui::RichText::new("Reference (automation)")
                                    .small()
                                    .strong()
                                    .color(theme::ink_faint()),
                            )
                            .id_source("manual_toc_reference")
                            .default_open(false)
                            .show(ui, |ui| {
                                for (i, (label, is_ref)) in sections.iter().enumerate() {
                                    if !*is_ref {
                                        continue;
                                    }
                                    if ui
                                        .selectable_label(
                                            current == i,
                                            egui::RichText::new(label.as_str()).small(),
                                        )
                                        .clicked()
                                    {
                                        clicked = Some(i);
                                    }
                                }
                            });
                        });
                },
            );
            ui.separator();
            let content_w = (ui.available_width() - 4.0).max(200.0);
            ui.allocate_ui_with_layout(
                egui::vec2(content_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::ScrollArea::vertical()
                        .id_source("manual_content")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.render_manual_body(ui, target);
                        });
                },
            );
        });

        if let Some(i) = clicked {
            self.manual_scroll_target = Some(i);
            self.manual_current_section = i;
        }
    }

    /// Render the manual body with light markdown formatting (headings, inline bold +
    /// code, fenced code blocks) and scroll to the requested section heading. (WP-026)
    fn render_manual_body(&self, ui: &mut egui::Ui, target: Option<usize>) {
        use egui::text::{LayoutJob, TextFormat};
        use egui::{FontFamily, FontId};

        let body_size = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .map(|f| f.size)
            .unwrap_or(16.0);
        let ink = theme::ink();

        let mut h = 0usize;
        let mut skip = false;
        let mut in_code = false;
        let mut code = String::new();
        let mut para = LayoutJob::default();
        let mut para_has = false;

        let flush = |ui: &mut egui::Ui, para: &mut LayoutJob, has: &mut bool| {
            if *has {
                ui.label(std::mem::take(para));
                *has = false;
            }
        };

        for line in self.manual_text.lines() {
            let t = line.trim_start();
            if t.starts_with("<topic") {
                skip = parse_topic_id(t).as_deref() == Some("contents");
                continue;
            }
            if t.starts_with("</topic>") {
                skip = false;
                continue;
            }
            if skip
                || line == "---"
                || line.starts_with("file_id:")
                || line.starts_with("file_kind:")
                || line.starts_with("updated_at:")
            {
                continue;
            }

            if t.starts_with("```") {
                flush(ui, &mut para, &mut para_has);
                if in_code {
                    render_manual_code_block(ui, &code, body_size);
                    code.clear();
                }
                in_code = !in_code;
                continue;
            }
            if in_code {
                code.push_str(line);
                code.push('\n');
                continue;
            }

            if line.is_empty() {
                flush(ui, &mut para, &mut para_has);
                ui.add_space(6.0);
            } else if let Some(rest) = line.strip_prefix("# ") {
                flush(ui, &mut para, &mut para_has);
                ui.heading(rest);
            } else if let Some(rest) = line.strip_prefix("## ") {
                flush(ui, &mut para, &mut para_has);
                ui.add_space(6.0);
                let resp = ui.add(egui::Label::new(
                    egui::RichText::new(rest)
                        .family(FontFamily::Name(theme::HEADING_FAMILY.into()))
                        .size((body_size * 1.18).round())
                        .color(theme::ink()),
                ));
                if target == Some(h) {
                    resp.scroll_to_me(Some(egui::Align::TOP));
                }
                theme::hairline(ui);
                h += 1;
            } else if let Some(rest) = line.strip_prefix("### ") {
                flush(ui, &mut para, &mut para_has);
                ui.add(egui::Label::new(
                    egui::RichText::new(rest)
                        .family(FontFamily::Name(theme::HEADING_FAMILY.into()))
                        .color(theme::ink()),
                ));
            } else {
                if para_has {
                    para.append(
                        "\n",
                        0.0,
                        TextFormat {
                            font_id: FontId::new(body_size, FontFamily::Proportional),
                            color: ink,
                            ..Default::default()
                        },
                    );
                }
                append_manual_inline(&mut para, line, body_size, ink);
                para_has = true;
            }
        }
        flush(ui, &mut para, &mut para_has);
    }
}

/// Extract the `id="..."` value from a `<topic ...>` opening tag line.
fn parse_topic_id(line: &str) -> Option<String> {
    let start = line.find("id=\"")? + 4;
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Append a manual body line to `job`, rendering inline `**bold**` and `` `code` ``
/// runs with the heading (SemiBold) and monospace faces. (WP-026)
fn append_manual_inline(
    job: &mut egui::text::LayoutJob,
    line: &str,
    base: f32,
    color: egui::Color32,
) {
    use egui::text::TextFormat;
    use egui::{FontFamily, FontId};
    let code_col = theme::accent();
    let mut seg = String::new();
    let mut mode = 0u8; // 0 = normal, 1 = bold, 2 = code
    let mut i = 0usize;
    let mut push = |seg: &mut String, mode: u8, job: &mut egui::text::LayoutJob| {
        if seg.is_empty() {
            return;
        }
        let (font, col) = match mode {
            1 => (
                FontId::new(base, FontFamily::Name(theme::HEADING_FAMILY.into())),
                color,
            ),
            2 => (
                FontId::new((base * 0.92).round(), FontFamily::Monospace),
                code_col,
            ),
            _ => (FontId::new(base, FontFamily::Proportional), color),
        };
        job.append(
            seg,
            0.0,
            TextFormat {
                font_id: font,
                color: col,
                ..Default::default()
            },
        );
        seg.clear();
    };
    while i < line.len() {
        if mode != 2 && line[i..].starts_with("**") {
            push(&mut seg, mode, job);
            mode = if mode == 1 { 0 } else { 1 };
            i += 2;
            continue;
        }
        if line[i..].starts_with('`') {
            push(&mut seg, mode, job);
            mode = if mode == 2 { 0 } else { 2 };
            i += 1;
            continue;
        }
        let ch = line[i..].chars().next().unwrap();
        seg.push(ch);
        i += ch.len_utf8();
    }
    push(&mut seg, mode, job);
}

/// Render a fenced code block as a monospace box. (WP-026)
fn render_manual_code_block(ui: &mut egui::Ui, code: &str, base: f32) {
    egui::Frame::none()
        .fill(theme::mat())
        .stroke(theme::rule_stroke())
        .inner_margin(egui::Margin::same(8.0))
        .outer_margin(egui::Margin::symmetric(0.0, 2.0))
        .show(ui, |ui| {
            ui.add(egui::Label::new(
                egui::RichText::new(code.trim_end_matches('\n'))
                    .monospace()
                    .size((base * 0.92).round())
                    .color(theme::ink()),
            ));
        });
}

fn is_supported_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "gif"
            )
        })
}

/// Width of the "mm:ss / mm:ss" transport label at the current font scale, so
/// scrubbers can reserve exactly what the row needs rather than a fixed guess
/// that breaks at other window sizes and font scales (WP-070).
fn media_time_label_width(ui: &egui::Ui, length_ms: i64) -> f32 {
    let sample = format!(
        "{} / {}",
        format_media_time(length_ms),
        format_media_time(length_ms)
    );
    let font = egui::TextStyle::Body.resolve(ui.style());
    ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(sample, font, egui::Color32::WHITE)
            .size()
            .x
    })
}

/// (mtime seconds, size bytes) for cache keys; zeros when unreadable.
fn file_stat_pair(path: &str) -> (u64, u64) {
    std::fs::metadata(path)
        .map(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, meta.len())
        })
        .unwrap_or((0, 0))
}

/// Resolve a stable label ID through the operator's persisted backend hex.
fn build_media_label_color_cache(
    definitions: &[crate::media_db::ColorLabelDefinition],
) -> Arc<HashMap<String, egui::Color32>> {
    Arc::new(
        definitions
            .iter()
            .map(|definition| {
                let color = crate::media_db::normalize_hex_color(&definition.hex)
                    .map(|(_, [r, g, b])| egui::Color32::from_rgb(r, g, b))
                    .unwrap_or_else(|| egui::Color32::from_rgb(128, 128, 128));
                (definition.id.clone(), color)
            })
            .collect(),
    )
}

fn media_label_color(
    definitions: &[crate::media_db::ColorLabelDefinition],
    label_id: &str,
) -> egui::Color32 {
    definitions
        .iter()
        .find(|definition| definition.id == label_id)
        .and_then(|definition| crate::media_db::normalize_hex_color(&definition.hex))
        .map(|(_, [r, g, b])| egui::Color32::from_rgb(r, g, b))
        .unwrap_or_else(|| egui::Color32::from_rgb(128, 128, 128))
}

fn is_supported_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "mpg" | "mpeg" | "wmv"
            )
        })
}

/// Clean a user-entered folder path: trims whitespace and strips a single pair of
/// surrounding quotes. Windows "Copy as path" wraps the path in double quotes, which
/// would otherwise make the folder read as "not found".
fn load_media_tabs_with_recovery(media_db: &MediaDb) -> (MediaTabsState, String, bool) {
    let Some(encoded) = media_db.setting(MEDIA_TABS_SETTING_KEY) else {
        return (MediaTabsState::default(), String::new(), false);
    };
    match MediaTabsState::decode(&encoded) {
        Ok(state) => (state, String::new(), false),
        Err(decode_error) => {
            match media_db.set_setting(MEDIA_TABS_RECOVERY_SETTING_KEY, &encoded) {
                Ok(()) => (
                    MediaTabsState::default(),
                    format!(
                        "Media tab session rejected and preserved for recovery; using safe default: {decode_error}"
                    ),
                    false,
                ),
                Err(recovery_error) => (
                    MediaTabsState::default(),
                    format!(
                        "Media tab session rejected; recovery copy failed and automatic tab persistence is disabled: {decode_error}; recovery error: {recovery_error}"
                    ),
                    true,
                ),
            }
        }
    }
}

fn sanitize_folder_input(raw: &str) -> String {
    let s = raw.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].trim().to_string();
        }
    }
    s.to_string()
}

fn pending_path_index_in_sorted(
    sorted_paths: &[String],
    exact_path: &str,
    canonical_key: &str,
    mut key_for: impl FnMut(&str) -> String,
) -> Option<usize> {
    sorted_paths
        .binary_search_by(|candidate| candidate.as_str().cmp(exact_path))
        .ok()
        .or_else(|| {
            // Cached inventory can retain earlier drive-letter case or
            // separator spelling. Pay the canonical O(n) fallback only after
            // the exact O(log n) lookup misses, and only once at terminal
            // publication rather than on every render frame.
            sorted_paths
                .iter()
                .position(|candidate| key_for(candidate) == canonical_key)
        })
}

fn keep_inline_video_awaiting(
    scan_reconciling: bool,
    request_age: Option<std::time::Duration>,
) -> bool {
    request_age.is_some_and(|age| {
        let limit = if scan_reconciling {
            // Enough for the verified 141k mapped-folder scan plus inventory
            // commit, but still bounded if a provider or worker stalls.
            std::time::Duration::from_secs(120)
        } else {
            std::time::Duration::from_secs(10)
        };
        age < limit
    })
}

fn preserve_inline_request_anchor(scan_reconciling: bool) -> bool {
    scan_reconciling
}

/// Classify the exact requested folder rather than borrowing the active tab's
/// root. This keeps staged cross-drive/UNC browsing in the correct I/O budget
/// and attributes its diagnostics to the path actually being enumerated.
fn media_io_root_identity_for_path(path: &str, generation: u64) -> crate::media_io::RootIdentity {
    let root = Path::new(path);
    let stable_root = crate::media_db::stable_media_root_identity(root);
    let kind = if stable_root.starts_with("//") || crate::video_player::is_remote_media_path(root) {
        crate::media_io::RootKind::Remote
    } else if root.is_absolute() {
        crate::media_io::RootKind::Local
    } else {
        crate::media_io::RootKind::Unknown
    };
    crate::media_io::RootIdentity::new(stable_root, generation, kind)
}

/// Render-time variant for staged folder browsing. It keeps the same exact
/// path classification but uses a lexical key so Windows network-provider
/// alias resolution never blocks the UI thread.
fn media_io_staged_root_identity_for_path(
    path: &str,
    generation: u64,
) -> crate::media_io::RootIdentity {
    let root = Path::new(path);
    let lexical_root = crate::media_db::lexical_media_root_identity(root);
    // UNC is lexically remote. A drive letter may be local or mapped; classify
    // it as Unknown here so the conservative coordinator limits apply without
    // calling GetDriveTypeW (or a network provider) on the render thread.
    let kind = if lexical_root.starts_with("//") {
        crate::media_io::RootKind::Remote
    } else if root.is_absolute() {
        crate::media_io::RootKind::Unknown
    } else {
        crate::media_io::RootKind::Unknown
    };
    crate::media_io::RootIdentity::new(lexical_root, generation, kind)
}

fn collect_media_paths_for_compare(
    root: &Path,
    recursive: bool,
    media_filter: MediaFilterMode,
    mut on_batch: impl FnMut(Vec<String>),
) -> Result<(Vec<String>, usize), String> {
    collect_media_paths_for_compare_cancellable(
        root,
        recursive,
        media_filter,
        || false,
        &mut on_batch,
    )
    .map_err(|failure| match failure {
        MediaScanFailure::Cancelled => "Media scan cancelled".to_string(),
        MediaScanFailure::Failed(error) => error,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum MediaScanFailure {
    Cancelled,
    Failed(String),
}

fn collect_media_paths_for_compare_cancellable(
    root: &Path,
    recursive: bool,
    media_filter: MediaFilterMode,
    mut is_cancelled: impl FnMut() -> bool,
    mut on_batch: impl FnMut(Vec<String>),
) -> Result<(Vec<String>, usize), MediaScanFailure> {
    if is_cancelled() {
        return Err(MediaScanFailure::Cancelled);
    }
    let root_metadata = std::fs::metadata(root).map_err(|error| {
        MediaScanFailure::Failed(if error.kind() == std::io::ErrorKind::NotFound {
            format!("Folder not found: {root:?}")
        } else {
            format!("Cannot read media path {root:?}: {error}")
        })
    })?;
    if root_metadata.is_file() {
        let filter_ok = match media_filter {
            MediaFilterMode::ImagesOnly => is_supported_image_path(root),
            MediaFilterMode::VideosOnly => is_supported_video_path(root),
            MediaFilterMode::All => is_supported_image_path(root) || is_supported_video_path(root),
        };
        if filter_ok {
            let item = root.to_string_lossy().to_string();
            on_batch(vec![item.clone()]);
            return Ok((vec![item], 0usize));
        }
        return Err(MediaScanFailure::Failed(format!(
            "Path is not a supported media file for this filter: {root:?}"
        )));
    }
    if !root_metadata.is_dir() {
        return Err(MediaScanFailure::Failed(format!(
            "Path is not a folder: {root:?}"
        )));
    }

    let mut out = Vec::new();
    // Canonicalize the root once. Normal descendants inherit an equivalent
    // identity lexically; only reparse/symlink directories pay another
    // canonicalization. This preserves loop/alias suppression without one NAS
    // canonicalization round trip for every ordinary directory.
    let root_identity = root
        .canonicalize()
        .unwrap_or_else(|_| absolute_lexical_path(root));
    let mut queue = vec![(root.to_path_buf(), root_identity)];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut dir_errors = 0usize;
    const FIRST_BATCH: usize = 64;
    const BULK_BATCH: usize = 1_024;
    let mut batch_limit = FIRST_BATCH;
    let mut batch: Vec<String> = Vec::with_capacity(BULK_BATCH);

    while let Some((path, identity)) = queue.pop() {
        if is_cancelled() {
            return Err(MediaScanFailure::Cancelled);
        }
        if !seen.insert(identity.clone()) {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(&path) else {
            dir_errors += 1;
            continue;
        };
        for entry_result in entries {
            if is_cancelled() {
                return Err(MediaScanFailure::Cancelled);
            }
            // A directory iterator can fail after read_dir itself succeeds
            // (notably when an SMB enumeration is interrupted). Never hide
            // that failure and later commit a partial set as authoritative.
            let entry = match entry_result {
                Ok(entry) => entry,
                Err(_) => {
                    dir_errors = dir_errors.saturating_add(1);
                    continue;
                }
            };
            let entry_path = entry.path();
            let file_type = match entry.file_type() {
                Ok(value) => value,
                Err(_) => {
                    dir_errors = dir_errors.saturating_add(1);
                    continue;
                }
            };
            if file_type.is_dir() {
                if recursive {
                    let child_identity = if directory_entry_needs_canonical(&entry, &file_type) {
                        entry_path
                            .canonicalize()
                            .unwrap_or_else(|_| identity.join(entry.file_name()))
                    } else {
                        identity.join(entry.file_name())
                    };
                    queue.push((entry_path, child_identity));
                }
                continue;
            }
            // Preserve the former Path::metadata symlink semantics, but pay
            // the follow-up query only for link/reparse entries rather than
            // for every regular media file.
            if file_type.is_symlink() {
                let Ok(metadata) = entry_path.metadata() else {
                    dir_errors = dir_errors.saturating_add(1);
                    continue;
                };
                if metadata.is_dir() {
                    if recursive {
                        let child_identity = entry_path
                            .canonicalize()
                            .unwrap_or_else(|_| identity.join(entry.file_name()));
                        queue.push((entry_path, child_identity));
                    }
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
            } else if !file_type.is_file() {
                continue;
            }
            if media_filter_accepts(media_filter, &entry_path) {
                let item = entry_path.to_string_lossy().to_string();
                out.push(item.clone());
                batch.push(item);
                if batch.len() >= batch_limit {
                    on_batch(std::mem::replace(
                        &mut batch,
                        Vec::with_capacity(BULK_BATCH),
                    ));
                    batch_limit = BULK_BATCH;
                }
            }
        }
    }

    if !batch.is_empty() {
        on_batch(batch);
    }

    // An empty, zero-error traversal is an authoritative empty collection and
    // must be allowed to replace a prior inventory. An empty traversal with
    // errors remains incomplete via dir_errors and therefore is never
    // committed by start_compare_scan.
    Ok((out, dir_errors))
}

fn media_filter_accepts(media_filter: MediaFilterMode, path: &Path) -> bool {
    match media_filter {
        MediaFilterMode::ImagesOnly => is_supported_image_path(path),
        MediaFilterMode::VideosOnly => is_supported_video_path(path),
        MediaFilterMode::All => is_supported_image_path(path) || is_supported_video_path(path),
    }
}

fn absolute_lexical_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn directory_entry_needs_canonical(
    entry: &std::fs::DirEntry,
    file_type: &std::fs::FileType,
) -> bool {
    if file_type.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT. DirEntry::metadata reuses enumeration
        // information on Windows and does not follow the reparse point.
        return entry
            .metadata()
            .map(|metadata| metadata.file_attributes() & 0x400 != 0)
            .unwrap_or(true);
    }
    #[cfg(not(windows))]
    false
}

/// Shared softened-background veil for focused in-app surfaces (WP-051/WP-055).
/// A dismissible veil owns the full-screen interaction layer, preventing an
/// outside click from reaching Media controls beneath the modal.
fn draw_soft_modal_backdrop(
    ctx: &egui::Context,
    id_source: &'static str,
    dismissible: bool,
    blurred_texture: Option<&TextureHandle>,
) -> bool {
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new(id_source))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .interactable(dismissible)
        .show(ctx, |ui| {
            let sense = if dismissible {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            };
            let response = ui.allocate_response(screen.size(), sense);
            if let Some(texture) = blurred_texture {
                ui.painter().image(
                    texture.id(),
                    response.rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                // Neutral fallback only: never mix in a theme color, which
                // caused the washed/high-saturation appearance.
                ui.painter()
                    .rect_filled(response.rect, 0.0, egui::Color32::from_black_alpha(42));
            }
            response.clicked()
        })
        .inner
}

/// Build a bounded, untinted Gaussian backdrop. Downsampling first both
/// widens the perceived softness and caps the one-shot CPU/upload cost on
/// high-resolution displays.
fn gaussian_settings_backdrop(source: &ColorImage, max_edge: usize) -> ColorImage {
    let [source_w, source_h] = source.size;
    if source_w == 0 || source_h == 0 {
        return source.clone();
    }
    let scale = (max_edge.max(1) as f32 / source_w.max(source_h) as f32).min(1.0);
    let target_w = ((source_w as f32 * scale).round() as u32).max(1);
    let target_h = ((source_h as f32 * scale).round() as u32).max(1);
    let mut rgba = Vec::with_capacity(source.pixels.len() * 4);
    for pixel in &source.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }
    let image = image::RgbaImage::from_raw(source_w as u32, source_h as u32, rgba)
        .expect("ColorImage dimensions match its pixel buffer");
    let downsampled = image::imageops::resize(
        &image,
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    let blurred = image::imageops::blur(&downsampled, 6.0);
    ColorImage::from_rgba_unmultiplied(
        [blurred.width() as usize, blurred.height() as usize],
        blurred.as_raw(),
    )
}

/// Apply distance-readable typography and hit targets only to the Settings UI
/// subtree. The operator's persisted global `font_size_pt` remains untouched.
fn apply_settings_couch_style(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    for font in style.text_styles.values_mut() {
        font.size = (font.size * 1.35).max(28.0).min(44.0);
    }
    style.spacing.interact_size.y = style.spacing.interact_size.y.max(48.0);
    style.spacing.item_spacing = egui::vec2(12.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
}

/// One binding-cell button shared by the desktop table and narrow stacked
/// fallback. Empty bindings are normalized before this point to `Unassigned`.
fn settings_binding_button(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    height: f32,
    hover: &str,
) -> bool {
    ui.add_sized(
        [width, height],
        egui::Button::new(egui::RichText::new(text)),
    )
    .on_hover_text(hover)
    .clicked()
}

/// Content-size bounds for the Settings window. Keeping this pure makes the
/// viewport clamp independently regression-testable without a foreground GUI.
fn media_settings_sizes(screen: egui::Vec2) -> (egui::Vec2, egui::Vec2, egui::Vec2) {
    let available = egui::vec2((screen.x - 48.0).max(1.0), (screen.y - 48.0).max(1.0));
    let default_size = egui::vec2(760.0, 680.0).min(available);
    let min_size = egui::vec2(520.0, 400.0).min(default_size);
    (available, default_size, min_size)
}

/// Format a count with thousands separators ("12,438") for stable reading.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Middle-elide a string to at most `max` chars, keeping the extension end
/// visible ("very-long-image-name…1234.png"). Used for footer filenames.
fn elide_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 5 {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// Wrap `current + delta` into `[0, total)` so navigation round-trips at the ends
/// of a set (last -> first, first -> last). `total` of 0 returns 0.
fn wrap_relative_index(current: usize, delta: isize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let t = total as isize;
    (((current as isize + delta) % t + t) % t) as usize
}

fn fit_for_compare_frame(image_size: egui::Vec2, available: egui::Vec2) -> egui::Vec2 {
    if image_size.x <= 0.0 || image_size.y <= 0.0 {
        return available;
    }
    let fit_w = if available.x.is_finite() && available.x > 0.0 {
        available.x
    } else {
        image_size.x
    };
    let fit_h = if available.y.is_finite() && available.y > 0.0 {
        available.y
    } else {
        image_size.y
    };
    if fit_w <= 0.0 || fit_h <= 0.0 {
        return image_size;
    }
    let scale = (fit_w / image_size.x).min(fit_h / image_size.y);
    egui::vec2(
        (image_size.x * scale).max(1.0),
        (image_size.y * scale).max(1.0),
    )
}

fn collect_image_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    queue.push(entry_path);
                    continue;
                }
                if is_supported_image_path(&entry_path) {
                    out.push(entry_path.to_string_lossy().to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_playback_relocates_after_terminal_scan_sort() {
        let mut final_paths = vec![
            "Z:/videos/z-last.mp4".to_string(),
            "Z:/videos/a-first.mp4".to_string(),
        ];
        let progressive_index = 0;
        final_paths.sort();

        let key_for = |path: &str| path.replace('\\', "/").to_lowercase();
        let relocated = pending_path_index_in_sorted(
            &final_paths,
            "Z:/videos/z-last.mp4",
            "z:/videos/z-last.mp4",
            key_for,
        )
        .unwrap();
        assert_ne!(relocated, progressive_index);
        assert_eq!(final_paths[relocated], "Z:/videos/z-last.mp4");
        assert_eq!(
            pending_path_index_in_sorted(
                &final_paths,
                "Z:/videos/missing.mp4",
                "z:/videos/missing.mp4",
                key_for,
            ),
            None
        );
        assert_eq!(
            pending_path_index_in_sorted(
                &final_paths,
                "z:\\VIDEOS\\Z-LAST.mp4",
                "z:/videos/z-last.mp4",
                key_for,
            ),
            Some(relocated)
        );

        let workspace = Path::new("D:/workspace");
        let final_workspace_paths = vec!["d:/WORKSPACE/media/a.mp4".to_string()];
        let cached_workspace_path = "D:\\workspace\\MEDIA\\a.mp4";
        let cached_workspace_key = crate::media_db::canonical_key(workspace, cached_workspace_path);
        assert_eq!(
            pending_path_index_in_sorted(
                &final_workspace_paths,
                cached_workspace_path,
                &cached_workspace_key,
                |path| crate::media_db::canonical_key(workspace, path),
            ),
            Some(0)
        );
        assert!(keep_inline_video_awaiting(
            true,
            Some(std::time::Duration::from_secs(45))
        ));
        assert!(!keep_inline_video_awaiting(
            true,
            Some(std::time::Duration::from_secs(120))
        ));
        assert!(!keep_inline_video_awaiting(
            false,
            Some(std::time::Duration::from_secs(10))
        ));
        assert!(keep_inline_video_awaiting(
            false,
            Some(std::time::Duration::from_secs(9))
        ));
        let mut visible_then_hidden_age = Some(std::time::Duration::from_secs(45));
        if !preserve_inline_request_anchor(true) {
            visible_then_hidden_age = None;
        }
        assert!(keep_inline_video_awaiting(true, visible_then_hidden_age));
        assert!(!keep_inline_video_awaiting(
            true,
            Some(std::time::Duration::from_secs(120))
        ));
    }

    fn valid_surface(bounds: [i32; 4]) -> crate::video_player::NativeSurfaceDiagnostics {
        crate::video_player::NativeSurfaceDiagnostics {
            parent_hwnd: Some(1),
            child_hwnd: Some(2),
            parent_valid: true,
            child_valid: true,
            child_parent_matches: true,
            child_visible: true,
            target_bounds_px: Some(bounds),
            child_bounds_px: Some(bounds),
            libvlc_hwnd_matches: Some(true),
            last_error_code: None,
        }
    }

    #[test]
    fn explicit_video_play_actions_select_one_canonical_owner() {
        assert_eq!(explicit_video_owner("play"), ExplicitVideoOwner::Viewer);
        assert_eq!(
            explicit_video_owner("play_library"),
            ExplicitVideoOwner::Library
        );
        assert_eq!(
            explicit_video_owner("play_pause"),
            ExplicitVideoOwner::Preserve
        );
        assert_eq!(
            video_surface_owner(Some("a.mp4"), Some("a.mp4")),
            Some("library")
        );
        assert_eq!(video_surface_owner(Some("a.mp4"), None), Some("viewer"));
        assert_eq!(video_surface_owner(None, Some("a.mp4")), None);
    }

    #[test]
    fn model_snapshot_never_consumes_an_unowned_screenshot_reply() {
        assert!(!model_snapshot_owns_screenshot(false, false, false));
        assert!(!model_snapshot_owns_screenshot(false, true, false));
        assert!(!model_snapshot_owns_screenshot(true, true, false));
        assert!(model_snapshot_owns_screenshot(true, false, false));
        // WP-064: the folder-navigator backdrop capture is a modal capture and
        // owns the reply exactly as the Settings capture does.
        assert!(!model_snapshot_owns_screenshot(false, false, true));
        assert!(!model_snapshot_owns_screenshot(true, false, true));
        assert!(!model_snapshot_owns_screenshot(true, true, true));
    }

    /// WP-064 regression. Reproduced live against `facial.exe --background`:
    /// `media_folder_navigate --action open_new_tab` was rejected with
    /// "folder navigator is closed; send action=open first" whenever it arrived
    /// during the pre-open backdrop-capture window, because that window sets
    /// `show_folder_navigator = false`. The navigator must be treated as active
    /// for the whole request lifetime.
    #[test]
    fn folder_navigator_stays_active_across_its_backdrop_capture_window() {
        // Closed and idle: not active.
        assert!(!folder_navigator_is_active(false, false));
        // Requested, capture in flight, not yet visible: still active.
        assert!(folder_navigator_is_active(false, true));
        // Open with the captured backdrop already applied.
        assert!(folder_navigator_is_active(true, false));
        // Open while a further capture is pending.
        assert!(folder_navigator_is_active(true, true));
    }

    /// WP-065. A folder change must decide explicitly what happens to active
    /// playback. `start_compare_scan_internal` previously never touched the
    /// player, so a video kept decoding after its owning inventory was
    /// discarded and no owner ever placed the native surface again — the
    /// operator's "switching folder broke playback, audio only" report.
    #[test]
    fn folder_membership_decides_whether_playback_survives_a_folder_change() {
        let folder = r"D:\media\clips";
        // Direct child survives.
        assert!(path_is_inside_folder(folder, r"D:\media\clips\a.mp4"));
        // Subfolder survives, because a recursive scan keeps it.
        assert!(path_is_inside_folder(folder, r"D:\media\clips\nested\b.mp4"));
        // Separator style and case must not matter on Windows paths.
        assert!(path_is_inside_folder(folder, "d:/MEDIA/Clips/c.MP4"));
        assert!(path_is_inside_folder(r"D:\media\clips\", r"D:\media\clips\d.mp4"));
        // A sibling folder is not inside, even though it shares a prefix.
        assert!(!path_is_inside_folder(folder, r"D:\media\clips-old\e.mp4"));
        // A different tree is not inside.
        assert!(!path_is_inside_folder(folder, r"D:\other\f.mp4"));
        // The folder itself is not "inside" itself.
        assert!(!path_is_inside_folder(folder, folder));
        // An empty folder never retains playback.
        assert!(!path_is_inside_folder("", r"D:\media\clips\a.mp4"));
        // UNC shares behave the same way.
        assert!(path_is_inside_folder(
            r"\\nas\media",
            r"\\nas\media\show\g.mkv"
        ));
        assert!(!path_is_inside_folder(r"\\nas\media", r"\\nas\other\h.mkv"));
    }

    #[test]
    fn native_video_capture_requires_current_visible_matching_surface() {
        let valid = valid_surface([10, 20, 100, 50]);
        assert_eq!(
            current_surface_capture_region(&valid, [640, 480]).unwrap(),
            Some(SurfaceCaptureRegion {
                full: [10, 20, 100, 50],
                visible: [10, 20, 100, 50],
                source_offset: [0, 0],
            })
        );

        let mut hidden = valid;
        hidden.child_visible = false;
        assert_eq!(
            current_surface_capture_region(&hidden, [640, 480]).unwrap(),
            None
        );

        let mut wrong_parent = valid;
        wrong_parent.child_parent_matches = false;
        assert!(current_surface_capture_region(&wrong_parent, [640, 480]).is_err());

        let mut detached = valid;
        detached.libvlc_hwnd_matches = Some(false);
        assert!(current_surface_capture_region(&detached, [640, 480]).is_err());

        let mut stale = valid;
        stale.child_bounds_px = Some([11, 20, 100, 50]);
        assert!(current_surface_capture_region(&stale, [640, 480]).is_err());
    }

    #[test]
    fn native_video_capture_clips_partial_surface_without_rescaling_hidden_pixels() {
        let surface = valid_surface([-20, -10, 100, 50]);
        assert_eq!(
            current_surface_capture_region(&surface, [640, 480]).unwrap(),
            Some(SurfaceCaptureRegion {
                full: [-20, -10, 100, 50],
                visible: [0, 0, 80, 40],
                source_offset: [20, 10],
            })
        );
    }

    fn legacy_media_paths(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut queue = vec![root.to_path_buf()];
        let mut seen = HashSet::new();
        while let Some(path) = queue.pop() {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen.insert(canonical) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = path.metadata() else {
                    continue;
                };
                if metadata.is_dir() {
                    queue.push(path);
                } else if is_supported_image_path(&path) || is_supported_video_path(&path) {
                    out.push(path.to_string_lossy().to_string());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn wrap_relative_index_round_trips_at_ends() {
        // Wraps past the end -> first, before the start -> last (round trip).
        assert_eq!(wrap_relative_index(9, 1, 10), 0);
        assert_eq!(wrap_relative_index(0, -1, 10), 9);
        // Normal steps in range are unaffected.
        assert_eq!(wrap_relative_index(4, 1, 10), 5);
        assert_eq!(wrap_relative_index(4, -1, 10), 3);
        // Multi-step wrap, single-item, and empty sets.
        assert_eq!(wrap_relative_index(8, 5, 10), 3);
        assert_eq!(wrap_relative_index(0, 0, 1), 0);
        assert_eq!(wrap_relative_index(0, 1, 0), 0);
    }

    #[test]
    fn autocomplete_publication_rejects_cancelled_and_stale_requests() {
        let index_key = MediaSearchIndexKey {
            lane_id: 7,
            scan_id: 11,
            content_generation: 13,
            inventory_generation: Some(17),
            meta_generation: 19,
        };
        let request = MediaSuggestionRequestKey {
            index_key: index_key.clone(),
            folder: "//nas/media".to_string(),
            query: "hero".to_string(),
        };
        assert!(media_suggestion_result_is_current(
            &request,
            Some(&index_key),
            Some(&index_key),
            "hero",
            "//nas/media",
            false,
        ));
        assert!(!media_suggestion_result_is_current(
            &request,
            Some(&index_key),
            Some(&index_key),
            "hero",
            "//nas/media",
            true,
        ));
        let mut stale_index = index_key.clone();
        stale_index.content_generation += 1;
        assert!(!media_suggestion_result_is_current(
            &request,
            Some(&stale_index),
            Some(&index_key),
            "hero",
            "//nas/media",
            false,
        ));
        assert!(!media_suggestion_result_is_current(
            &request,
            Some(&index_key),
            Some(&index_key),
            "heroine",
            "//nas/media",
            false,
        ));
        assert!(!media_background_index_work_allowed(true, false));
        assert!(media_background_index_work_allowed(true, true));
        assert!(media_background_index_work_allowed(false, false));
    }

    #[test]
    fn media_settings_size_is_clamped_to_each_viewport() {
        let (available, preferred, minimum) = media_settings_sizes(egui::vec2(1280.0, 800.0));
        assert_eq!(available, egui::vec2(1232.0, 752.0));
        assert_eq!(preferred, egui::vec2(760.0, 680.0));
        assert_eq!(minimum, egui::vec2(520.0, 400.0));

        let (available, preferred, minimum) = media_settings_sizes(egui::vec2(760.0, 520.0));
        assert_eq!(available, egui::vec2(712.0, 472.0));
        assert_eq!(preferred, available);
        assert_eq!(minimum, egui::vec2(520.0, 400.0));

        let (available, preferred, minimum) = media_settings_sizes(egui::vec2(420.0, 300.0));
        assert_eq!(available, egui::vec2(372.0, 252.0));
        assert_eq!(preferred, available);
        assert_eq!(minimum, available);
    }

    #[test]
    fn gaussian_settings_backdrop_is_bounded_and_does_not_tint_color() {
        let source = ColorImage::new([1600, 900], egui::Color32::from_rgb(31, 117, 203));
        let blurred = gaussian_settings_backdrop(&source, 640);
        assert_eq!(blurred.size, [640, 360]);
        assert!(blurred
            .pixels
            .iter()
            .all(|pixel| pixel.to_array() == [31, 117, 203, 255]));
    }

    #[test]
    fn sanitize_folder_input_strips_quotes_and_space() {
        assert_eq!(sanitize_folder_input("  D:/a/b  "), "D:/a/b");
        assert_eq!(sanitize_folder_input("\"D:/a b/c\""), "D:/a b/c");
        assert_eq!(sanitize_folder_input("'D:/a/c'"), "D:/a/c");
        assert_eq!(sanitize_folder_input("D:/a/c"), "D:/a/c");
    }

    #[test]
    fn group_thousands_formats_counts() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(12_438), "12,438");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn elide_middle_keeps_ends() {
        assert_eq!(elide_middle("short.png", 38), "short.png");
        let long = "a-very-long-image-file-name-from-a-shoot-20260611-0042.png";
        let out = elide_middle(long, 38);
        assert!(out.chars().count() <= 38);
        assert!(out.contains('…'));
        assert!(out.ends_with("0042.png"));
        assert!(out.starts_with("a-very-long"));
    }

    #[test]
    fn fit_for_compare_frame_preserves_aspect() {
        let down = fit_for_compare_frame(egui::vec2(2000.0, 1000.0), egui::vec2(500.0, 500.0));
        assert!((down.x - 500.0).abs() < 0.01);
        assert!((down.y - 250.0).abs() < 0.01);
        let up = fit_for_compare_frame(egui::vec2(100.0, 100.0), egui::vec2(300.0, 200.0));
        assert!((up.x - 200.0).abs() < 0.01);
        assert!((up.y - 200.0).abs() < 0.01);
    }

    #[test]
    fn compare_tab_keeps_operator_name_and_vocab() {
        assert_eq!(Tab::Compare.label(), "Compare");
        assert_eq!(Tab::Compare.vocab(), "compare");
        assert!(matches!(Tab::from_vocab("compare"), Some(Tab::Compare)));
        assert!(matches!(Tab::from_vocab("lanes"), Some(Tab::Compare)));
    }

    #[test]
    fn media_scan_emits_bounded_progressive_batches() {
        let root = std::env::temp_dir().join(format!("facial-media-scan-{}", uuid::Uuid::new_v4()));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        for index in 0..600usize {
            std::fs::write(nested.join(format!("image-{index:04}.jpg")), b"").unwrap();
        }
        std::fs::write(nested.join("clip.mp4"), b"").unwrap();
        std::fs::write(nested.join("ignored.txt"), b"").unwrap();

        let mut batches: Vec<Vec<String>> = Vec::new();
        let (all, errors) =
            collect_media_paths_for_compare(&root, true, MediaFilterMode::All, |batch| {
                batches.push(batch)
            })
            .unwrap();

        assert_eq!(errors, 0);
        assert_eq!(all.len(), 601);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 64);
        assert_eq!(batches[1].len(), 537);
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), all.len());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn media_scan_cancellation_stops_before_complete_result() {
        let root =
            std::env::temp_dir().join(format!("facial-media-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..300usize {
            std::fs::write(root.join(format!("image-{index:04}.jpg")), b"").unwrap();
        }
        let mut checks = 0usize;
        let result = collect_media_paths_for_compare_cancellable(
            &root,
            true,
            MediaFilterMode::All,
            || {
                checks += 1;
                checks > 24
            },
            |_| {},
        );
        assert_eq!(result, Err(MediaScanFailure::Cancelled));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_zero_error_scan_is_authoritative() {
        let root =
            std::env::temp_dir().join(format!("facial-media-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let mut batches = Vec::new();
        let result = collect_media_paths_for_compare(&root, true, MediaFilterMode::All, |batch| {
            batches.push(batch)
        })
        .expect("a readable empty folder is a complete scan");

        assert_eq!(result, (Vec::new(), 0));
        assert!(batches.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn broken_symlink_marks_scan_incomplete_when_supported() {
        let root =
            std::env::temp_dir().join(format!("facial-media-broken-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("kept.jpg"), b"").unwrap();
        let missing = root.join("missing.jpg");
        let link = root.join("broken.jpg");
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&missing, &link).is_ok();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&missing, &link).is_ok();
        #[cfg(not(any(windows, unix)))]
        let linked = false;

        let (files, errors) =
            collect_media_paths_for_compare(&root, true, MediaFilterMode::All, |_| {}).unwrap();
        assert_eq!(files.len(), 1);
        if linked {
            assert_eq!(errors, 1, "broken link must prevent authoritative commit");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn zero_media_with_a_broken_link_is_incomplete_when_supported() {
        let root = std::env::temp_dir().join(format!(
            "facial-media-empty-broken-link-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let missing = root.join("missing.jpg");
        let link = root.join("broken.jpg");
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&missing, &link).is_ok();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&missing, &link).is_ok();
        #[cfg(not(any(windows, unix)))]
        let linked = false;

        let (files, errors) =
            collect_media_paths_for_compare(&root, true, MediaFilterMode::All, |_| {}).unwrap();
        assert!(files.is_empty());
        if linked {
            assert_eq!(
                errors, 1,
                "a zero-file partial scan must not look like an authoritative empty folder"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn optimized_scan_matches_legacy_path_set_including_unicode_and_available_links() {
        let root =
            std::env::temp_dir().join(format!("facial-media-exact-{}", uuid::Uuid::new_v4()));
        let real = root.join("실제-folder");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("α-image.JPG"), b"").unwrap();
        std::fs::write(real.join("clip.MKV"), b"").unwrap();
        std::fs::write(real.join("ignored.txt"), b"").unwrap();
        let linked_target = std::env::temp_dir().join(format!(
            "facial-media-exact-link-target-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&linked_target).unwrap();
        std::fs::write(linked_target.join("linked-image.png"), b"").unwrap();
        let link = root.join("linked-folder");
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&linked_target, &link).is_ok();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&linked_target, &link).is_ok();
        #[cfg(not(any(windows, unix)))]
        let linked = false;

        let expected = legacy_media_paths(&root);
        let (mut actual, errors) =
            collect_media_paths_for_compare(&root, true, MediaFilterMode::All, |_| {}).unwrap();
        actual.sort();
        assert_eq!(errors, 0);
        assert_eq!(actual, expected, "optimized traversal changed the path set");
        if linked {
            assert!(
                actual.iter().any(|path| path.contains("linked-folder")),
                "available directory links must participate in exact-set proof"
            );
        }
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(linked_target);
    }

    /// Operator/model diagnostic for real large media trees. Run explicitly:
    /// `FACIAL_LARGE_MEDIA_TEST_DIR=<dir> FACIAL_EXPECT_MEDIA_COUNT=<n>
    ///  cargo test --manifest-path product/Cargo.toml large_media_scan_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "requires FACIAL_LARGE_MEDIA_TEST_DIR"]
    fn large_media_scan_probe() {
        let root = std::env::var("FACIAL_LARGE_MEDIA_TEST_DIR")
            .expect("FACIAL_LARGE_MEDIA_TEST_DIR must name the media tree");
        let expected = std::env::var("FACIAL_EXPECT_MEDIA_COUNT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let started = std::time::Instant::now();
        let mut batch_count = 0usize;
        let mut first_batch_at = None;
        let (mut all, errors) = collect_media_paths_for_compare(
            Path::new(&root),
            true,
            MediaFilterMode::All,
            |batch| {
                batch_count += 1;
                if first_batch_at.is_none() {
                    first_batch_at = Some(started.elapsed());
                }
                assert!(!batch.is_empty() && batch.len() <= 1_024);
                if batch_count == 1 && batch.len() > 64 {
                    panic!("first batch must stay small, got {}", batch.len());
                }
            },
        )
        .expect("large media scan succeeds");
        all.sort();
        let optimized_elapsed = started.elapsed();
        let legacy_started = std::time::Instant::now();
        let expected_paths = legacy_media_paths(Path::new(&root));
        assert_eq!(
            all, expected_paths,
            "optimized large-tree scan changed the exact sorted path set"
        );
        assert_eq!(
            errors, 0,
            "exact NAS proof requires a complete readable scan"
        );
        if let Some(expected) = expected {
            assert_eq!(all.len(), expected, "canonical supported-media count");
        }
        println!(
            "large_media_scan_probe root={root:?} media={} batches={} first_batch_ms={} optimized_total_ms={} legacy_total_ms={} exact_set=true dir_errors={errors}",
            all.len(),
            batch_count,
            first_batch_at.unwrap_or_default().as_millis(),
            optimized_elapsed.as_millis(),
            legacy_started.elapsed().as_millis(),
        );
        assert!(
            batch_count > 1,
            "large trees must publish progressive batches"
        );
    }
}

impl FacialApp {
    /// Set which tab the next `render_ui` draws. Used by the headless inspector.
    pub fn set_active_tab(&mut self, tab: Tab) {
        self.active_tab = tab;
    }

    /// Headless-inspector hook: force the in-app folder browser open for a
    /// lane so dialog layout can be captured without pointer input.
    pub fn debug_open_folder_picker(&mut self, lane_id: usize) {
        self.folder_picker.open(lane_id, "");
    }

    /// Headless-inspector hook: close the folder browser again so later
    /// captures (media presets) are not shadowed by the floating dialog.
    pub fn debug_close_folder_picker(&mut self) {
        self.folder_picker.close();
    }

    /// Headless-inspector hook (WP-044): load a media fixture directly into
    /// the explorer lane (no async scan) and DISABLE the thumbnail engine so
    /// every tile renders its deterministic placeholder — snapshots stay
    /// byte-identical across runs regardless of decode-thread timing.
    pub fn debug_media_load_fixture(&mut self, folder: &str, files: Vec<String>) {
        if self.compare_lanes.is_empty() {
            self.compare_lanes = vec![CompareLane::new(0)];
            self.compare_next_lane_id = 1;
        }
        self.thumb_engine = None;
        self.debug_preview_fixture = false;
        let fixture_count = files.len();
        let lane = &mut self.compare_lanes[0];
        lane.folder = folder.to_string();
        lane.files = Arc::new(files);
        lane.scanning = false;
        lane.scan_error.clear();
        lane.selected_files = [2usize, 3usize].into_iter().collect();
        lane.selection_anchor = Some(2);
        lane.index = 3;
        self.media_explorer.cursor = Some(3);
        self.media_explorer.tile_edge = 500.0;
        self.media_explorer.show_names = false;
        self.media_explorer.show_settings = false;
        self.media_explorer.show_favorites = false;
        self.media_explorer.show_folder_navigator = false;
        self.media_explorer.folder_navigator_location.clear();
        self.media_explorer.folder_cursor = None;
        self.media_explorer.folder_scroll_to_cursor = false;
        self.media_child_folder_cache.clear();
        self.media_folder_entry_cache.clear();
        self.media_child_folder_inflight.clear();
        self.media_child_folder_cancel.clear();
        // Inspector fixtures are already local, bounded, and created by the
        // inspector itself. Seed the background cache here so deterministic
        // headless passes do not race an OS thread; live rendering never uses
        // this synchronous debug-only path.
        let mut fixture_children = Vec::new();
        if let Ok(entries) = fs::read_dir(Path::new(folder)) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    if let Some(name) = entry.file_name().to_str() {
                        fixture_children.push(name.to_string());
                    }
                }
            }
            fixture_children.sort_unstable();
        }
        self.media_folder_entry_cache.insert(
            folder.to_string(),
            Arc::new(crate::media_explorer::prepare_folder_entries(
                folder,
                &fixture_children,
            )),
        );
        self.media_child_folder_cache
            .insert(folder.to_string(), Arc::new(fixture_children));
        self.media_search_query.clear();
        self.media_semantic = None;
        self.media_semantic_inflight = None;
        self.media_semantic_generation = self.media_semantic_generation.wrapping_add(1);
        self.media_content_generation = self.media_content_generation.wrapping_add(1);
        // Give the first inspector frame the exact fixture order. Live Media
        // intentionally resolves display order off-thread, but deterministic
        // visual proof must not capture the transient empty/stale cache while
        // that worker races a three-pass headless render.
        self.media_display_cache = Arc::new((0..fixture_count).collect());
        self.media_display_cache_key = None;
        self.active_tab = Tab::Media;
    }

    #[doc(hidden)]
    pub fn debug_media_add_inactive_tab(&mut self, folder: &str) {
        // WP-064: the multi-tab regression fixture needs more than two tabs, so
        // this is capped at a small deterministic count rather than exactly one
        // extra tab. Existing presets that add a single tab are unaffected.
        if self.media_tabs.tabs().len() >= 4 {
            return;
        }
        self.snapshot_active_media_tab();
        let original = self.media_tabs.active_id().clone();
        let key = self.media_db.key_for(folder);
        if self.media_tabs.open_folder_in_new_tab(key).is_ok() {
            let _ = self.media_tabs.activate(&original);
        }
    }

    /// Headless-inspector hook (WP-061): seed an arbitrary catalog and
    /// ordered multi-label assignments entirely in the existing in-memory
    /// caches. The render path therefore exercises the same bounded
    /// path-to-small-vector lookups as the live UI without writing or reading
    /// metadata while a frame is painted.
    pub fn debug_media_seed_label_fixture(&mut self, files: &[String], catalog_size: usize) {
        const NAMED: [(&str, &str, &str); 5] = [
            ("fixture-selects", "Selects", "#D9534F"),
            ("fixture-review", "Needs review", "#F0AD4E"),
            ("fixture-motion", "Motion", "#5BC0DE"),
            ("fixture-approved", "Approved", "#5CB85C"),
            ("fixture-export", "Ready to export", "#7B61A8"),
        ];
        let count = catalog_size.max(NAMED.len());
        let mut definitions = Vec::with_capacity(count);
        for (id, name, hex) in NAMED {
            definitions.push(crate::media_db::ColorLabelDefinition {
                id: id.to_string(),
                name: name.to_string(),
                hex: hex.to_string(),
            });
        }
        for index in NAMED.len()..count {
            // Deterministic, canonical, unique colors for the inspector-only
            // overflow rows. The IDs remain stable across repeated captures.
            let red = 32u8.wrapping_add((index as u8).wrapping_mul(37));
            let green = 64u8.wrapping_add((index as u8).wrapping_mul(53));
            let blue = 96u8.wrapping_add((index as u8).wrapping_mul(71));
            definitions.push(crate::media_db::ColorLabelDefinition {
                id: format!("fixture-catalog-{index:02}"),
                name: format!("Catalog label {:02}", index + 1),
                hex: format!("#{red:02X}{green:02X}{blue:02X}"),
            });
        }

        let assigned_ids: Vec<String> = definitions
            .iter()
            .take(NAMED.len())
            .map(|definition| definition.id.clone())
            .collect();
        let mut assignments = BTreeMap::new();
        let mut usage = BTreeMap::new();
        for (index, path) in files.iter().take(8).enumerate() {
            // Keep both the first visible Library tile and the selected Viewer
            // item at five assignments so the bounded three-dot +N lane and
            // the full Viewer chip row are simultaneously inspectable.
            let assigned_count = if index == 0 || index == 3 {
                NAMED.len()
            } else {
                1 + index % NAMED.len()
            };
            let ids = assigned_ids[..assigned_count].to_vec();
            for id in &ids {
                *usage.entry(id.clone()).or_insert(0) += 1;
            }
            assignments.insert(self.media_db.key_for(path), ids);
        }
        self.media_label_definitions = definitions;
        self.refresh_media_label_colors();
        self.media_color_labels = Arc::new(assignments);
        self.media_label_usage_counts = usage;
        self.media_label_create_name = "New collection".to_string();
        self.media_label_create_rgb = [45, 160, 110];
        self.media_label_create_for_key = None;
        self.media_label_delete_confirm = None;
    }

    /// Structured companion gate for the WP-061 20+ catalog visual fixture.
    pub fn debug_media_label_catalog_len(&self) -> usize {
        self.media_label_definitions.len()
    }

    /// WP-061 performance fixture: populate a real 50k-scale in-memory
    /// path-to-multi-label cache before timing begins. No persistence method is
    /// called here or by the tile paint lane.
    pub fn debug_media_seed_label_performance_fixture(&mut self, files: &[String]) {
        self.debug_media_seed_label_fixture(files, 5);
        let ids: Vec<String> = self
            .media_label_definitions
            .iter()
            .take(5)
            .map(|definition| definition.id.clone())
            .collect();
        let mut assignments = BTreeMap::new();
        for path in files {
            assignments.insert(self.media_db.key_for(path), ids.clone());
        }
        self.media_color_labels = Arc::new(assignments);
        self.media_label_usage_counts = ids.into_iter().map(|id| (id, files.len())).collect();
    }

    /// Same 50k-key metadata-cache shape as the candidate, with empty vectors
    /// for the no-label baseline so the A/B delta isolates bounded badge work
    /// instead of measuring a missing-map fast path.
    pub fn debug_media_seed_empty_label_performance_fixture(&mut self, files: &[String]) {
        self.debug_media_seed_label_performance_fixture(files);
        for labels in Arc::make_mut(&mut self.media_color_labels).values_mut() {
            labels.clear();
        }
        self.media_label_usage_counts.clear();
    }

    pub fn debug_label_paint_probe_start(&mut self) {
        self.debug_label_paint_probe = Some(0);
    }

    /// Return visible-tile cache lookups from the measured label-paint
    /// interval, disabling the live counter immediately afterward.
    pub fn debug_label_paint_probe_finish(&mut self) -> u64 {
        self.debug_label_paint_probe.take().unwrap_or(0)
    }

    /// Deterministic preview texture for visual proof. The normal inspector
    /// disables decode workers, so without this seam the Viewer panel would be
    /// blank and could not prove fitted/fullscreen surface use.
    pub fn debug_media_set_preview_fixture(&mut self, ctx: &egui::Context) {
        self.debug_preview_fixture = true;
        let size = [640usize, 360usize];
        let mut pixels = Vec::with_capacity(size[0] * size[1]);
        for y in 0..size[1] {
            for x in 0..size[0] {
                let band = ((x / 80) + (y / 60)) % 2;
                pixels.push(if band == 0 {
                    egui::Color32::from_rgb(72, 116, 164)
                } else {
                    egui::Color32::from_rgb(194, 137, 86)
                });
            }
        }
        if let Some(lane) = self.compare_lanes.first_mut() {
            lane.texture = Some(ctx.load_texture(
                "inspector-media-preview",
                ColorImage { size, pixels },
                TextureOptions::LINEAR,
            ));
            lane.loading_image = false;
            lane.loading_image_inflight = false;
            lane.image_error.clear();
        }
    }

    /// Headless-inspector hook (WP-044): force view mode + chrome visibility.
    pub fn debug_media_set_view(&mut self, full_grid: bool, chrome_hidden: bool) {
        self.media_explorer.view_mode = if full_grid {
            crate::media_explorer::MediaViewMode::FullGrid
        } else {
            crate::media_explorer::MediaViewMode::TwoPanel
        };
        self.media_explorer.chrome_hidden = chrome_hidden;
    }

    /// Headless-inspector hook (WP-050): filename visibility preset.
    pub fn debug_media_set_names(&mut self, show_names: bool) {
        self.media_explorer.show_names = show_names;
        // Keep the normal/default preset at 500 points, but reduce the
        // dedicated names-on proof enough that the caption itself lands in
        // the 1280x800 inspector viewport.
        self.media_explorer.tile_edge = if show_names { 360.0 } else { 500.0 };
    }

    /// Headless-inspector hook: shrink tiles so a caption-proof preset can fit
    /// every fixture row on one 1280x800 screen (WP-070 international names).
    pub fn debug_media_set_tile_edge(&mut self, edge: f32) {
        self.media_explorer.tile_edge = edge.clamp(64.0, 512.0);
    }

    /// Headless-inspector hook: select one exact raw fixture index.
    pub fn debug_media_select_index(&mut self, index: usize) {
        if let Some(lane) = self.compare_lanes.first_mut() {
            if index < lane.files.len() {
                lane.selected_files.clear();
                lane.selected_files.insert(index);
                lane.selection_anchor = Some(index);
                lane.index = index;
                self.media_explorer.cursor = Some(index);
            }
        }
    }

    /// Headless-inspector hook (WP-050): readable settings-window preset.
    pub fn debug_media_show_settings(&mut self, show: bool) {
        self.media_explorer.show_settings = show;
        if show {
            self.media_explorer.show_favorites = false;
        } else {
            self.media_explorer.settings_couch_fullscreen = false;
        }
    }

    /// Headless-inspector hook (WP-062): enter/leave the transient Settings
    /// couch surface without mutating the saved application font size.
    pub fn debug_media_set_settings_couch(&mut self, couch: bool, prior_fullscreen: bool) {
        self.media_explorer.settings_couch_fullscreen = couch;
        self.media_explorer.settings_couch_prior_fullscreen = prior_fullscreen;
        self.media_explorer.show_settings = true;
        self.media_explorer.show_favorites = false;
        self.close_media_folder_navigator();
    }

    pub fn debug_media_settings_couch(&self) -> bool {
        self.media_explorer.settings_couch_fullscreen
    }

    /// Headless-inspector hook: select a unified Settings category without
    /// requiring synthetic pointer input.
    pub fn debug_media_set_settings_category(&mut self, category: u8) {
        self.media_explorer.settings_category = category.min(3);
    }

    pub fn debug_media_settings_category(&self) -> u8 {
        self.media_explorer.settings_category
    }

    /// Headless-inspector hook (WP-055): exercise Settings at an elevated
    /// operator font scale without persisting a test-only preference.
    pub fn debug_media_set_font_size(&mut self, ctx: &egui::Context, points: f32) {
        self.font_size_pt = points.clamp(12.0, 40.0);
        theme::apply_text_styles(ctx, self.font_size_pt);
    }

    /// Structured close-state proof for backdrop/Escape inspector actions.
    pub fn debug_media_settings_visible(&self) -> bool {
        self.media_explorer.show_settings
    }

    /// Headless-inspector proof that a modal backdrop click did not activate
    /// a navigation control underneath it.
    pub fn debug_active_tab(&self) -> Tab {
        self.active_tab
    }

    /// Headless-inspector hook for non-pointer navigation boundary checks.
    pub fn debug_set_active_tab(&mut self, tab: Tab) {
        self.active_tab = tab;
    }

    /// Headless-inspector hook (WP-051): couch-distance Folders window.
    pub fn debug_media_show_folder_navigator(&mut self, show: bool, cursor: usize) {
        self.media_explorer.show_folder_navigator = show;
        if show {
            let active = self
                .compare_lanes
                .first()
                .map(|lane| sanitize_folder_input(&lane.folder))
                .unwrap_or_default();
            self.media_explorer.folder_navigator_location = active.clone();
            self.media_explorer.folder_location_input = active;
        }
        self.media_explorer.folder_cursor = if show {
            self.compare_lanes
                .first()
                .map(|lane| lane.id)
                .and_then(|lane_id| {
                    let entries = self.media_folder_entries(lane_id);
                    let drive_count = entries.iter().take_while(|entry| entry.is_drive).count();
                    let folder_count = entries.len().saturating_sub(drive_count);
                    if folder_count > 0 {
                        Some(drive_count + cursor.min(folder_count - 1))
                    } else {
                        (!entries.is_empty()).then_some(0)
                    }
                })
        } else {
            None
        };
        self.media_explorer.folder_scroll_to_cursor = show;
        if show {
            self.media_explorer.show_settings = false;
            self.media_explorer.show_favorites = false;
        } else {
            self.folder_navigator_backdrop = None;
            self.folder_navigator_backdrop_requested_at = None;
        }
    }

    /// Model-safe state proof for staged folder browsing. The active folder,
    /// scan generation, and loaded row count are reported separately from the
    /// transient navigator path so a no-context inspector can detect any
    /// accidental browse-time commit.
    pub fn debug_media_folder_navigator_state(&self) -> serde_json::Value {
        let lane = self.compare_lanes.first();
        serde_json::json!({
            "visible": self.media_explorer.show_folder_navigator,
            "active_folder": lane.map(|lane| sanitize_folder_input(&lane.folder)).unwrap_or_default(),
            "staged_folder": sanitize_folder_input(&self.media_explorer.folder_navigator_location),
            "active_scan_id": lane.map(|lane| lane.scan_id).unwrap_or_default(),
            "active_file_count": lane.map(|lane| lane.files.len()).unwrap_or_default(),
            "cursor": self.media_explorer.folder_cursor,
            "backdrop": if self.folder_navigator_backdrop.is_some() { "gaussian" } else { "neutral_fallback" },
        })
    }

    /// Exercise the same staged Enter transition used by mouse, keyboard,
    /// controller, and `media_folder_navigate`, without foreground input.
    pub fn debug_media_folder_navigator_enter(&mut self) {
        if let Some(lane_id) = self.compare_lanes.first().map(|lane| lane.id) {
            self.media_navigator_enter(lane_id);
        }
    }

    /// Build the full frame: header, the active tab body, and any floating
    /// in-app dialog. Shared by the live `update()` and the headless GUI
    /// inspector so both draw the identical UI. No `eframe::Frame` dependency,
    /// so it runs offscreen.
    pub fn render_ui(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.pending_system_clipboard.take() {
            ctx.output_mut(|output| output.copied_text = text);
        }
        // Native fullscreen and the Settings overlay are Media-only. A model
        // intent can change tabs without going through the modal backdrop, so
        // it must also unwind transient couch fullscreen or Escape would no
        // longer have a visible Settings surface through which to restore it.
        if self.active_tab != Tab::Media {
            if self.media_explorer.settings_couch_fullscreen {
                self.exit_settings_couch_fullscreen(ctx);
                self.media_explorer.show_settings = false;
                self.settings_backdrop = None;
                self.settings_backdrop_requested_at = None;
            }
            if self.media_explorer.chrome_hidden {
                self.media_explorer.chrome_hidden = false;
                self.media_explorer.chrome_hidden_at = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        }
        // Fullscreen (WP-050): Ctrl+F strips app chrome and sends the root
        // viewport borderless-fullscreen; Esc/Ctrl+F restores.
        let hide_chrome = self.active_tab == Tab::Media && self.media_explorer.chrome_hidden;
        if !hide_chrome {
            egui::TopBottomPanel::top("header")
                .frame(
                    egui::Frame::none()
                        .fill(theme::sheet())
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
                )
                .show(ctx, |ui| {
                    self.draw_header(ui);
                });

            egui::TopBottomPanel::bottom("status_bar")
                .frame(
                    egui::Frame::none()
                        .fill(theme::sheet())
                        .inner_margin(egui::Margin::symmetric(12.0, 4.0)),
                )
                .show(ctx, |ui| {
                    self.draw_status_bar(ui);
                });
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::desk())
                    .inner_margin(egui::Margin::same(12.0)),
            )
            .show(ctx, |ui| {
                // Rough paper grain under everything (WP-048); widgets paint
                // above it because they allocate later in the same layer.
                theme::paint_grain(ui.painter(), ui.max_rect().expand(12.0), &self.grain);
                // Compare and Media size themselves to the viewport (images
                // own the space); every other tab scrolls vertically so
                // content taller than the window is never unreachable.
                if self.active_tab == Tab::Compare {
                    self.draw_compare_tab(ui);
                } else if self.active_tab == Tab::Media {
                    self.draw_media_tab(ui);
                } else {
                    ScrollArea::vertical()
                        .id_source("tab_body_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| match self.active_tab {
                            Tab::Media | Tab::Compare => unreachable!(),
                            Tab::Project => self.draw_project_tab(ui),
                            Tab::QualityIq => self.draw_quality_tab(ui),
                            Tab::Identity => self.draw_identity_tab(ui),
                            Tab::Duplicates => self.draw_duplicates_tab(ui),
                            Tab::RunDebug => self.draw_run_debug_tab(ui),
                            Tab::Manual => self.draw_manual_tab(ui),
                            Tab::Options => self.draw_options_tab(ui),
                        });
                }
            });

        // In-app folder browser (WP-014): floats above the panels and renders
        // through this same path, so it never leaves the app window and shows
        // up in headless inspector snapshots.
        if let PickerEvent::Picked { lane_id, folder } = self.folder_picker.show(ctx) {
            if let Some(pos) = self.compare_lane_position(lane_id) {
                self.compare_lanes[pos].folder = folder.to_string_lossy().to_string();
            }
            self.start_compare_scan(lane_id);
        }
    }
}

impl eframe::App for FacialApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frame_started = std::time::Instant::now();
        self.handle_settings_backdrop_capture(ctx);
        self.handle_folder_navigator_backdrop_capture(ctx);
        if self.active_tab != Tab::Media {
            self.video_player.stop();
            self.media_inline_video_path = None;
            self.media_inline_video_requested_at = None;
            self.media_inline_video_pending_target = None;
            self.media_playback_lease = None;
        }
        // Debounced media-metadata write-through (WP-042).
        let _ = self.flush_media_metadata(false);
        self.handle_events(ctx);
        let _applied = self.poll_and_apply_model_intent(ctx);
        self.handle_model_snapshot_capture(ctx);
        // Bounded poll for file-based model intents; no busy loop, no focus grab.
        // 1s idle cadence keeps idle CPU near zero (WP-010); while a scan/decode
        // or pipeline is in flight, poll at 100ms so results appear promptly.
        let busy = self.running_pipeline
            || self.media_display_inflight.is_some()
            || self.media_search_index_inflight.is_some()
            || self.media_explorer.stats_loading
            || !self.media_child_folder_inflight.is_empty()
            || self
                .compare_lanes
                .iter()
                .any(|lane| lane.scanning || lane.loading_image || lane.loading_image_inflight);
        let mut cadence = if busy { 100 } else { 1000 };
        // Controller liveness (WP-046): gilrs is polled during media frames,
        // so while a pad is connected and the Media tab is active, keep a
        // bounded ~20fps repaint schedule (idle CPU stays flat with no pad —
        // the WP-010 guarantee is scoped to the no-controller case).
        if self.active_tab == Tab::Media
            && (self.controller_active.is_some() || self.controller_legacy_active)
        {
            cadence = cadence.min(50);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(cadence));

        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.refresh_all();
        }

        self.render_ui(ctx);
        self.media_ui_frame_last_us = frame_started.elapsed().as_micros() as u64;
        self.media_ui_frame_max_us = self.media_ui_frame_max_us.max(self.media_ui_frame_last_us);
    }

    /// eframe persistence hook (also runs at shutdown): force-flush pending
    /// media metadata so a quick close never loses the debounce window.
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.snapshot_active_media_tab();
        if let Err(error) = self.write_media_tabs() {
            self.compare_action_message = format!("Media tabs save failed: {error}");
        }
        let _ = self.flush_media_metadata(true);
    }
}

fn format_media_time(milliseconds: i64) -> String {
    let total_seconds = milliseconds.max(0) / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
