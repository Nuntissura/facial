//! File-based command + receipt protocol and shared API types.
//!
//! This module is the single owner of all protocol/shared types
//! (`Command`, `CommandKind`, `Receipt`, `ActionStatus`, `AppStateSnapshot`,
//! `ApiPaths`) consumed by `ui.rs` and `main.rs`. There is no socket layer and
//! no OS-window interaction: backend models drive the app by dropping command
//! files into `<api_root>/commands/` and reading the resulting receipts. The
//! GUI applies ui-intents from `<api_root>/intents/` on its own frames.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::lanes::{LaneBatchAggregate, LaneRecord};
use crate::media_db::MediaDb;
use crate::models::ModelRecord;
use crate::plugin_host::PluginManifest;
use crate::service::FacialService;

pub const API_PROTOCOL_VERSION: u32 = 1;

// ---------- status ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Ok,       // backend command executed to completion
    Error,    // backend command failed
    Accepted, // ui-intent validated + persisted to intents/, awaiting GUI apply
    Applied,  // ui-intent applied by a live GUI frame
    Rejected, // command refused (bad vocab, path escape, run already active, etc.)
}

// ---------- command ----------

/// Wire enum. `#[serde(tag = "kind")]` => the JSON object carries a flat
/// "kind" discriminator alongside the variant fields (see §1.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandKind {
    // ---- backend-executable (run fully headless; terminal Receipt) ----
    ListFeatures,
    ListModels,
    ListWorktrees,
    GetState,
    StartRun {
        project_name: String,
        image_paths: Vec<String>,
        feature_keys: Vec<String>,
        #[serde(default)]
        worktree_path: Option<String>,
        #[serde(default)]
        in_place: bool,
    },
    GetRunStatus {
        run_id: String,
    },
    GetRunSummary {
        run_id: String,
    },
    ListArtifacts {
        run_id: String,
    },
    ReadArtifact {
        path: String,
    },
    SetWorkspaceRoot {
        path: String,
    },
    SetCopyLocation {
        path: String,
    },
    SortRun {
        run_id: String,
        #[serde(default)]
        in_parent: bool,
        #[serde(default)]
        keep_dir: String,
        #[serde(default)]
        cull_dir: String,
        #[serde(default)]
        review_dir: String,
    },
    IdentityStatus,
    IdentityGate {
        image: String,
    },
    IdentityGateDir {
        dir: String,
    },
    IdentityDedup {
        dir: String,
        #[serde(default = "default_dedup_threshold")]
        threshold: f32,
    },
    RenderEval {
        dir: String,
    },
    CalibrateThreshold,
    AnchorMontage {
        image: String,
    },
    ReviewInit {
        dir: String,
        #[serde(default = "default_review_shards")]
        shards: usize,
        #[serde(default)]
        gate_manifest: Option<String>,
        #[serde(default)]
        clusters: Option<String>,
    },
    ReviewMontage {
        session: String,
        #[serde(default)]
        shard: Option<usize>,
        #[serde(default)]
        page: usize,
        #[serde(default)]
        face_crop: bool,
        #[serde(default)]
        filters: Vec<String>,
    },
    ReviewExport {
        session: String,
        out: String,
        #[serde(default = "default_review_repeats")]
        repeats: usize,
        name: String,
        #[serde(default)]
        allow_partial: bool,
    },
    ReviewClaim {
        session: String,
        #[serde(default)]
        shard: Option<usize>,
        #[serde(default)]
        actor: String,
        #[serde(default)]
        steal: bool,
    },
    ReviewDecide {
        session: String,
        id: String,
        decision: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        actor: String,
    },
    ReviewStatus {
        session: String,
    },
    ListLanes,
    SetLane {
        lane_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        folder: Option<String>,
        #[serde(default)]
        recursive: Option<bool>,
        #[serde(default)]
        steal: bool,
        #[serde(default)]
        feature_keys: Option<Vec<String>>,
    },
    ScanLane {
        lane_id: String,
        #[serde(default)]
        steal: bool,
    },
    ScanAllLanes {
        #[serde(default)]
        steal: bool,
    },
    ClaimLane {
        lane_id: String,
        #[serde(default)]
        actor: String,
        #[serde(default)]
        steal: bool,
    },
    ReleaseLane {
        lane_id: String,
        #[serde(default)]
        actor: String,
        #[serde(default)]
        steal: bool,
    },
    LaneStatus {
        #[serde(default)]
        lane_id: Option<String>,
    },
    StartLaneBatch {
        lane_id: String,
        #[serde(default)]
        project_name: String,
        #[serde(default)]
        feature_keys: Vec<String>,
        #[serde(default)]
        in_place: bool,
        #[serde(default)]
        steal: bool,
    },
    StartAllLaneBatches {
        #[serde(default)]
        project_name: String,
        #[serde(default)]
        feature_keys: Vec<String>,
        #[serde(default = "default_lane_batch_concurrency")]
        concurrency_limit: usize,
        #[serde(default)]
        in_place: bool,
        #[serde(default)]
        steal: bool,
    },

    // ---- media metadata (WP-042; backend-executable against the media DB) ----
    MediaMetaGet {
        path: String,
    },
    MediaMetaSet {
        path: String,
        #[serde(default)]
        notes: Option<String>,
        #[serde(default)]
        tags: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    MediaMetaList {
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    /// List stable color-label IDs plus operator-visible names and backend hex.
    MediaLabelsList,
    /// Legacy alias for updating one existing stable label definition without
    /// changing asset assignments.
    MediaLabelConfigure {
        id: String,
        name: String,
        hex: String,
    },
    MediaLabelCreate {
        name: String,
        hex: String,
        /// When present, catalog creation + file assignment is atomic.
        #[serde(default)]
        path: Option<String>,
    },
    MediaLabelUpdate {
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        hex: Option<String>,
    },
    MediaLabelDelete {
        id: String,
        #[serde(default)]
        confirmed: bool,
    },
    MediaLabelAssign {
        path: String,
        /// Stable ID or current name. Omitted only for `clear`.
        #[serde(default)]
        id: Option<String>,
        /// add | remove | clear
        action: String,
    },
    MediaFavAdd {
        path: String,
    },
    MediaFavRemove {
        path: String,
    },
    MediaFavList,
    /// Sweep the thumbnail disk cache (age + size caps; WP-043).
    ThumbsGc {
        #[serde(default)]
        cap_mb: Option<u64>,
    },
    /// Build/refresh the CLIP embedding index for a folder (WP-047).
    MediaIndexBuild {
        dir: String,
        #[serde(default)]
        recursive: bool,
    },
    /// Headless semantic search over a folder's cached embeddings (WP-047).
    MediaSemanticSearch {
        query: String,
        dir: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    // ---- ui-intent (persisted to intents/; applied by a live GUI) ----
    SetProject {
        project_name: String,
    },
    SetWorktree {
        worktree_path: String,
    },
    SelectTab {
        tab: String,
    }, // vocab: "project"|"quality_iq"|"identity"|"duplicates"|"run_debug"|"manual"|"media"|"lanes"|"options" ("compare" alias)
    SetFeatures {
        feature_keys: Vec<String>,
    },
    SetInPlace {
        in_place: bool,
    },
    ImportPaths {
        project_name: String,
        paths: Vec<String>,
        #[serde(default)]
        in_place: bool,
    },
    StartRunUi, // request the live GUI to press "Run selected features"
    /// Capture the exact live egui framebuffer without activating or focusing
    /// the native window. Embedded video is composited from LibVLC's decoded
    /// frame at the diagnosed native-surface bounds.
    UiSnapshot {
        #[serde(default)]
        output: Option<String>,
    },
    // media browser intents (WP-042): drive the front surface from files.
    MediaSetFolder {
        path: String,
    },
    MediaSearch {
        query: String,
        #[serde(default)]
        mode: Option<String>, // name|fuzzy|tags|notes|semantic (default name)
    },
    MediaSelect {
        paths: Vec<String>,
    },
    MediaOpenSelected,
    /// Manage the document-style Media viewport tabs through the live GUI.
    /// `list` returns structured state; `select`/`close` address a stable tab
    /// ID; `open` creates and selects a tab for `path` (or an empty tab when
    /// omitted). The shared MediaDb remains workspace-scoped.
    MediaTabs {
        action: String,
        #[serde(default)]
        tab_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    /// Drive the couch-distance folder navigator through the same state
    /// transitions used by keyboard and controller input (WP-051).
    MediaFolderNavigate {
        action: String,
    },
    /// Receipt-backed transport/track control for the selected embedded video
    /// (WP-052). Numeric actions use `value` in milliseconds, percent, or ID.
    MediaVideoControl {
        action: String,
        #[serde(default)]
        value: Option<i64>,
        /// Optional frame-capture output. Relative paths resolve from the
        /// configured workspace root in the live GUI.
        #[serde(default)]
        output: Option<String>,
    },
    /// Live-GUI equivalent of catalog CRUD and per-file assignment. The GUI
    /// applies this against its already-open MediaDb handle, avoiding the embedded store's
    /// cross-process exclusive lock.
    MediaLabelMutation {
        /// create | update | delete | add | remove | clear
        action: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        hex: Option<String>,
        #[serde(default)]
        confirmed: bool,
    },
}

impl CommandKind {
    /// Stable snake_case discriminator string (matches the serialized "kind").
    pub fn id_str(&self) -> &'static str {
        match self {
            CommandKind::ListFeatures => "list_features",
            CommandKind::ListModels => "list_models",
            CommandKind::ListWorktrees => "list_worktrees",
            CommandKind::GetState => "get_state",
            CommandKind::StartRun { .. } => "start_run",
            CommandKind::GetRunStatus { .. } => "get_run_status",
            CommandKind::GetRunSummary { .. } => "get_run_summary",
            CommandKind::ListArtifacts { .. } => "list_artifacts",
            CommandKind::ReadArtifact { .. } => "read_artifact",
            CommandKind::SetWorkspaceRoot { .. } => "set_workspace_root",
            CommandKind::SetCopyLocation { .. } => "set_copy_location",
            CommandKind::SortRun { .. } => "sort_run",
            CommandKind::IdentityStatus => "identity_status",
            CommandKind::IdentityGate { .. } => "identity_gate",
            CommandKind::IdentityGateDir { .. } => "identity_gate_dir",
            CommandKind::IdentityDedup { .. } => "identity_dedup",
            CommandKind::RenderEval { .. } => "render_eval",
            CommandKind::CalibrateThreshold => "calibrate_threshold",
            CommandKind::AnchorMontage { .. } => "anchor_montage",
            CommandKind::ReviewInit { .. } => "review_init",
            CommandKind::ReviewClaim { .. } => "review_claim",
            CommandKind::ReviewDecide { .. } => "review_decide",
            CommandKind::ReviewStatus { .. } => "review_status",
            CommandKind::ReviewMontage { .. } => "review_montage",
            CommandKind::ReviewExport { .. } => "review_export",
            CommandKind::ListLanes => "list_lanes",
            CommandKind::SetLane { .. } => "set_lane",
            CommandKind::ScanLane { .. } => "scan_lane",
            CommandKind::ScanAllLanes { .. } => "scan_all_lanes",
            CommandKind::ClaimLane { .. } => "claim_lane",
            CommandKind::ReleaseLane { .. } => "release_lane",
            CommandKind::LaneStatus { .. } => "lane_status",
            CommandKind::StartLaneBatch { .. } => "start_lane_batch",
            CommandKind::StartAllLaneBatches { .. } => "start_all_lane_batches",
            CommandKind::MediaMetaGet { .. } => "media_meta_get",
            CommandKind::MediaMetaSet { .. } => "media_meta_set",
            CommandKind::MediaMetaList { .. } => "media_meta_list",
            CommandKind::MediaLabelsList => "media_labels_list",
            CommandKind::MediaLabelConfigure { .. } => "media_label_configure",
            CommandKind::MediaLabelCreate { .. } => "media_label_create",
            CommandKind::MediaLabelUpdate { .. } => "media_label_update",
            CommandKind::MediaLabelDelete { .. } => "media_label_delete",
            CommandKind::MediaLabelAssign { .. } => "media_label_assign",
            CommandKind::MediaFavAdd { .. } => "media_fav_add",
            CommandKind::MediaFavRemove { .. } => "media_fav_remove",
            CommandKind::MediaFavList => "media_fav_list",
            CommandKind::ThumbsGc { .. } => "thumbs_gc",
            CommandKind::MediaIndexBuild { .. } => "media_index_build",
            CommandKind::MediaSemanticSearch { .. } => "media_semantic_search",
            CommandKind::SetProject { .. } => "set_project",
            CommandKind::SetWorktree { .. } => "set_worktree",
            CommandKind::SelectTab { .. } => "select_tab",
            CommandKind::SetFeatures { .. } => "set_features",
            CommandKind::SetInPlace { .. } => "set_in_place",
            CommandKind::ImportPaths { .. } => "import_paths",
            CommandKind::StartRunUi => "start_run_ui",
            CommandKind::UiSnapshot { .. } => "ui_snapshot",
            CommandKind::MediaSetFolder { .. } => "media_set_folder",
            CommandKind::MediaSearch { .. } => "media_search",
            CommandKind::MediaSelect { .. } => "media_select",
            CommandKind::MediaOpenSelected => "media_open_selected",
            CommandKind::MediaTabs { .. } => "media_tabs",
            CommandKind::MediaFolderNavigate { .. } => "media_folder_navigate",
            CommandKind::MediaVideoControl { .. } => "media_video_control",
            CommandKind::MediaLabelMutation { .. } => "media_label_mutation",
        }
    }

    /// True for the ui-intent variants (everything that needs a live GUI frame).
    pub fn is_ui_intent(&self) -> bool {
        matches!(
            self,
            CommandKind::SetProject { .. }
                | CommandKind::SetWorktree { .. }
                | CommandKind::SelectTab { .. }
                | CommandKind::SetFeatures { .. }
                | CommandKind::SetInPlace { .. }
                | CommandKind::ImportPaths { .. }
                | CommandKind::StartRunUi
                | CommandKind::UiSnapshot { .. }
                | CommandKind::MediaSetFolder { .. }
                | CommandKind::MediaSearch { .. }
                | CommandKind::MediaSelect { .. }
                | CommandKind::MediaOpenSelected
                | CommandKind::MediaTabs { .. }
                | CommandKind::MediaFolderNavigate { .. }
                | CommandKind::MediaVideoControl { .. }
                | CommandKind::MediaLabelMutation { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    pub action_id: String, // join key across command/receipt/intent/events; uuid recommended
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    #[serde(default)]
    pub actor: Option<String>, // swarm model id, for attribution
    #[serde(default)]
    pub issued_at: Option<String>,
    #[serde(flatten)]
    pub command: CommandKind,
}

fn default_protocol_version() -> u32 {
    API_PROTOCOL_VERSION
}

fn default_review_shards() -> usize {
    1
}

fn default_review_repeats() -> usize {
    10
}

fn default_dedup_threshold() -> f32 {
    0.9
}

fn default_lane_batch_concurrency() -> usize {
    2
}

fn effective_lane_actor<'a>(cmd: &'a Command, variant_actor: &'a str) -> &'a str {
    if variant_actor.trim().is_empty() {
        cmd.actor.as_deref().unwrap_or("")
    } else {
        variant_actor
    }
}

// ---------- receipt ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub action_id: String,
    pub kind: String, // CommandKind::id_str()
    pub status: ActionStatus,
    #[serde(default)]
    pub actor: Option<String>,
    pub protocol_version: u32,
    pub started_at: String,  // rfc3339
    pub finished_at: String, // rfc3339
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value, // ListFeatures->plugins, StartRun->RunSummary, GetState->AppStateSnapshot, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>, // human hint (e.g. "tab not yet active", dropped feature keys)
}

// ---------- state snapshot (single canonical definition) ----------
//
// NOTE: the contract specifies `#[derive(Clone, Debug, Serialize, Deserialize)]`
// here, but `ModelRecord` (models.rs) and `PluginManifest` (plugin_host.rs) do
// NOT derive `Debug`, and this task is limited to api.rs. Deriving `Debug` would
// therefore not compile. `Debug` is intentionally dropped from this struct to
// keep the single-file build green; flagged for the repair phase.
#[derive(Clone, Serialize, Deserialize)]
pub struct AppStateSnapshot {
    pub protocol_version: u32,
    pub captured_at: String, // rfc3339
    pub repo_root: String,
    pub workspace_root: String,
    pub worktrees_root: String,
    pub api_root: String,
    pub ingest_in_place_default: bool,
    pub models: Vec<ModelRecord>,
    pub plugins: Vec<PluginManifest>, // features nested inside manifests
    pub worktrees: BTreeMap<String, Vec<String>>, // project -> run dir paths
    #[serde(default)]
    pub lanes: Vec<LaneRecord>,
    // live-GUI fields (populated by ui.rs current_state_snapshot; defaulted in headless capture_state)
    #[serde(default)]
    pub active_tab: String,
    #[serde(default)]
    pub project_name: String,
    #[serde(default)]
    pub worktree_path: String,
    #[serde(default)]
    pub in_place: bool,
    #[serde(default)]
    pub selected_features: Vec<String>,
    #[serde(default)]
    pub running_pipeline: bool,
    #[serde(default)]
    pub run_output: String,
    /// Structured GUI-operability surfaces for no-context models.
    #[serde(default)]
    pub media_tabs: Value,
    #[serde(default)]
    pub media_folder_navigation: Value,
    #[serde(default)]
    pub media_controller: Value,
    #[serde(default)]
    pub media_video: Value,
}

// ---------- on-disk path layout ----------

pub struct ApiPaths {
    pub root: PathBuf,               // <data>/api
    pub commands: PathBuf,           // <data>/api/commands
    pub processing: PathBuf,         // <data>/api/processing
    pub receipts: PathBuf,           // <data>/api/receipts
    pub intents: PathBuf,            // <data>/api/intents
    pub intents_processing: PathBuf, // <data>/api/intents/processing
    pub intents_applied: PathBuf,    // <data>/api/intents/applied
    pub dead: PathBuf,               // <data>/api/dead
    pub state_file: PathBuf,         // <data>/api/state/state.json
}

impl ApiPaths {
    pub fn from_config(cfg: &AppConfig) -> Self {
        let root = cfg.api_root.clone();
        Self {
            commands: root.join("commands"),
            processing: root.join("processing"),
            receipts: root.join("receipts"),
            intents: root.join("intents"),
            intents_processing: root.join("intents").join("processing"),
            intents_applied: root.join("intents").join("applied"),
            dead: root.join("dead"),
            state_file: root.join("state").join("state.json"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.commands)?;
        fs::create_dir_all(&self.processing)?;
        fs::create_dir_all(&self.receipts)?;
        fs::create_dir_all(&self.intents)?;
        fs::create_dir_all(&self.intents_processing)?;
        fs::create_dir_all(&self.intents_applied)?;
        fs::create_dir_all(&self.dead)?;
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    pub fn receipt_path(&self, action_id: &str) -> PathBuf {
        self.receipts.join(format!("{action_id}.json"))
    }

    pub fn intent_path(&self, action_id: &str) -> PathBuf {
        self.intents.join(format!("{action_id}.json"))
    }

    /// Path to the sentinel that stops `watch_queue` when present.
    fn stop_file(&self) -> PathBuf {
        self.root.join("stop")
    }
}

// ---------- helpers ----------

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// Build a fresh action id when a producer omitted one.
fn new_action_id() -> String {
    Uuid::new_v4().to_string()
}

/// Atomically write `contents` to `target` (tmp -> rename, replacing any
/// existing target). Every writer gets a unique temporary path: accepted and
/// terminal UI receipts can otherwise collide on `<action>.json.tmp` when the
/// live GUI claims an intent immediately after the CLI publishes it. On
/// Windows rename-over-existing fails, so the existing target is removed first.
fn atomic_write(target: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let target_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "artifact".into());
    let tmp = target.with_file_name(format!(
        ".{target_name}.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::write(&tmp, contents.as_bytes())?;

    // A Windows receipt poller may have the previous JSON open for the few
    // microseconds in which a terminal receipt replaces Accepted. Retry that
    // sharing violation for a bounded interval; all other errors remain hard.
    const REPLACE_ATTEMPTS: usize = 21;
    let retryable = |error: &std::io::Error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::AlreadyExists
        ) || matches!(error.raw_os_error(), Some(5 | 32 | 33))
    };
    let mut last_error = None;
    for attempt in 0..REPLACE_ATTEMPTS {
        match fs::remove_file(target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if retryable(&error) && attempt + 1 < REPLACE_ATTEMPTS => {
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                return Err(error);
            }
        }
        match fs::rename(&tmp, target) {
            Ok(()) => return Ok(()),
            Err(error) if retryable(&error) && attempt + 1 < REPLACE_ATTEMPTS => {
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                return Err(error);
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "atomic replace retries exhausted",
        )
    }))
}

/// Construct a terminal/accepted/rejected receipt for `cmd`.
/// Errored receipt for media commands when the media DB cannot be opened at
/// all (typically: the configured embedded store is unavailable).
fn media_db_unavailable_receipt(cmd: &Command, started_at: String, db: &MediaDb) -> Receipt {
    make_receipt(
        cmd,
        ActionStatus::Error,
        started_at,
        Value::Null,
        Some(db.status().unwrap_or("media db unavailable").to_string()),
        Some(
            "media db is locked or unavailable — if the GUI is running, close it or drive the \
             media surface through ui-intents (media_set_folder/media_search/media_select)"
                .to_string(),
        ),
    )
}

fn make_receipt(
    cmd: &Command,
    status: ActionStatus,
    started_at: String,
    result: Value,
    error: Option<String>,
    note: Option<String>,
) -> Receipt {
    Receipt {
        action_id: cmd.action_id.clone(),
        kind: cmd.command.id_str().to_string(),
        status,
        actor: cmd.actor.clone(),
        protocol_version: cmd.protocol_version,
        started_at,
        finished_at: now_rfc3339(),
        result,
        error,
        note,
    }
}

fn write_lane_batch_child_receipts(
    service: &mut FacialService,
    paths: &ApiPaths,
    cmd: &Command,
    aggregate: &LaneBatchAggregate,
    started_at: &str,
) -> Result<(), String> {
    for result in &aggregate.results {
        let status = if result.error.is_some() {
            ActionStatus::Error
        } else {
            ActionStatus::Ok
        };
        let receipt = Receipt {
            action_id: result.action_id.clone(),
            kind: "start_lane_batch".to_string(),
            status,
            actor: cmd.actor.clone(),
            protocol_version: cmd.protocol_version,
            started_at: started_at.to_string(),
            finished_at: now_rfc3339(),
            result: serde_json::to_value(result).unwrap_or(Value::Null),
            error: result.error.clone(),
            note: Some(format!(
                "child receipt for start_all_lane_batches {}",
                cmd.action_id
            )),
        };
        write_receipt(service, paths, &receipt)
            .map_err(|err| format!("write child lane receipt {}: {err}", result.action_id))?;
    }
    Ok(())
}

/// Canonicalize `candidate` and assert it lives under one of `roots`. Returns
/// the canonical path on success, or an error string suitable for a rejection
/// note. If the path does not exist yet, its closest existing ancestor is
/// canonicalized so the containment guard still applies.
fn guard_path_under(candidate: &Path, roots: &[&Path]) -> Result<PathBuf, String> {
    let canonical = canonicalize_best_effort(candidate);
    let canon_roots: Vec<PathBuf> = roots.iter().map(|r| canonicalize_best_effort(r)).collect();
    if canon_roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        Err(format!(
            "path escapes allowed roots: {}",
            canonical.to_string_lossy()
        ))
    }
}

/// Canonicalize a path; if it does not exist, canonicalize the nearest existing
/// ancestor and re-append the remaining components so symlink/`..` resolution
/// still happens for the part of the path that exists.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(path) {
        return c;
    }
    let mut current = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !current.as_os_str().is_empty() {
        if let Ok(c) = fs::canonicalize(&current) {
            let mut resolved = c;
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        if let Some(name) = current.file_name() {
            tail.push(name.to_os_string());
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    path.to_path_buf()
}

/// Convert the service's `list_worktrees` shape (`BTreeMap<String, Vec<PathBuf>>`)
/// into the protocol shape (`BTreeMap<String, Vec<String>>`).
fn worktrees_as_strings(service: &mut FacialService) -> BTreeMap<String, Vec<String>> {
    service
        .list_worktrees()
        .into_iter()
        .map(|(project, runs)| {
            (
                project,
                runs.into_iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect(),
            )
        })
        .collect()
}

/// Re-hydrate `PluginManifest` values from the service's JSON-valued plugin
/// listing. Malformed entries are silently skipped.
fn plugins_as_manifests(values: Vec<Value>) -> Vec<PluginManifest> {
    values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<PluginManifest>(value).ok())
        .collect()
}

// ---------- dispatch / queue / parsing (all owned here) ----------

/// Execute ONE backend command synchronously against the live service, OR
/// validate+persist a ui-intent to intents/ (returns ActionStatus::Accepted).
/// Always returns a terminal-or-accepted Receipt; never panics.
pub fn dispatch(service: &mut FacialService, paths: &ApiPaths, cmd: &Command) -> Receipt {
    let started_at = now_rfc3339();

    // ui-intents are validated lightly then persisted for the live GUI to apply.
    if cmd.command.is_ui_intent() {
        return dispatch_ui_intent_started(paths, cmd, started_at);
    }

    match &cmd.command {
        CommandKind::ListFeatures => {
            let plugins = service.list_plugins();
            make_receipt(
                cmd,
                ActionStatus::Ok,
                started_at,
                Value::Array(plugins),
                None,
                None,
            )
        }
        CommandKind::ListModels => {
            let models = service.list_models();
            let result = serde_json::to_value(models).unwrap_or(Value::Null);
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::ListWorktrees => {
            let worktrees = worktrees_as_strings(service);
            let result = serde_json::to_value(worktrees).unwrap_or(Value::Null);
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::GetState => {
            let snapshot = capture_state(service, paths);
            let result = serde_json::to_value(snapshot).unwrap_or(Value::Null);
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::StartRun {
            project_name,
            image_paths,
            feature_keys,
            worktree_path,
            in_place,
        } => match service.run_pipeline(
            project_name,
            image_paths,
            feature_keys,
            worktree_path.clone(),
            *in_place,
        ) {
            Ok(summary) => {
                let result = serde_json::to_value(summary).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::SetWorkspaceRoot { path } => match service.set_workspace_root(path) {
            Ok(resolved) => make_receipt(
                cmd,
                ActionStatus::Ok,
                started_at,
                serde_json::json!({
                    "workspace_root": resolved,
                    "worktrees_root": service.config().worktrees_root.to_string_lossy(),
                    "api_root": service.config().api_root.to_string_lossy(),
                }),
                None,
                None,
            ),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::SetCopyLocation { path } => match service.set_copy_location(path) {
            Ok(resolved) => make_receipt(
                cmd,
                ActionStatus::Ok,
                started_at,
                serde_json::json!({ "copy_location": resolved }),
                None,
                None,
            ),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::SortRun {
            run_id,
            in_parent,
            keep_dir,
            cull_dir,
            review_dir,
        } => match service.sort_run(run_id, *in_parent, keep_dir, cull_dir, review_dir) {
            Ok(summary) => make_receipt(cmd, ActionStatus::Ok, started_at, summary, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::IdentityStatus => make_receipt(
            cmd,
            ActionStatus::Ok,
            started_at,
            service.identity_status(),
            None,
            None,
        ),
        CommandKind::IdentityGate { image } => match service.identity_gate(image) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::IdentityGateDir { dir } => match service.identity_gate_dir(dir) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::IdentityDedup { dir, threshold } => {
            match service.identity_dedup(dir, *threshold) {
                Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::RenderEval { dir } => match service.render_eval(dir) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::CalibrateThreshold => match service.calibrate_threshold() {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::AnchorMontage { image } => match service.anchor_montage(image) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewInit {
            dir,
            shards,
            gate_manifest,
            clusters,
        } => match service.review_init(dir, *shards, gate_manifest.as_deref(), clusters.as_deref())
        {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewMontage {
            session,
            shard,
            page,
            face_crop,
            filters,
        } => match service.review_montage(session, *shard, *page, *face_crop, filters) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewExport {
            session,
            out,
            repeats,
            name,
            allow_partial,
        } => match service.review_export(session, out, *repeats, name, *allow_partial) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewClaim {
            session,
            shard,
            actor,
            steal,
        } => match service.review_claim(session, *shard, actor, *steal) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewDecide {
            session,
            id,
            decision,
            reason,
            actor,
        } => match service.review_decide(session, id, decision, reason, actor) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReviewStatus { session } => match service.review_status(session) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ListLanes => match service.list_lanes() {
            Ok(lanes) => {
                let result = serde_json::to_value(lanes).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::SetLane {
            lane_id,
            name,
            mode,
            folder,
            recursive,
            steal,
            feature_keys,
        } => match service.set_lane_for_actor(
            lane_id,
            name.as_deref(),
            mode.as_deref(),
            folder.as_deref(),
            *recursive,
            feature_keys.as_deref(),
            cmd.actor.as_deref(),
            *steal,
        ) {
            Ok(lane) => {
                let result = serde_json::to_value(lane).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ScanLane { lane_id, steal } => {
            match service.scan_lane_for_actor(lane_id, cmd.actor.as_deref(), *steal) {
                Ok(result) => {
                    let result = serde_json::to_value(result).unwrap_or(Value::Null);
                    make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
                }
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::ScanAllLanes { steal } => {
            match service.scan_all_lanes_for_actor(cmd.actor.as_deref(), *steal) {
                Ok(results) => {
                    let result = serde_json::to_value(results).unwrap_or(Value::Null);
                    make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
                }
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::ClaimLane {
            lane_id,
            actor,
            steal,
        } => match service.claim_lane(lane_id, effective_lane_actor(cmd, actor), *steal) {
            Ok(lane) => {
                let result = serde_json::to_value(lane).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::ReleaseLane {
            lane_id,
            actor,
            steal,
        } => match service.release_lane(lane_id, effective_lane_actor(cmd, actor), *steal) {
            Ok(lane) => {
                let result = serde_json::to_value(lane).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::LaneStatus { lane_id } => match service.lane_status(lane_id.as_deref()) {
            Ok(lanes) => {
                let result = serde_json::to_value(lanes).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::StartLaneBatch {
            lane_id,
            project_name,
            feature_keys,
            in_place,
            steal,
        } => match service.start_lane_batch_with_action_id(
            lane_id,
            project_name,
            feature_keys,
            *in_place,
            cmd.actor.as_deref(),
            *steal,
            &cmd.action_id,
        ) {
            Ok(result) => {
                let result = serde_json::to_value(result).unwrap_or(Value::Null);
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::StartAllLaneBatches {
            project_name,
            feature_keys,
            concurrency_limit,
            in_place,
            steal,
        } => match service.start_all_lane_batches(
            project_name,
            feature_keys,
            *concurrency_limit,
            *in_place,
            cmd.actor.as_deref(),
            *steal,
        ) {
            Ok(result) => {
                match write_lane_batch_child_receipts(service, paths, cmd, &result, &started_at) {
                    Ok(()) => {
                        let result = serde_json::to_value(result).unwrap_or(Value::Null);
                        make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
                    }
                    Err(err) => {
                        let result = serde_json::to_value(result).unwrap_or(Value::Null);
                        make_receipt(
                            cmd,
                            ActionStatus::Error,
                            started_at,
                            result,
                            Some(err),
                            Some(
                                "batch ran but one or more child receipts could not be written"
                                    .to_string(),
                            ),
                        )
                    }
                }
            }
            Err(err) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(err),
                None,
            ),
        },
        CommandKind::GetRunStatus { run_id } => {
            let found = service.find_run_results(run_id).is_some();
            let status = if found { "completed" } else { "unknown" };
            let result = serde_json::json!({ "status": status, "found": found });
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::GetRunSummary { run_id } => match service.find_run_results(run_id) {
            Some(path) => match fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                    Ok(value) => make_receipt(cmd, ActionStatus::Ok, started_at, value, None, None),
                    Err(err) => make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some(format!("results.json parse error: {err}")),
                        None,
                    ),
                },
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(format!("results.json read error: {err}")),
                    None,
                ),
            },
            None => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(format!("run not found: {run_id}")),
                None,
            ),
        },
        CommandKind::ListArtifacts { run_id } => match service.find_run_results(run_id) {
            Some(results_path) => {
                let run_dir = results_path.parent().map(|p| p.to_path_buf());
                match run_dir {
                    Some(dir) => {
                        // Guard: the run dir must live under a known artifact root.
                        let roots = service.artifact_roots();
                        let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                        match guard_path_under(&dir, &root_refs) {
                            Ok(_) => {
                                let artifacts = list_files_recursive(&dir);
                                let result = serde_json::to_value(artifacts).unwrap_or(Value::Null);
                                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
                            }
                            Err(note) => make_receipt(
                                cmd,
                                ActionStatus::Rejected,
                                started_at,
                                Value::Null,
                                None,
                                Some(note),
                            ),
                        }
                    }
                    None => make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some("run dir has no parent".to_string()),
                        None,
                    ),
                }
            }
            None => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(format!("run not found: {run_id}")),
                None,
            ),
        },
        CommandKind::ReadArtifact { path } => {
            let roots = service.artifact_roots();
            let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
            match guard_path_under(Path::new(path), &root_refs) {
                Ok(canonical) => match fs::read_to_string(&canonical) {
                    Ok(raw) => {
                        // Parse JSON when possible; otherwise return as a string value.
                        let value = serde_json::from_str::<Value>(&raw)
                            .unwrap_or_else(|_| Value::String(raw));
                        make_receipt(cmd, ActionStatus::Ok, started_at, value, None, None)
                    }
                    Err(err) => make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some(format!("artifact read error: {err}")),
                        None,
                    ),
                },
                Err(note) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    Value::Null,
                    None,
                    Some(note),
                ),
            }
        }
        // media metadata (WP-042): backend commands against the workspace
        // media DB. The live GUI and command path share one application store: while a live
        // GUI runs, a CLI process can neither write nor read — those receipts
        // must be errors (never ok-with-empty, which models would misread as
        // "no metadata"). Headless operation (no GUI running) has full access.
        CommandKind::MediaMetaGet { path } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let meta = db.meta(path);
            let result = serde_json::json!({
                "path": path,
                "key": db.key_for(path),
                "notes": meta.notes,
                "tags": meta.tags,
                "label": meta.label,
                "labels": meta.labels,
                "favorite": meta.favorite,
                "db_status": db.status(),
            });
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::MediaMetaSet {
            path,
            notes,
            tags,
            label,
        } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let mut errors: Vec<String> = Vec::new();
            if let Some(notes) = notes {
                if let Err(err) = db.set_notes(path, notes) {
                    errors.push(format!("notes: {err}"));
                }
            }
            if let Some(tags) = tags {
                if let Err(err) = db.set_tags(path, tags) {
                    errors.push(format!("tags: {err}"));
                }
            }
            if let Some(label) = label {
                if let Err(err) = db.set_label(path, label) {
                    errors.push(format!("label: {err}"));
                }
            }
            let meta = db.meta(path);
            let result = serde_json::json!({
                "path": path,
                "key": db.key_for(path),
                "notes": meta.notes,
                "tags": meta.tags,
                "label": meta.label,
                "labels": meta.labels,
                "favorite": meta.favorite,
            });
            if errors.is_empty() {
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            } else {
                make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    result,
                    Some(errors.join("; ")),
                    None,
                )
            }
        }
        CommandKind::MediaMetaList { tag, label } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let rows: Vec<Value> = db
                .list_meta(tag.as_deref(), label.as_deref())
                .into_iter()
                .map(|(path, meta)| {
                    serde_json::json!({
                        "path": path,
                        "notes": meta.notes,
                        "tags": meta.tags,
                        "label": meta.label,
                        "labels": meta.labels,
                        "favorite": meta.favorite,
                    })
                })
                .collect();
            let result = serde_json::json!({
                "count": rows.len(),
                "rows": rows,
                "tag_vocab": db.tag_vocab(),
                "db_status": db.status(),
            });
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::MediaLabelsList => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            make_receipt(
                cmd,
                ActionStatus::Ok,
                started_at,
                serde_json::json!({
                    "labels": db.color_label_definitions(),
                    "usage": db.color_label_usage_counts(),
                    "db_status": db.status(),
                }),
                None,
                None,
            )
        }
        CommandKind::MediaLabelConfigure { id, name, hex } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let mut definitions = db.color_label_definitions();
            let Some(definition) = definitions.iter_mut().find(|item| item.id == *id) else {
                return make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({ "labels": definitions }),
                    Some(format!("unknown stable color-label id: {id}")),
                    None,
                );
            };
            definition.name = name.clone();
            definition.hex = hex.clone();
            match db.set_color_label_definitions(&definitions) {
                Ok(labels) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({ "labels": labels }),
                    None,
                    None,
                ),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({ "labels": db.color_label_definitions() }),
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MediaLabelCreate { name, hex, path } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let result = match path {
                Some(path) => db.create_color_label_and_assign(path, name, hex),
                None => db.create_color_label(name, hex),
            };
            match result {
                Ok(label) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({
                        "label": label,
                        "path": path,
                        "assigned_labels": path.as_deref().map(|path| db.labels(path)),
                        "labels": db.color_label_definitions(),
                    }),
                    None,
                    None,
                ),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({ "labels": db.color_label_definitions() }),
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MediaLabelUpdate { id, name, hex } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            match db.update_color_label(id, name.as_deref(), hex.as_deref()) {
                Ok(label) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({
                        "label": label,
                        "labels": db.color_label_definitions(),
                    }),
                    None,
                    None,
                ),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({ "labels": db.color_label_definitions() }),
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MediaLabelDelete { id, confirmed } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let usage_count = db.color_label_usage_counts().get(id).copied().unwrap_or(0);
            match db.delete_color_label(id, *confirmed) {
                Ok(deleted) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({
                        "deleted": deleted,
                        "labels": db.color_label_definitions(),
                    }),
                    None,
                    None,
                ),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({
                        "id": id,
                        "usage_count": usage_count,
                        "confirmation_required": usage_count > 0 && !confirmed,
                        "labels": db.color_label_definitions(),
                    }),
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MediaLabelAssign { path, id, action } => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let result = match action.as_str() {
                "add" => id
                    .as_deref()
                    .ok_or_else(|| "media_label_assign add requires --label".to_string())
                    .and_then(|id| db.add_label(path, id)),
                "remove" => id
                    .as_deref()
                    .ok_or_else(|| "media_label_assign remove requires --label".to_string())
                    .and_then(|id| db.remove_label(path, id)),
                "clear" => db.clear_labels(path).map(|()| Vec::new()),
                _ => Err(format!(
                    "unknown media label assignment action: {action} (expected add|remove|clear)"
                )),
            };
            match result {
                Ok(labels) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({ "path": path, "labels": labels }),
                    None,
                    None,
                ),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Rejected,
                    started_at,
                    serde_json::json!({ "path": path, "labels": db.labels(path) }),
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MediaFavAdd { path } => {
            let db = MediaDb::open(&service.config().workspace_root);
            match db.add_favorite(path) {
                Ok(()) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({ "path": path, "favorite": true }),
                    None,
                    None,
                ),
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::MediaFavRemove { path } => {
            let db = MediaDb::open(&service.config().workspace_root);
            match db.remove_favorite(path) {
                Ok(()) => make_receipt(
                    cmd,
                    ActionStatus::Ok,
                    started_at,
                    serde_json::json!({ "path": path, "favorite": false }),
                    None,
                    None,
                ),
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::MediaIndexBuild { dir, recursive } => {
            let config = service.config();
            let status = crate::media_clip::resolve(config);
            if !status.ready() {
                return make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(status.detail),
                    None,
                );
            }
            let engine = match crate::media_clip::ClipEngine::load(&status) {
                Ok(engine) => engine,
                Err(err) => {
                    return make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some(err),
                        None,
                    )
                }
            };
            let index = match crate::media_clip::ClipIndex::open(&config.workspace_root) {
                Ok(index) => index,
                Err(err) => {
                    return make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some(err),
                        None,
                    )
                }
            };
            let files = collect_image_files(Path::new(dir), *recursive);
            let mut indexed = 0usize;
            let mut cached = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for path in &files {
                let key = crate::media_db::canonical_key(&config.workspace_root, path);
                let (mtime, size) = stat_pair(path);
                if index.get(&key, mtime, size).is_some() {
                    cached += 1;
                    continue;
                }
                match engine.embed_image_path(path) {
                    Ok(embedding) => match index.put(&key, mtime, size, &embedding) {
                        Ok(()) => indexed += 1,
                        Err(err) => failed.push(format!("{path}: {err}")),
                    },
                    Err(err) => failed.push(format!("{path}: {err}")),
                }
            }
            let result = serde_json::json!({
                "dir": dir,
                "recursive": recursive,
                "files": files.len(),
                "indexed": indexed,
                "already_cached": cached,
                "failed": failed.len(),
                "failures": failed.iter().take(20).collect::<Vec<_>>(),
                "embedding_dim": engine.dim,
                "index_path": crate::media_clip::ClipIndex::index_path(&config.workspace_root).to_string_lossy(),
            });
            let status = if failed.is_empty() {
                ActionStatus::Ok
            } else {
                ActionStatus::Error
            };
            let error =
                (!failed.is_empty()).then(|| format!("{} file(s) failed to embed", failed.len()));
            make_receipt(cmd, status, started_at, result, error, None)
        }
        CommandKind::MediaSemanticSearch { query, dir, limit } => {
            let config = service.config();
            let status = crate::media_clip::resolve(config);
            if !status.ready() {
                return make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(status.detail),
                    None,
                );
            }
            let outcome = (|| -> Result<Value, String> {
                let engine = crate::media_clip::ClipEngine::load(&status)?;
                let index = crate::media_clip::ClipIndex::open(&config.workspace_root)?;
                let query_vec = engine.embed_text(query)?;
                let files = collect_image_files(Path::new(dir), true);
                let mut ranked: Vec<(String, f32)> = Vec::new();
                let mut missing = 0usize;
                for path in &files {
                    let key = crate::media_db::canonical_key(&config.workspace_root, path);
                    let (mtime, size) = stat_pair(path);
                    match index.get(&key, mtime, size) {
                        Some(embedding) => ranked.push((
                            path.clone(),
                            crate::media_clip::cosine(&query_vec, &embedding),
                        )),
                        None => missing += 1,
                    }
                }
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                ranked.truncate(limit.unwrap_or(50));
                Ok(serde_json::json!({
                    "query": query,
                    "dir": dir,
                    "results": ranked
                        .iter()
                        .map(|(path, score)| serde_json::json!({"path": path, "score": score}))
                        .collect::<Vec<_>>(),
                    "unindexed_skipped": missing,
                    "note": (missing > 0).then(|| "run media_index_build to embed the skipped files"),
                }))
            })();
            match outcome {
                Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
                Err(err) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(err),
                    None,
                ),
            }
        }
        CommandKind::ThumbsGc { cap_mb } => {
            let config = service.config();
            let cache_root =
                crate::media_thumbs::ThumbnailEngine::cache_root(&config.workspace_root);
            let cap = cap_mb.unwrap_or(config.media_thumb_cache_mb);
            let (removed, removed_bytes) = crate::media_thumbs::ThumbnailEngine::gc(
                &cache_root,
                cap,
                crate::media_thumbs::CACHE_MAX_AGE_DAYS,
            );
            let result = serde_json::json!({
                "cache_root": cache_root.to_string_lossy(),
                "cap_mb": cap,
                "removed_files": removed,
                "removed_bytes": removed_bytes,
            });
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        CommandKind::MediaFavList => {
            let db = MediaDb::open(&service.config().workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let favorites = db.favorites();
            let result = serde_json::json!({
                "count": favorites.len(),
                "favorites": favorites,
                "db_status": db.status(),
            });
            make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
        }
        // ui-intents are handled above; this arm keeps the match total.
        _ => make_receipt(
            cmd,
            ActionStatus::Rejected,
            started_at,
            Value::Null,
            None,
            Some("unhandled command kind".to_string()),
        ),
    }
}

/// Validate a ui-intent and persist it to intents/ for the live GUI to apply.
/// Returns a Receipt with `ActionStatus::Accepted` (or `Rejected`).
fn dispatch_ui_intent_started(paths: &ApiPaths, cmd: &Command, started_at: String) -> Receipt {
    // Light validation: SelectTab vocab must be one of the known tabs.
    if let CommandKind::SelectTab { tab } = &cmd.command {
        const TAB_VOCAB: [&str; 9] = [
            "project",
            "quality_iq",
            "identity",
            "duplicates",
            "run_debug",
            "manual",
            "media",
            "lanes",
            "options",
        ];
        const TAB_ALIASES: [&str; 1] = ["compare"];
        if !TAB_VOCAB.contains(&tab.as_str()) && !TAB_ALIASES.contains(&tab.as_str()) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                None,
                Some(format!(
                    "unknown tab vocab: {tab} (expected one of {}; compare is accepted as an alias)",
                    TAB_VOCAB.join("|")
                )),
            );
        }
    }

    // Light validation: MediaSearch mode vocabulary (WP-042).
    if let CommandKind::MediaSearch {
        mode: Some(mode), ..
    } = &cmd.command
    {
        const MODE_VOCAB: [&str; 5] = ["name", "fuzzy", "tags", "notes", "semantic"];
        if !MODE_VOCAB.contains(&mode.as_str()) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                None,
                Some(format!(
                    "unknown media search mode: {mode} (expected one of {})",
                    MODE_VOCAB.join("|")
                )),
            );
        }
    }

    // Light validation: couch navigator action vocabulary (WP-051).
    if let CommandKind::MediaFolderNavigate { action } = &cmd.command {
        const ACTION_VOCAB: [&str; 14] = [
            "open",
            "close",
            "toggle",
            "up",
            "down",
            "page_up",
            "page_down",
            "home",
            "end",
            "enter",
            "parent",
            "refresh",
            "commit",
            "open_new_tab",
        ];
        if !ACTION_VOCAB.contains(&action.as_str()) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(format!(
                    "unknown folder navigator action: {action} (expected one of {})",
                    ACTION_VOCAB.join("|")
                )),
                None,
            );
        }
    }

    if let CommandKind::MediaTabs {
        action,
        tab_id,
        path,
    } = &cmd.command
    {
        // WP-067 adds `open_collection`, which reuses `path` to carry the
        // sub-view vocabulary (fav_videos | fav_images | labels).
        const ACTION_VOCAB: [&str; 23] = [
            "list",
            "select",
            "open",
            "close",
            "open_collection",
            // WP-066/WP-068 per-tab controls, both carrying their value in `path`.
            "set_scope",
            "set_sort",
            "navigate_grid",
            "set_chrome",
            "set_split",
            // Read-only label catalog, reachable while the GUI holds the
            // exclusive media-database lock.
            "labels",
            // WP-067: drop the selected rows' membership in the open collection.
            // The GUI has this as "Remove from view"; without a matching intent
            // a model could see an orphaned favourite but never clear it, since
            // the backend media_fav_remove is blocked by the GUI's exclusive
            // database lock.
            "remove_from_view",
            // WP-070: filename captions and thumbnail size were GUI-only, so a
            // model could not reproduce caption behaviour on a live app at all.
            "set_names",
            "set_tile_size",
            // WP-073: confirmed delete of the current selection. The explicit
            // path token (recycle|permanent) is the confirmation a model
            // cannot click; without it the intent rejects and deletes nothing.
            "delete_selected",
            // WP-072: Viewer metadata band height in points, per tab.
            "set_meta_height",
            // WP-074: batch selection, the selection sheet, and
            // destination-directed filing of the current selection.
            "set_select_mode",
            "set_sheet",
            "export_sheet",
            "move_to",
            "copy_to",
            // WP-075: right-panel receiving folder for drag-and-drop filing.
            "open_receiving_pane",
            "close_receiving_pane",
        ];
        const COLLECTION_VIEWS: [&str; 3] = ["fav_videos", "fav_images", "labels"];
        const PATH_ACTIONS: [&str; 16] = [
            "open",
            "open_collection",
            "set_scope",
            "set_sort",
            "navigate_grid",
            "set_chrome",
            "set_split",
            "set_names",
            "set_tile_size",
            "delete_selected",
            "set_meta_height",
            "set_select_mode",
            "set_sheet",
            "move_to",
            "copy_to",
            "open_receiving_pane",
        ];
        let invalid = !ACTION_VOCAB.contains(&action.as_str())
            || (matches!(action.as_str(), "select" | "close") && tab_id.is_none())
            || (!PATH_ACTIONS.contains(&action.as_str()) && path.is_some())
            || (matches!(
                action.as_str(),
                "set_scope"
                    | "set_sort"
                    | "set_names"
                    | "set_tile_size"
                    | "navigate_grid"
                    | "set_chrome"
                    | "set_split"
                    | "delete_selected"
                    | "set_meta_height"
                    | "set_select_mode"
                    | "set_sheet"
                    | "move_to"
                    | "copy_to"
                    | "open_receiving_pane"
            ) && path.is_none())
            || (PATH_ACTIONS.contains(&action.as_str()) && tab_id.is_some())
            || (matches!(
                action.as_str(),
                "list" | "labels" | "remove_from_view" | "export_sheet" | "close_receiving_pane"
            ) && (tab_id.is_some() || path.is_some()))
            || (action == "open_collection"
                && path.as_deref().is_some_and(|view| {
                    // `labels:<label-id>` selects a label in the same call.
                    let key = view.split_once(':').map_or(view, |(key, _)| key);
                    !COLLECTION_VIEWS.contains(&key)
                }));
        if invalid {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(
                    "invalid media_tabs intent; list takes no fields, select/close require tab_id, open accepts optional path, open_collection accepts path=fav_videos|fav_images|labels or labels:LABEL_ID, labels takes no fields, remove_from_view takes no fields (it acts on the current selection in the open collection tab; select rows first with media_select), set_scope requires path=folder|tab, set_sort requires path=name|modified|size|created[:asc|:desc], navigate_grid requires path=left|right|up|down|page_up|page_down|home|end, set_chrome requires path=hidden|visible, set_split requires path=<ratio>, set_names requires path=on|off, set_tile_size requires path=<points>, delete_selected requires the explicit confirmation path=recycle|permanent and acts on the current selection, set_meta_height requires path=<points>, set_select_mode requires path=on|off, set_sheet requires path=on|off|names_on|names_off, export_sheet takes no fields, move_to and copy_to require path=<destination folder> and act on the current selection, open_receiving_pane requires path=<tab id> of a non-active tab, close_receiving_pane takes no fields"
                        .to_string(),
                ),
                None,
            );
        }
    }

    if let CommandKind::MediaVideoControl {
        action,
        value,
        output,
    } = &cmd.command
    {
        const ACTION_VOCAB: [&str; 12] = [
            "status",
            "play_pause",
            "play",
            "play_library",
            "pause",
            "stop",
            "seek_ms",
            "volume",
            "audio_track",
            "subtitle_track",
            "loop",
            "capture_frame",
        ];
        if !ACTION_VOCAB.contains(&action.as_str()) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(format!(
                    "unknown video action: {action} (expected one of {})",
                    ACTION_VOCAB.join("|")
                )),
                None,
            );
        }
        if matches!(
            action.as_str(),
            "seek_ms" | "volume" | "audio_track" | "subtitle_track" | "loop"
        ) && value.is_none()
        {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(format!("video action {action} requires --value")),
                None,
            );
        }
        if action != "capture_frame" && output.is_some() {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(format!("video action {action} does not accept --out")),
                None,
            );
        }
    }

    if let CommandKind::MediaLabelMutation {
        action,
        path,
        id,
        name,
        hex,
        ..
    } = &cmd.command
    {
        const ACTION_VOCAB: [&str; 6] = ["create", "update", "delete", "add", "remove", "clear"];
        let invalid = !ACTION_VOCAB.contains(&action.as_str())
            || (matches!(action.as_str(), "add" | "remove") && (path.is_none() || id.is_none()))
            || (action == "clear" && path.is_none())
            || (action == "create" && (name.is_none() || hex.is_none()))
            || (matches!(action.as_str(), "update" | "delete") && id.is_none())
            || (action == "update" && name.is_none() && hex.is_none());
        if invalid {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(
                    "invalid media label mutation; create needs name+hex, update/delete need id, add/remove need path+id, clear needs path"
                        .to_string(),
                ),
                None,
            );
        }
    }

    // Publish Accepted before making the intent visible. The GUI may claim and
    // finalize a newly-visible intent in the same scheduler slice; publishing
    // the intent first lets the CLI's Accepted write race the GUI's terminal
    // write, which can either downgrade Applied back to Accepted or make both
    // writers collide on Windows. Callers must not rewrite Accepted receipts.
    let accepted = make_receipt(
        cmd,
        ActionStatus::Accepted,
        started_at.clone(),
        serde_json::to_value(&cmd.command).unwrap_or(Value::Null),
        None,
        Some("ui-intent persisted; awaiting live GUI apply".to_string()),
    );
    if let Err(err) = write_receipt_file(paths, &accepted) {
        return make_receipt(
            cmd,
            ActionStatus::Error,
            started_at,
            Value::Null,
            Some(format!("accepted receipt persist error: {err}")),
            None,
        );
    }

    // Persist the full command to intents/<id>.json (atomic). Only this final
    // rename exposes work to the live GUI, after Accepted is already durable.
    let target = paths.intent_path(&cmd.action_id);
    let serialized = match serde_json::to_string_pretty(cmd) {
        Ok(s) => s,
        Err(err) => {
            return make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(format!("intent serialize error: {err}")),
                None,
            );
        }
    };
    if let Err(err) = atomic_write(&target, &serialized) {
        return make_receipt(
            cmd,
            ActionStatus::Error,
            started_at,
            Value::Null,
            Some(format!("intent persist error: {err}")),
            None,
        );
    }

    // Echo the queued intent payload so callers can verify exactly what was
    // persisted (including select_tab's result["tab"] contract).
    accepted
}

/// Validate and persist a UI intent without constructing the heavyweight
/// backend service. This keeps controller/media navigation commands instant;
/// the live GUI remains the only process that applies them.
pub fn dispatch_ui_intent(paths: &ApiPaths, cmd: &Command) -> Receipt {
    dispatch_ui_intent_started(paths, cmd, now_rfc3339())
}

/// Image files under `dir` (sorted; optionally recursive) for CLIP indexing.
fn collect_image_files(dir: &Path, recursive: bool) -> Vec<String> {
    let mut out = Vec::new();
    let is_image = |path: &Path| {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "gif"
                )
            })
    };
    if recursive {
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            let path = entry.path();
            if path.is_file() && is_image(path) {
                out.push(path.to_string_lossy().to_string());
            }
        }
    } else if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_image(&path) {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

/// (mtime seconds, size bytes); zeros when unreadable.
fn stat_pair(path: &str) -> (u64, u64) {
    fs::metadata(path)
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

/// Recursively list every file under `dir` as path strings (sorted).
fn list_files_recursive(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(list_files_recursive(&path));
            } else if path.is_file() {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

/// Parse a command file (rejects *.tmp at call sites, not here).
pub fn parse_command_file(path: &Path) -> Result<Command, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("cannot read command file {}: {err}", path.display()))?;
    parse_command_str(&raw)
}

/// Parse an inline JSON command string.
pub fn parse_command_str(json: &str) -> Result<Command, String> {
    let mut cmd: Command =
        serde_json::from_str(json).map_err(|err| format!("invalid command json: {err}"))?;
    if cmd.action_id.trim().is_empty() {
        cmd.action_id = new_action_id();
    }
    Ok(cmd)
}

/// Atomically write a receipt (tmp -> rename, replacing any existing target)
/// AND mirror it to events.jsonl via service.config()/DebugBus (source="api").
pub fn write_receipt(
    service: &mut FacialService,
    paths: &ApiPaths,
    receipt: &Receipt,
) -> std::io::Result<()> {
    let target = paths.receipt_path(&receipt.action_id);
    let serialized = serde_json::to_string_pretty(receipt)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    atomic_write(&target, &serialized)?;

    // Mirror to events.jsonl. record_applied_action keys on applied=bool; for
    // backend receipts we map Ok/Accepted/Applied -> applied=true, else false,
    // and pass the receipt metadata as the structured snapshot.
    let status_str = match receipt.status {
        ActionStatus::Ok => "ok",
        ActionStatus::Error => "error",
        ActionStatus::Accepted => "accepted",
        ActionStatus::Applied => "applied",
        ActionStatus::Rejected => "rejected",
    };
    let applied = matches!(
        receipt.status,
        ActionStatus::Ok | ActionStatus::Accepted | ActionStatus::Applied
    );
    let message = format!("command {status_str} {}", receipt.kind);
    let snapshot = serde_json::json!({
        "action_id": receipt.action_id,
        "kind": receipt.kind,
        "status": status_str,
        "actor": receipt.actor,
        "error": receipt.error,
        "note": receipt.note,
    });
    service.record_applied_action(
        &receipt.action_id,
        &receipt.kind,
        applied,
        &message,
        snapshot,
    );
    Ok(())
}

/// Write an accepted/rejected UI-intent receipt without initializing service
/// models merely to emit a debug event. The live GUI records the authoritative
/// applied/rejected event when it consumes the intent.
pub fn write_receipt_file(paths: &ApiPaths, receipt: &Receipt) -> std::io::Result<()> {
    let target = paths.receipt_path(&receipt.action_id);
    let serialized = serde_json::to_string_pretty(receipt)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    atomic_write(&target, &serialized)
}

/// Move a malformed/undispatchable command file into dead/ and write a paired
/// error receipt. Best-effort; the source file is consumed.
fn quarantine_dead(
    service: &mut FacialService,
    paths: &ApiPaths,
    source: &Path,
    action_id: &str,
    error: String,
) -> Receipt {
    let now = now_rfc3339();
    // Move the raw command into dead/ for later inspection.
    let dead_target = paths.dead.join(format!("{action_id}.json"));
    if dead_target.exists() {
        let _ = fs::remove_file(&dead_target);
    }
    if fs::rename(source, &dead_target).is_err() {
        // If rename fails (e.g. cross-volume), copy then remove.
        if fs::copy(source, &dead_target).is_ok() {
            let _ = fs::remove_file(source);
        }
    }
    let receipt = Receipt {
        action_id: action_id.to_string(),
        kind: "unparseable".to_string(),
        status: ActionStatus::Rejected,
        actor: None,
        protocol_version: API_PROTOCOL_VERSION,
        started_at: now.clone(),
        finished_at: now,
        result: Value::Null,
        error: Some(error),
        note: Some("command quarantined to dead/".to_string()),
    };
    let _ = write_receipt(service, paths, &receipt);
    receipt
}

/// Claim each commands/<id>.json via atomic rename into processing/, dispatch,
/// write receipt, remove the processing file. Skips *.tmp. Idempotent: if
/// receipts/<id>.json already exists, the command is dropped without reprocessing.
/// Returns the receipts produced this pass.
pub fn run_queue_once(service: &mut FacialService, paths: &ApiPaths) -> Vec<Receipt> {
    let _ = paths.ensure_dirs();
    let mut receipts = Vec::new();

    let entries = match fs::read_dir(&paths.commands) {
        Ok(entries) => entries,
        Err(_) => return receipts,
    };

    // Snapshot candidate command file names first (stable iteration).
    let mut command_files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        // Skip tmp files (producer mid-write) and anything not *.json.
        if name.ends_with(".tmp") || !name.ends_with(".json") {
            continue;
        }
        command_files.push(path);
    }
    command_files.sort();

    for source in command_files {
        let file_stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Idempotency: if a receipt already exists, drop without reprocessing.
        if paths.receipt_path(&file_stem).exists() {
            let _ = fs::remove_file(&source);
            continue;
        }

        // Claim via atomic rename into processing/.
        let claimed = paths.processing.join(format!("{file_stem}.json"));
        if claimed.exists() {
            let _ = fs::remove_file(&claimed);
        }
        if fs::rename(&source, &claimed).is_err() {
            // Another worker claimed it, or it vanished. Skip.
            continue;
        }

        // Parse + dispatch.
        match parse_command_file(&claimed) {
            Ok(cmd) => {
                let receipt = dispatch(service, paths, &cmd);
                if receipt.status != ActionStatus::Accepted {
                    let _ = write_receipt(service, paths, &receipt);
                }
                let _ = fs::remove_file(&claimed);
                receipts.push(receipt);
            }
            Err(err) => {
                let receipt = quarantine_dead(service, paths, &claimed, &file_stem, err);
                // quarantine_dead consumes the file; remove any leftover.
                let _ = fs::remove_file(&claimed);
                receipts.push(receipt);
            }
        }
    }

    receipts
}

/// Bounded poll loop over run_queue_once. Stops when <api_root>/stop exists.
/// No sockets, no window, no focus. poll_ms bounds latency.
pub fn watch_queue(
    service: &mut FacialService,
    paths: &ApiPaths,
    poll_ms: u64,
) -> std::io::Result<()> {
    paths.ensure_dirs()?;
    let stop = paths.stop_file();
    let interval = std::time::Duration::from_millis(poll_ms.max(1));
    loop {
        if stop.exists() {
            break;
        }
        let _ = run_queue_once(service, paths);
        if stop.exists() {
            break;
        }
        std::thread::sleep(interval);
    }
    Ok(())
}

/// Build the current snapshot and atomically persist to state/state.json.
pub fn capture_state(service: &mut FacialService, paths: &ApiPaths) -> AppStateSnapshot {
    let cfg = service.config().clone();
    let models = service.list_models();
    let plugins = plugins_as_manifests(service.list_plugins());
    let worktrees = worktrees_as_strings(service);
    let lanes = service.list_lanes().unwrap_or_default();

    let snapshot = AppStateSnapshot {
        protocol_version: API_PROTOCOL_VERSION,
        captured_at: now_rfc3339(),
        repo_root: cfg.repo_root.to_string_lossy().to_string(),
        workspace_root: cfg.workspace_root.to_string_lossy().to_string(),
        worktrees_root: cfg.worktrees_root.to_string_lossy().to_string(),
        api_root: cfg.api_root.to_string_lossy().to_string(),
        ingest_in_place_default: cfg.ingest_in_place_default,
        models,
        plugins,
        worktrees,
        lanes,
        // headless defaults for live-GUI fields
        active_tab: String::new(),
        project_name: String::new(),
        worktree_path: String::new(),
        in_place: cfg.ingest_in_place_default,
        selected_features: Vec::new(),
        running_pipeline: false,
        run_output: String::new(),
        media_tabs: Value::Null,
        media_folder_navigation: Value::Null,
        media_controller: Value::Null,
        media_video: Value::Null,
    };

    // Persist best-effort; capture_state always returns the snapshot.
    if let Ok(serialized) = serde_json::to_string_pretty(&snapshot) {
        let _ = atomic_write(&paths.state_file, &serialized);
    }
    snapshot
}

fn ui_intent_claim_path(paths: &ApiPaths, action_id: &str) -> PathBuf {
    paths
        .intents_processing
        .join(format!("{action_id}.{}.json", std::process::id()))
}

fn ui_intent_claim_identity(path: &Path) -> (String, Option<u32>) {
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    match stem.rsplit_once('.') {
        Some((action_id, owner)) => match owner.parse::<u32>() {
            Ok(owner) => (action_id.to_string(), Some(owner)),
            Err(_) => (stem.to_string(), None),
        },
        None => (stem.to_string(), None),
    }
}

#[cfg(windows)]
fn process_is_alive(process_id: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let queried = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0;
    unsafe { CloseHandle(process) };
    queried && exit_code == STILL_ACTIVE as u32
}

#[cfg(target_os = "linux")]
fn process_is_alive(process_id: u32) -> bool {
    Path::new("/proc").join(process_id.to_string()).exists()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_is_alive(process_id: u32) -> bool {
    // Conservative fallback: never reclaim another process's owned claim on a
    // platform where this product has no native liveness probe.
    process_id == std::process::id()
}

/// GUI side: atomically claim the oldest `intents/<id>.json` into
/// `intents/processing/`, then return its command. The processing copy remains
/// durable until `mark_intent_applied` has persisted the terminal receipt.
/// None when no pending intent. Ignores *.tmp.
pub fn poll_pending_intent(paths: &ApiPaths) -> Option<Command> {
    let _ = fs::create_dir_all(&paths.intents_processing);
    let entries = fs::read_dir(&paths.intents).ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if name.ends_with(".tmp") || !name.ends_with(".json") {
            continue;
        }
        candidates.push(path);
    }
    if candidates.is_empty() {
        return None;
    }

    // FIFO: oldest by modified time, falling back to lexical name order.
    candidates.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        match (ma, mb) {
            (Some(ta), Some(tb)) => ta.cmp(&tb).then_with(|| a.cmp(b)),
            _ => a.cmp(b),
        }
    });

    for source in candidates {
        let action_id = source.file_stem()?.to_string_lossy();
        let claimed = ui_intent_claim_path(paths, &action_id);
        // Never destroy an earlier in-flight claim. A concurrent GUI either
        // wins this rename or observes that the source has vanished.
        if claimed.exists() || fs::rename(&source, &claimed).is_err() {
            continue;
        }
        // A malformed claimed file is deliberately retained for startup
        // recovery instead of being silently discarded.
        return parse_command_file(&claimed).ok();
    }
    None
}

/// GUI side: durably persist an Applied/Rejected receipt and its audit copy,
/// then delete the claimed intent. Any failure leaves the processing claim in
/// place so startup recovery can safely requeue or finish it.
pub fn mark_intent_applied(
    service: &mut FacialService,
    paths: &ApiPaths,
    receipt: &Receipt,
) -> std::io::Result<()> {
    if !matches!(
        receipt.status,
        ActionStatus::Applied | ActionStatus::Rejected
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "UI intent finalization requires an applied or rejected receipt",
        ));
    }
    let claimed =
        paths
            .intents_processing
            .join(format!("{}.{}.json", receipt.action_id, std::process::id()));
    if !claimed.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "current GUI process does not own UI intent claim {}",
                receipt.action_id
            ),
        ));
    }
    let applied_target = paths
        .intents_applied
        .join(format!("{}.json", receipt.action_id));
    let serialized = serde_json::to_string_pretty(receipt)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

    // The receipt is the externally authoritative completion signal. Persist
    // it before the audit copy and before consuming the processing claim.
    write_receipt(service, paths, receipt)?;
    atomic_write(&applied_target, &serialized)?;
    match fs::remove_file(&claimed) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Startup recovery for UI-intent claims interrupted between atomic claim and
