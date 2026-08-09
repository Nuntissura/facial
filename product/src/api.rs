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
            CommandKind::MediaSetFolder { .. } => "media_set_folder",
            CommandKind::MediaSearch { .. } => "media_search",
            CommandKind::MediaSelect { .. } => "media_select",
            CommandKind::MediaOpenSelected => "media_open_selected",
            CommandKind::MediaFolderNavigate { .. } => "media_folder_navigate",
            CommandKind::MediaVideoControl { .. } => "media_video_control",
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
                | CommandKind::MediaSetFolder { .. }
                | CommandKind::MediaSearch { .. }
                | CommandKind::MediaSelect { .. }
                | CommandKind::MediaOpenSelected
                | CommandKind::MediaFolderNavigate { .. }
                | CommandKind::MediaVideoControl { .. }
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
}

// ---------- on-disk path layout ----------

pub struct ApiPaths {
    pub root: PathBuf,            // <data>/api
    pub commands: PathBuf,        // <data>/api/commands
    pub processing: PathBuf,      // <data>/api/processing
    pub receipts: PathBuf,        // <data>/api/receipts
    pub intents: PathBuf,         // <data>/api/intents
    pub intents_applied: PathBuf, // <data>/api/intents/applied
    pub dead: PathBuf,            // <data>/api/dead
    pub state_file: PathBuf,      // <data>/api/state/state.json
}

impl ApiPaths {
    pub fn from_config(cfg: &AppConfig) -> Self {
        let root = cfg.api_root.clone();
        Self {
            commands: root.join("commands"),
            processing: root.join("processing"),
            receipts: root.join("receipts"),
            intents: root.join("intents"),
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
/// existing target). On Windows rename-over-existing fails, so the existing
/// target is removed first.
fn atomic_write(target: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, contents.as_bytes())?;
    if target.exists() {
        let _ = fs::remove_file(target);
    }
    match fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Best-effort cleanup of the temp file on failure.
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Construct a terminal/accepted/rejected receipt for `cmd`.
/// Errored receipt for media commands when the media DB cannot be opened at
/// all (typically: a live GUI holds redb's exclusive lock).
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
        return dispatch_ui_intent(service, paths, cmd, started_at);
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
        // media DB. redb holds an EXCLUSIVE lock per open handle: while a live
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
fn dispatch_ui_intent(
    _service: &mut FacialService,
    paths: &ApiPaths,
    cmd: &Command,
    started_at: String,
) -> Receipt {
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
        const ACTION_VOCAB: [&str; 12] = [
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

    if let CommandKind::MediaVideoControl {
        action,
        value,
        output,
    } = &cmd.command
    {
        const ACTION_VOCAB: [&str; 11] = [
            "status",
            "play_pause",
            "play",
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

    // Persist the full command to intents/<id>.json (atomic).
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

    // Echo the queued intent payload as the receipt result so callers can
    // verify exactly what was persisted (also repairs the long-standing
    // select_tab receipt contract: result["tab"] mirrors the request).
    let queued = serde_json::to_value(&cmd.command).unwrap_or(Value::Null);
    make_receipt(
        cmd,
        ActionStatus::Accepted,
        started_at,
        queued,
        None,
        Some("ui-intent persisted; awaiting live GUI apply".to_string()),
    )
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
                let _ = write_receipt(service, paths, &receipt);
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
    };

    // Persist best-effort; capture_state always returns the snapshot.
    if let Ok(serialized) = serde_json::to_string_pretty(&snapshot) {
        let _ = atomic_write(&paths.state_file, &serialized);
    }
    snapshot
}

/// GUI side: read+remove the oldest intents/<id>.json (FIFO), return it.
/// None when no pending intent. Ignores *.tmp.
pub fn poll_pending_intent(paths: &ApiPaths) -> Option<Command> {
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

    let oldest = candidates.into_iter().next()?;
    let raw = fs::read_to_string(&oldest).ok()?;
    let cmd = parse_command_str(&raw).ok();
    // Read+remove: the GUI now owns this intent.
    let _ = fs::remove_file(&oldest);
    cmd
}

/// GUI side: move an applied intent to intents/applied/ and append the
/// follow-up Receipt (status Applied or Rejected) via write_receipt.
pub fn mark_intent_applied(
    service: &mut FacialService,
    paths: &ApiPaths,
    receipt: &Receipt,
) -> std::io::Result<()> {
    // The intent file was already removed by poll_pending_intent; persist a
    // copy of the applied receipt for audit, then mirror the receipt.
    let applied_target = paths
        .intents_applied
        .join(format!("{}.json", receipt.action_id));
    if let Some(parent) = applied_target.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(serialized) = serde_json::to_string_pretty(receipt) {
        let _ = atomic_write(&applied_target, &serialized);
    }
    write_receipt(service, paths, receipt)
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