/// durable completion. Accepted/non-terminal claims return to `intents/`;
/// terminal receipts reconstruct the applied audit copy and consume the claim.
pub fn recover_ui_intents(paths: &ApiPaths) -> std::io::Result<usize> {
    paths.ensure_dirs()?;
    let mut recovered = 0usize;
    let entries = match fs::read_dir(&paths.intents_processing) {
        Ok(entries) => entries,
        Err(_) => return Ok(0),
    };
    for entry in entries.flatten() {
        let claimed = entry.path();
        if !claimed.is_file() {
            continue;
        }
        let Some(name) = claimed.file_name().map(|name| name.to_owned()) else {
            continue;
        };
        let name_text = name.to_string_lossy();
        if name_text.ends_with(".tmp") || !name_text.ends_with(".json") {
            continue;
        }
        let (action_id, owner_process_id) = ui_intent_claim_identity(&claimed);
        if owner_process_id.is_some_and(process_is_alive) {
            // Another live GUI still owns this claim. Startup recovery must not
            // requeue it and create a second application of the same action.
            continue;
        }
        let receipt_path = paths.receipt_path(&action_id);
        let terminal_receipt = fs::read_to_string(&receipt_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Receipt>(&raw).ok())
            .filter(|receipt| {
                matches!(
                    receipt.status,
                    ActionStatus::Applied | ActionStatus::Rejected
                )
            });

        if let Some(receipt) = terminal_receipt {
            let serialized = serde_json::to_string_pretty(&receipt)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let applied_target = paths
                .intents_applied
                .join(format!("{}.json", receipt.action_id));
            atomic_write(&applied_target, &serialized)?;
            fs::remove_file(&claimed)?;
            recovered += 1;
            continue;
        }

        let destination = paths.intents.join(format!("{action_id}.json"));
        if destination.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "cannot recover UI intent {}; a queued intent with the same ID exists",
                    action_id
                ),
            ));
        }
        fs::rename(&claimed, &destination)?;
        recovered += 1;
    }
    Ok(recovered)
}

/// Startup recovery: move processing/<id>.json with no matching receipt back to commands/.
pub fn recover_processing(paths: &ApiPaths) -> std::io::Result<usize> {
    paths.ensure_dirs()?;
    let mut recovered = 0usize;
    let entries = match fs::read_dir(&paths.processing) {
        Ok(entries) => entries,
        Err(_) => return Ok(0),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if name.ends_with(".tmp") || !name.ends_with(".json") {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // If a receipt already exists, the command completed; drop the leftover.
        if paths.receipt_path(&stem).exists() {
            let _ = fs::remove_file(&path);
            continue;
        }
        // Otherwise, requeue it.
        let dest = paths.commands.join(&name);
        if dest.exists() {
            let _ = fs::remove_file(&dest);
        }
        if fs::rename(&path, &dest).is_ok() {
            recovered += 1;
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ui_snapshot_is_a_receipt_backed_ui_intent() {
        let command = CommandKind::UiSnapshot {
            output: Some(".facial/ui-snapshots/live-ui/proof.png".to_string()),
        };
        assert!(command.is_ui_intent());
        assert_eq!(command.id_str(), "ui_snapshot");
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial_api_test_{}_{}",
            name,
            Uuid::new_v4().to_string().replace('-', "_")
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_config(root: &Path) -> AppConfig {
        AppConfig {
            repo_root: root.to_path_buf(),
            workspace_root: root.to_path_buf(),
            worktrees_root: root.join("worktrees"),
            model_registry_path: root.join("data").join("model_registry.json"),
            debug_log_path: root.join("data").join("events.jsonl"),
            plugins_root: root.join("plugins"),
            api_root: root.join("data").join("api"),
            ingest_in_place_default: false,
            max_debug_events: 50,
            font_size_pt: 19.0,
            copy_location: None,
            identity_model_path: None,
            identity_detector_path: None,
            identity_reference_dir: None,
            identity_negative_dir: None,
            identity_threshold: 0.5,
            identity_margin: 0.1,
            identity_count_threshold: 0.9,
            framing_closeup_min: 0.09,
            framing_threequarter_min: 0.03,
            theme_mode: "paper".to_string(),
            landmark_model_path: None,
            media_thumb_cache_mb: 2048,
        }
    }

    fn command(kind: CommandKind) -> Command {
        Command {
            action_id: Uuid::new_v4().to_string(),
            protocol_version: API_PROTOCOL_VERSION,
            actor: Some("api-test".to_string()),
            issued_at: Some(now_rfc3339()),
            command: kind,
        }
    }

    fn terminal_ui_receipt(command: &Command, status: ActionStatus) -> Receipt {
        assert!(matches!(
            status,
            ActionStatus::Applied | ActionStatus::Rejected
        ));
        let now = now_rfc3339();
        Receipt {
            action_id: command.action_id.clone(),
            kind: command.command.id_str().to_string(),
            status,
            actor: command.actor.clone(),
            protocol_version: command.protocol_version,
            started_at: now.clone(),
            finished_at: now,
            result: serde_json::json!({"proof": true}),
            error: None,
            note: Some("test terminal UI receipt".to_string()),
        }
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_retries_a_windows_receipt_reader() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::mpsc;

        let root = test_root("atomic-write-reader-retry");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("receipt.json");
        atomic_write(&target, "accepted").unwrap();

        let held_target = target.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            // FILE_SHARE_READ only: model a Windows JSON poller that briefly
            // denies delete/replace while it consumes the accepted receipt.
            let handle = OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(held_target)
                .unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            drop(handle);
        });
        ready_rx.recv().unwrap();

        atomic_write(&target, "applied").unwrap();
        reader.join().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "applied");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ui_intent_claim_moves_atomically_and_finalizes_only_after_receipt() {
        let root = test_root("ui-intent-claim-finalize");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::SelectTab {
            tab: "media".to_string(),
        });
        let accepted = dispatch(&mut service, &paths, &command);
        assert_eq!(accepted.status, ActionStatus::Accepted);
        let persisted_accepted: Receipt = serde_json::from_str(
            &fs::read_to_string(paths.receipt_path(&command.action_id)).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted_accepted.status, ActionStatus::Accepted);

        let claimed = poll_pending_intent(&paths).expect("claim UI intent");
        assert_eq!(claimed.action_id, command.action_id);
        assert!(!paths.intent_path(&command.action_id).exists());
        let processing = ui_intent_claim_path(&paths, &command.action_id);
        assert!(processing.is_file());

        let terminal = terminal_ui_receipt(&command, ActionStatus::Applied);
        mark_intent_applied(&mut service, &paths, &terminal).unwrap();
        assert!(!processing.exists());
        assert!(paths
            .intents_applied
            .join(format!("{}.json", command.action_id))
            .is_file());
        let persisted: Receipt = serde_json::from_str(
            &fs::read_to_string(paths.receipt_path(&command.action_id)).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.status, ActionStatus::Applied);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ui_intent_finalization_requires_terminal_status_and_current_process_claim() {
        let root = test_root("ui-intent-finalize-ownership");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::SelectTab {
            tab: "media".to_string(),
        });
        let terminal = terminal_ui_receipt(&command, ActionStatus::Applied);
        assert_eq!(
            mark_intent_applied(&mut service, &paths, &terminal)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!paths.receipt_path(&command.action_id).exists());

        let accepted = dispatch(&mut service, &paths, &command);
        write_receipt(&mut service, &paths, &accepted).unwrap();
        assert!(poll_pending_intent(&paths).is_some());
        assert_eq!(
            mark_intent_applied(&mut service, &paths, &accepted)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(ui_intent_claim_path(&paths, &command.action_id).is_file());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_terminal_receipt_write_keeps_claim_and_recovery_requeues_it() {
        let root = test_root("ui-intent-failed-receipt-recovery");
        let mut service = FacialService::new(test_config(&root));
        let mut paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::SelectTab {
            tab: "media".to_string(),
        });
        let accepted = dispatch(&mut service, &paths, &command);
        write_receipt(&mut service, &paths, &accepted).unwrap();
        assert!(poll_pending_intent(&paths).is_some());
        let processing = ui_intent_claim_path(&paths, &command.action_id);

        let receipt_dir = paths.receipts.clone();
        let blocked = root.join("blocked-receipts");
        fs::write(&blocked, b"not a directory").unwrap();
        paths.receipts = blocked;
        let terminal = terminal_ui_receipt(&command, ActionStatus::Applied);
        assert!(mark_intent_applied(&mut service, &paths, &terminal).is_err());
        assert!(processing.is_file(), "failed persistence must retain claim");
        assert!(!paths
            .intents_applied
            .join(format!("{}.json", command.action_id))
            .exists());

        paths.receipts = receipt_dir;
        let abandoned =
            paths
                .intents_processing
                .join(format!("{}.{}.json", command.action_id, u32::MAX));
        fs::rename(&processing, &abandoned).unwrap();
        assert_eq!(recover_ui_intents(&paths).unwrap(), 1);
        assert!(!abandoned.exists());
        assert!(paths.intent_path(&command.action_id).is_file());
        assert!(poll_pending_intent(&paths).is_some());
        mark_intent_applied(&mut service, &paths, &terminal).unwrap();
        assert!(!processing.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_does_not_steal_a_claim_from_a_live_gui_process() {
        let root = test_root("ui-intent-live-owner");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::SelectTab {
            tab: "media".to_string(),
        });
        let accepted = dispatch(&mut service, &paths, &command);
        write_receipt(&mut service, &paths, &accepted).unwrap();
        assert!(poll_pending_intent(&paths).is_some());
        let processing = ui_intent_claim_path(&paths, &command.action_id);

        assert_eq!(recover_ui_intents(&paths).unwrap(), 0);
        assert!(processing.is_file());
        assert!(!paths.intent_path(&command.action_id).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_finishes_auditing_terminal_receipt_without_reapplying_intent() {
        let root = test_root("ui-intent-terminal-recovery");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::SelectTab {
            tab: "media".to_string(),
        });
        let accepted = dispatch(&mut service, &paths, &command);
        write_receipt(&mut service, &paths, &accepted).unwrap();
        assert!(poll_pending_intent(&paths).is_some());
        let processing = ui_intent_claim_path(&paths, &command.action_id);

        // Simulate a crash after the authoritative terminal receipt write but
        // before the audit copy and processing-claim deletion.
        let terminal = terminal_ui_receipt(&command, ActionStatus::Rejected);
        write_receipt(&mut service, &paths, &terminal).unwrap();
        assert!(processing.is_file());
        let abandoned =
            paths
                .intents_processing
                .join(format!("{}.{}.json", command.action_id, u32::MAX));
        fs::rename(&processing, &abandoned).unwrap();
        assert_eq!(recover_ui_intents(&paths).unwrap(), 1);
        assert!(!abandoned.exists());
        assert!(!paths.intent_path(&command.action_id).exists());
        let audit: Receipt = serde_json::from_str(
            &fs::read_to_string(
                paths
                    .intents_applied
                    .join(format!("{}.json", command.action_id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(audit.status, ActionStatus::Rejected);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn select_tab_accepts_media_vocab() {
        let root = test_root("select-tab-media");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let receipt = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SelectTab {
                tab: "media".to_string(),
            }),
        );

        assert_eq!(receipt.status, ActionStatus::Accepted);
        assert_eq!(receipt.result["tab"], "media");
    }

    #[test]
    fn color_label_api_persists_backend_hex_and_rejects_invalid_input() {
        let root = test_root("color-label-api");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let configured = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelConfigure {
                id: "red".to_string(),
                name: "Selects".to_string(),
                hex: "12abef".to_string(),
            }),
        );
        assert_eq!(configured.status, ActionStatus::Ok);
        let red = configured.result["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|label| label["id"] == "red")
            .unwrap();
        assert_eq!(red["name"], "Selects");
        assert_eq!(red["hex"], "#12ABEF");

        let listed = dispatch(&mut service, &paths, &command(CommandKind::MediaLabelsList));
        assert_eq!(listed.status, ActionStatus::Ok);
        let red = listed.result["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|label| label["id"] == "red")
            .unwrap();
        assert_eq!(red["name"], "Selects");
        assert_eq!(red["hex"], "#12ABEF");

        let invalid = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelConfigure {
                id: "red".to_string(),
                name: "Selects".to_string(),
                hex: "#xyz".to_string(),
            }),
        );
        assert_eq!(invalid.status, ActionStatus::Rejected);
        assert!(invalid
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid color label hex")));
        assert_eq!(
            MediaDb::open(&root)
                .color_label_definitions()
                .into_iter()
                .find(|label| label.id == "red")
                .unwrap()
                .hex,
            "#12ABEF"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dynamic_label_api_crud_multi_assignment_and_confirmed_delete() {
        let root = test_root("dynamic-label-api");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let asset = root.join("asset.jpg").to_string_lossy().to_string();

        let created = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelCreate {
                name: "Selects".to_string(),
                hex: "#123ABC".to_string(),
                path: Some(asset.clone()),
            }),
        );
        assert_eq!(created.status, ActionStatus::Ok);
        let id = created.result["label"]["id"].as_str().unwrap().to_string();
        assert_eq!(created.result["assigned_labels"][0], id);

        let add = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelAssign {
                path: asset.clone(),
                id: Some("blue".to_string()),
                action: "add".to_string(),
            }),
        );
        assert_eq!(add.status, ActionStatus::Ok);
        assert_eq!(add.result["labels"].as_array().unwrap().len(), 2);

        let meta = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaMetaGet {
                path: asset.clone(),
            }),
        );
        assert_eq!(meta.result["label"], id);
        assert_eq!(meta.result["labels"].as_array().unwrap().len(), 2);

        let updated = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelUpdate {
                id: id.clone(),
                name: Some("Keepers".to_string()),
                hex: Some("#ABCDEF".to_string()),
            }),
        );
        assert_eq!(updated.status, ActionStatus::Ok);
        assert_eq!(updated.result["label"]["name"], "Keepers");

        let refused = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelDelete {
                id: id.clone(),
                confirmed: false,
            }),
        );
        assert_eq!(refused.status, ActionStatus::Rejected);
        assert_eq!(refused.result["usage_count"], 1);
        assert_eq!(refused.result["confirmation_required"], true);

        let deleted = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelDelete {
                id: id.clone(),
                confirmed: true,
            }),
        );
        assert_eq!(deleted.status, ActionStatus::Ok);
        assert_eq!(deleted.result["deleted"]["assignments_removed"], 1);
        let db = MediaDb::open(&root);
        assert_eq!(db.labels(&asset), vec!["blue"]);
        assert!(db.find_color_label_id("Keepers").is_none());
        drop(db);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn live_label_mutation_validates_and_persists_as_ui_intent() {
        let root = test_root("live-label-intent");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let accepted = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelMutation {
                action: "add".to_string(),
                path: Some(root.join("asset.jpg").to_string_lossy().to_string()),
                id: Some("red".to_string()),
                name: None,
                hex: None,
                confirmed: false,
            }),
        );
        assert_eq!(accepted.status, ActionStatus::Accepted);

        let rejected = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelMutation {
                action: "create".to_string(),
                path: None,
                id: None,
                name: Some("missing color".to_string()),
                hex: None,
                confirmed: false,
            }),
        );
        assert_eq!(rejected.status, ActionStatus::Rejected);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folder_navigator_intent_accepts_actions_and_rejects_unknown_vocab() {
        let root = test_root("folder-navigator-intent");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let accepted = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaFolderNavigate {
                action: "down".to_string(),
            }),
        );
        assert_eq!(accepted.status, ActionStatus::Accepted);
        assert_eq!(accepted.result["action"], "down");

        let rejected = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaFolderNavigate {
                action: "teleport".to_string(),
            }),
        );
        assert_eq!(rejected.status, ActionStatus::Rejected);
        assert!(rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown folder navigator action")));
    }

    #[test]
    fn media_tabs_intent_validates_stable_id_and_path_fields() {
        let root = test_root("media-tabs-intent");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let listed = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "list".to_string(),
                tab_id: None,
                path: None,
            }),
        );
        assert_eq!(listed.status, ActionStatus::Accepted);

        let opened = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "open".to_string(),
                tab_id: None,
                path: Some(root.join("folder").to_string_lossy().to_string()),
            }),
        );
        assert_eq!(opened.status, ActionStatus::Accepted);

        let selected = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "select".to_string(),
                tab_id: Some("media-tab-7".to_string()),
                path: None,
            }),
        );
        assert_eq!(selected.status, ActionStatus::Accepted);

        let navigate = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "navigate_grid".to_string(),
                tab_id: None,
                path: Some("page_down".to_string()),
            }),
        );
        assert_eq!(navigate.status, ActionStatus::Accepted);

        let navigate_without_direction = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "navigate_grid".to_string(),
                tab_id: None,
                path: None,
            }),
        );
        assert_eq!(navigate_without_direction.status, ActionStatus::Rejected);

        let chrome = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "set_chrome".to_string(),
                tab_id: None,
                path: Some("hidden".to_string()),
            }),
        );
        assert_eq!(chrome.status, ActionStatus::Accepted);

        let split = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "set_split".to_string(),
                tab_id: None,
                path: Some("0.55".to_string()),
            }),
        );
        assert_eq!(split.status, ActionStatus::Accepted);

        let split_without_ratio = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "set_split".to_string(),
                tab_id: None,
                path: None,
            }),
        );
        assert_eq!(split_without_ratio.status, ActionStatus::Rejected);

        let chrome_without_state = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "set_chrome".to_string(),
                tab_id: None,
                path: None,
            }),
        );
        assert_eq!(chrome_without_state.status, ActionStatus::Rejected);

        let rejected = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaTabs {
                action: "close".to_string(),
                tab_id: None,
                path: None,
            }),
        );
        assert_eq!(rejected.status, ActionStatus::Rejected);
        assert!(rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid media_tabs intent")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn video_control_intent_validates_actions_and_numeric_values() {
        let root = test_root("video-control-intent");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let accepted = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaVideoControl {
                action: "seek_ms".to_string(),
                value: Some(2_500),
                output: None,
            }),
        );
        assert_eq!(accepted.status, ActionStatus::Accepted);
        assert_eq!(accepted.result["action"], "seek_ms");
        assert_eq!(accepted.result["value"], 2_500);

        let library_play = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaVideoControl {
                action: "play_library".to_string(),
                value: None,
                output: None,
            }),
        );
        assert_eq!(library_play.status, ActionStatus::Accepted);
        assert_eq!(library_play.result["action"], "play_library");

        let missing_value = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaVideoControl {
                action: "volume".to_string(),
                value: None,
                output: None,
            }),
        );
        assert_eq!(missing_value.status, ActionStatus::Rejected);

        let unknown = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaVideoControl {
                action: "rewind_the_world".to_string(),
                value: None,
                output: None,
            }),
        );
        assert_eq!(unknown.status, ActionStatus::Rejected);

        let capture = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaVideoControl {
                action: "capture_frame".to_string(),
                value: None,
                output: Some(".facial/ui-snapshots/proof.png".to_string()),
            }),
        );
        assert_eq!(capture.status, ActionStatus::Accepted);
        assert_eq!(capture.result["action"], "capture_frame");
        assert_eq!(capture.result["output"], ".facial/ui-snapshots/proof.png");
    }

    #[test]
    fn lane_dispatch_receipts_and_state_snapshot_include_lanes() {
        let root = test_root("lanes");
        let source = root.join("shoot-a");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.jpg"), b"not-a-real-image").unwrap();
        fs::write(source.join("ignore.txt"), b"ignore").unwrap();

        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let set = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SetLane {
                lane_id: "lane-001".to_string(),
                name: Some("Shoot A".to_string()),
                mode: Some("batch".to_string()),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                steal: false,
                feature_keys: Some(vec!["facet:quality_pass".to_string()]),
            }),
        );
        assert_eq!(set.status, ActionStatus::Ok);
        assert_eq!(set.result["lane_id"], "lane-001");

        let scan = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::ScanLane {
                lane_id: "lane-001".to_string(),
                steal: false,
            }),
        );
        assert_eq!(scan.status, ActionStatus::Ok);
        assert_eq!(scan.result["item_count"], 1);

        let claim = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::ClaimLane {
                lane_id: "lane-001".to_string(),
                actor: "agent-a".to_string(),
                steal: false,
            }),
        );
        assert_eq!(claim.status, ActionStatus::Ok);
        assert_eq!(claim.result["claim_owner"], "agent-a");

        let blocked = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::ClaimLane {
                lane_id: "lane-001".to_string(),
                actor: "agent-b".to_string(),
                steal: false,
            }),
        );
        assert_eq!(blocked.status, ActionStatus::Error);
        assert!(blocked.error.unwrap().contains("already claimed"));

        let list = dispatch(&mut service, &paths, &command(CommandKind::ListLanes));
        assert_eq!(list.status, ActionStatus::Ok);
        assert!(list.result.as_array().unwrap().len() >= 2);

        let state = dispatch(&mut service, &paths, &command(CommandKind::GetState));
        assert_eq!(state.status, ActionStatus::Ok);
        assert_eq!(state.result["lanes"][0]["lane_id"], "lane-001");
        assert_eq!(state.result["lanes"][0]["item_count"], 1);
    }

    #[test]
    fn set_lane_command_json_defaults_to_recursive_scan() {
        let raw = r#"{
            "action_id": "set-lane-default-recursive",
            "kind": "set_lane",
            "lane_id": "lane-001",
            "folder": "D:/shoot-a"
        }"#;

        let parsed = parse_command_str(raw).unwrap();

        match parsed.command {
            CommandKind::SetLane { recursive, .. } => assert_eq!(recursive, None),
            other => panic!("expected set_lane, got {}", other.id_str()),
        }
    }

    #[test]
    fn set_lane_partial_json_update_preserves_existing_fields() {
        let root = test_root("partial_set_lane");
        let source = root.join("shoot-a");
        fs::create_dir_all(&source).unwrap();
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());

        let full = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SetLane {
                lane_id: "lane-001".to_string(),
                name: Some("Before".to_string()),
                mode: Some("batch".to_string()),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(false),
                steal: false,
                feature_keys: Some(vec![
                    "facet:quality_pass".to_string(),
                    "deepface:detect".to_string(),
                ]),
            }),
        );
        assert_eq!(full.status, ActionStatus::Ok);

        let raw = r#"{
            "action_id": "partial-set",
            "kind": "set_lane",
            "lane_id": "lane-001",
            "name": "After"
        }"#;
        let partial = parse_command_str(raw).unwrap();
        let receipt = dispatch(&mut service, &paths, &partial);

        assert_eq!(receipt.status, ActionStatus::Ok);
        assert_eq!(receipt.result["name"], "After");
        assert_eq!(receipt.result["mode"], "batch");
        assert_eq!(receipt.result["recursive"], false);
        assert_eq!(receipt.result["feature_keys"][0], "facet:quality_pass");
        assert_eq!(receipt.result["feature_keys"][1], "deepface:detect");
    }

    #[test]
    fn json_lane_claim_uses_top_level_actor_for_ownership() {
        let root = test_root("json_actor_claim");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let raw = r#"{
            "action_id": "claim-from-json",
            "actor": "json-agent",
            "kind": "claim_lane",
            "lane_id": "lane-001"
        }"#;
        let cmd = parse_command_str(raw).unwrap();

        let receipt = dispatch(&mut service, &paths, &cmd);

        assert_eq!(receipt.status, ActionStatus::Ok);
        assert_eq!(receipt.actor.as_deref(), Some("json-agent"));
        assert_eq!(receipt.result["claim_owner"], "json-agent");
    }

    #[test]
    fn start_all_lane_batches_dispatches_aggregate_receipt() {
        let root = test_root("start_all_lane_batches");
        let source = root.join("source");
        let output = root.join("out");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.jpg"), b"not-a-real-image").unwrap();
        let mut cfg = test_config(&root);
        cfg.copy_location = Some(output);
        let mut service = FacialService::new(cfg);
        let paths = ApiPaths::from_config(service.config());

        let set = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SetLane {
                lane_id: "lane-001".to_string(),
                name: Some("Batch Lane".to_string()),
                mode: Some("batch".to_string()),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                steal: false,
                feature_keys: Some(vec!["invalid-feature-key".to_string()]),
            }),
        );
        assert_eq!(set.status, ActionStatus::Ok);
        let scan = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::ScanLane {
                lane_id: "lane-001".to_string(),
                steal: false,
            }),
        );
        assert_eq!(scan.status, ActionStatus::Ok);

        let receipt = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::StartAllLaneBatches {
                project_name: "Batch Project".to_string(),
                feature_keys: Vec::new(),
                concurrency_limit: 2,
                in_place: false,
                steal: false,
            }),
        );

        assert_eq!(receipt.status, ActionStatus::Ok);
        assert_eq!(receipt.result["concurrency_limit"], 2);
        assert_eq!(receipt.result["total_lanes"], 1);
        assert_eq!(receipt.result["results"][0]["lane_id"], "lane-001");
        assert_eq!(receipt.result["results"][0]["item_count"], 1);
        assert!(receipt.result["results"][0]["run_id"].is_string());
    }

    #[test]
    fn start_all_lane_batches_writes_child_lane_receipts() {
        let root = test_root("start_all_lane_batches_child_receipts");
        let source = root.join("source");
        let output = root.join("out");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.jpg"), b"not-a-real-image").unwrap();
        let mut cfg = test_config(&root);
        cfg.copy_location = Some(output);
        let mut service = FacialService::new(cfg);
        let paths = ApiPaths::from_config(service.config());

        let set = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SetLane {
                lane_id: "lane-001".to_string(),
                name: Some("Batch Lane".to_string()),
                mode: Some("batch".to_string()),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                steal: false,
                feature_keys: Some(vec!["invalid-feature-key".to_string()]),
            }),
        );
        assert_eq!(set.status, ActionStatus::Ok);
        let scan = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::ScanLane {
                lane_id: "lane-001".to_string(),
                steal: false,
            }),
        );
        assert_eq!(scan.status, ActionStatus::Ok);

        let receipt = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::StartAllLaneBatches {
                project_name: "Batch Project".to_string(),
                feature_keys: Vec::new(),
                concurrency_limit: 2,
                in_place: false,
                steal: false,
            }),
        );

        assert_eq!(receipt.status, ActionStatus::Ok);
        let child_id = receipt.result["results"][0]["action_id"].as_str().unwrap();
        let child_path = paths.receipt_path(child_id);
        assert!(child_path.is_file());
        let child: Receipt =
            serde_json::from_str(&fs::read_to_string(child_path).unwrap()).unwrap();
        assert_eq!(child.action_id, child_id);
        assert_eq!(child.kind, "start_lane_batch");
        assert_eq!(child.result["lane_id"], "lane-001");
    }

    #[test]
    fn direct_failed_lane_batch_status_points_at_command_receipt() {
        let root = test_root("direct_failed_lane_batch_receipt");
        let output = root.join("out");
        fs::create_dir_all(&output).unwrap();
        let mut cfg = test_config(&root);
        cfg.copy_location = Some(output);
        let mut service = FacialService::new(cfg);
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let set = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::SetLane {
                lane_id: "lane-003".to_string(),
                name: Some("Missing Scan".to_string()),
                mode: Some("batch".to_string()),
                folder: None,
                recursive: Some(true),
                steal: false,
                feature_keys: Some(vec!["invalid-feature-key".to_string()]),
            }),
        );
        assert_eq!(set.status, ActionStatus::Ok);

        let mut cmd = command(CommandKind::StartLaneBatch {
            lane_id: "lane-003".to_string(),
            project_name: "Batch Project".to_string(),
            feature_keys: Vec::new(),
            in_place: false,
            steal: false,
        });
        cmd.action_id = "direct-lane-batch-failure".to_string();
        let receipt = dispatch(&mut service, &paths, &cmd);
        write_receipt(&mut service, &paths, &receipt).unwrap();

        assert_eq!(receipt.status, ActionStatus::Error);
        assert!(paths.receipt_path(&cmd.action_id).is_file());
        let status = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::LaneStatus {
                lane_id: Some("lane-003".to_string()),
            }),
        );
        assert_eq!(status.status, ActionStatus::Ok);
        assert_eq!(status.result[0]["batch_action_id"], cmd.action_id);
        assert_eq!(status.result[0]["batch_status"], "error");
        assert!(status.result[0]["last_batch_error"]
            .as_str()
            .unwrap()
            .contains("scanned inventory"));
    }
}
