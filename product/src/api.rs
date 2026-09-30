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

/// Closed WP-084 correction vocabulary. Keeping this as a wire enum prevents
/// an unknown or misspelled identity mutation from reaching the live GUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchCorrectionAction {
    Same,
    Different,
    NotSure,
    ThisIsNot,
    ChangePerson,
    RemoveAssignment,
    IgnoreFace,
    NotAFace,
    DeleteFaceAnalysis,
    ManualFace,
    MoveToLook,
    SamePersonNewLook,
    MergePeople,
    SplitPerson,
    RemovePerson,
    Undo,
}

/// Orientation-independent rectangle in the EXIF-oriented source image.
/// `left`, `top`, `width`, and `height` are normalized to `[0, 1]`; the
/// source dimensions bind the normalization to the decoded source used when
/// the operator drew the manual region.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchNormalizedFaceBounds {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub source_width: u32,
    pub source_height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchFaceMediaFence {
    pub media_key: String,
    pub media_fingerprint: String,
}

/// Optimistic-concurrency fence for a Match correction. Maps are keyed by the
/// same stable IDs carried by the request and must cover those IDs exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCorrectionExpectedRevisions {
    pub schema_generation: String,
    pub model_generation: String,
    pub catalog_revision: u64,
    #[serde(default)]
    pub person_revisions: BTreeMap<String, u64>,
    #[serde(default)]
    pub face_revisions: BTreeMap<String, u64>,
}

/// Typed payload for one receipt-backed WP-084 correction. The newtype command
/// variant serializes these fields beside `kind`; `deny_unknown_fields` keeps
/// this new contract closed without tightening older extensible variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCorrectionRequest {
    pub action: MatchCorrectionAction,
    /// Sorted, unique stable FaceIds. Actions without a face scope require an
    /// empty list; `manual_face` creates its FaceId inside the transaction.
    #[serde(default)]
    pub face_ids: Vec<String>,
    #[serde(default)]
    pub person_id: Option<String>,
    #[serde(default)]
    pub target_person_id: Option<String>,
    #[serde(default)]
    pub look_id: Option<String>,
    #[serde(default)]
    pub look_name: Option<String>,
    /// Target operation for `undo`, the exact persisted Person-operation
    /// preview token for `merge_people`, `split_person`, and `remove_person`,
    /// or the exact `batch_preview.preview_id` for a supported multi-Face
    /// correction.
    /// Split keeps this token in addition to its exact selected Face/media and
    /// revision fences: the token proves that the confirmed request is the
    /// same preview, while those narrower fences prove every selected row.
    /// This remains distinct from the outer command `action_id`.
    #[serde(default)]
    pub operation_id: Option<String>,
    /// Exact action-specific authorization returned by
    /// `match_batch_correction_preflight`. Required, in full, for every
    /// supported correction over two or more FaceIds and rejected elsewhere.
    #[serde(default)]
    pub batch_preview: Option<crate::match_store::BatchCorrectionPreview>,
    #[serde(default)]
    pub media_key: Option<String>,
    #[serde(default)]
    pub media_fingerprint: Option<String>,
    /// Exact per-Face media fences for a cross-media batch. A single-Viewer
    /// correction uses the compact `media_key`/`media_fingerprint` pair;
    /// batches use this map so every selected Face remains independently
    /// bound to its canonical asset without loading a rendered slice.
    #[serde(default)]
    pub face_media: BTreeMap<String, MatchFaceMediaFence>,
    #[serde(default)]
    pub normalized_bounds: Option<MatchNormalizedFaceBounds>,
    /// EXIF orientation vocabulary 1 through 8, required with manual bounds.
    #[serde(default)]
    pub exif_orientation: Option<u8>,
    pub expected_revisions: MatchCorrectionExpectedRevisions,
    #[serde(default)]
    pub confirmed: bool,
}

/// Closed, read-only request for the exact split-Person preview consumed by a
/// later `match_correction`/`split_person` command. The request carries the
/// same optimistic revision and per-Face media fences as the mutation so a
/// no-context model cannot obtain an apparently current preview for stale or
/// differently scoped rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSplitPersonPreflightRequest {
    pub source_person_id: String,
    pub target_person_id: String,
    /// Sorted, unique stable FaceIds; bounded by the correction envelope cap.
    pub face_ids: Vec<String>,
    /// Exact FaceId -> media identity mapping. Keys must exactly equal
    /// `face_ids`; compact single-media shorthand is intentionally absent.
    pub face_media: BTreeMap<String, MatchFaceMediaFence>,
    pub expected_revisions: MatchCorrectionExpectedRevisions,
}

/// Closed WP-085 portability/recovery vocabulary. Keeping destructive reset
/// verbs distinct prevents a generic "reset" request from silently widening
/// a derived-only rebuild into deletion of operator-owned identity truth.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMaintenanceAction {
    IdentityExportPreview,
    IdentityExport,
    IdentityImportDryRun,
    IdentityImport,
    IdentityImportRollback,
    XmpExportPreview,
    XmpExport,
    XmpImportDryRun,
    /// Parse a bounded sidecar into the terminal applied receipt only. This
    /// never applies identity truth; the returned next-action contract drives
    /// explicit per-region manual-face corrections.
    XmpImport,
    RebuildMatchAnalysisPreview,
    RebuildMatchAnalysis,
    ClearAllMatchDataPreview,
    ClearAllMatchData,
    RestoreRecoveryBundle,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchVideoAction {
    AppearanceList,
    PersonAppearanceList,
    SeekAppearance,
    InspectAppearance,
    CorrectionPreview,
    CorrectionApply,
    SplitPreview,
    SplitApply,
    ContextReview,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchVideoRequest {
    pub action: MatchVideoAction,
    #[serde(default)]
    pub after_track_id: Option<String>,
    #[serde(default)]
    pub person_id: Option<String>,
    #[serde(default)]
    pub person_cursor: Option<crate::match_store::PersonAppearanceCursor>,
    pub media_key: String,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub track_revision: Option<u64>,
    #[serde(default)]
    pub timestamp: Option<crate::match_video::VideoTime>,
    #[serde(default)]
    pub correction_action: Option<String>,
    #[serde(default)]
    pub source_person_id: Option<String>,
    #[serde(default)]
    pub target_person_id: Option<String>,
    #[serde(default)]
    pub split_observation_ids: Vec<String>,
    #[serde(default)]
    pub preview_token: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
}

pub type MatchClusterReviewRequest = crate::match_store::UnnamedClusterReviewRequest;
pub type MatchMediaContextReplaceRequest = crate::match_store::MediaContextRequest;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchMediaContextGetRequest {
    pub media_key: String,
}

pub fn validate_match_media_context_get(
    request: &MatchMediaContextGetRequest,
) -> Result<(), String> {
    let key = &request.media_key;
    if key.is_empty() || key.len() > 4096 || key.trim() != key || key.chars().any(char::is_control)
    {
        return Err("media context requires a canonical media_key of 1–4096 bytes".into());
    }
    Ok(())
}

pub fn validate_match_cluster_review(request: &MatchClusterReviewRequest) -> Result<(), String> {
    request.validate()
}

pub fn validate_match_video(request: &MatchVideoRequest) -> Result<(), String> {
    use MatchVideoAction::*;
    for text in std::iter::once(request.media_key.as_str())
        .chain(request.track_id.as_deref())
        .chain(request.after_track_id.as_deref())
        .chain(request.person_id.as_deref())
        .chain(request.source_person_id.as_deref())
        .chain(request.target_person_id.as_deref())
        .chain(request.preview_token.as_deref())
        .chain(request.split_observation_ids.iter().map(String::as_str))
    {
        if text.trim().is_empty()
            || text.trim() != text
            || text.len() > 4096
            || text.chars().any(char::is_control)
        {
            return Err("match_video identifiers must be bounded nonempty canonical text".into());
        }
    }
    if request.after_track_id.is_some() && request.action != AppearanceList {
        return Err("track pagination is only valid for appearance_list".into());
    }
    if (request.action == PersonAppearanceList) != request.person_id.is_some()
        || (request.person_cursor.is_some() && request.action != PersonAppearanceList)
    {
        return Err("Person appearance pagination requires an explicit Person".into());
    }
    let scoped = !matches!(request.action, AppearanceList | PersonAppearanceList);
    if !scoped
        && (request.track_id.is_some()
            || request.track_revision.is_some()
            || request.timestamp.is_some())
    {
        return Err("appearance_list does not accept a track selection".into());
    }
    if scoped
        && (request.track_id.is_none()
            || request.track_revision.unwrap_or(0) == 0
            || request.timestamp.is_none())
    {
        return Err(
            "match_video requires stable track ID, positive revision and exact timestamp".into(),
        );
    }
    if let Some(time) = request.timestamp {
        time.validate()?;
        if time.milliseconds()? > i64::MAX as u64 {
            return Err("video seek timestamp overflows native transport".into());
        }
    }
    let correction = matches!(request.action, CorrectionPreview | CorrectionApply);
    if correction != request.correction_action.is_some()
        || request.correction_action.as_deref().is_some_and(|action| {
            !matches!(
                action,
                "assign" | "reassign" | "remove" | "ignore" | "not_a_person"
            )
        })
    {
        return Err("match_video correction action is missing or unsupported".into());
    }
    let person_fields_valid = match request.correction_action.as_deref() {
        Some("assign") => request.target_person_id.is_some() && request.source_person_id.is_none(),
        Some("reassign") => {
            request.target_person_id.is_some()
                && request.source_person_id.is_some()
                && request.target_person_id != request.source_person_id
        }
        Some("remove") => request.source_person_id.is_some() && request.target_person_id.is_none(),
        _ => request.source_person_id.is_none() && request.target_person_id.is_none(),
    };
    if !person_fields_valid {
        return Err("match_video Person fields do not match correction action".into());
    }
    let split = matches!(request.action, SplitPreview | SplitApply);
    if request.split_observation_ids.len() > 1024
        || (split && request.split_observation_ids.is_empty())
        || (!split && !request.split_observation_ids.is_empty())
    {
        return Err("match_video split requires 1..1024 explicit observation IDs".into());
    }
    let ids: std::collections::BTreeSet<_> = request.split_observation_ids.iter().collect();
    if ids.len() != request.split_observation_ids.len() {
        return Err("duplicate split observation ID".into());
    }
    let apply = matches!(request.action, CorrectionApply | SplitApply);
    if apply != request.confirmed || apply != request.preview_token.is_some() {
        return Err("only apply requires confirmation and its exact preview token".into());
    }
    Ok(())
}

/// Typed live-GUI request for WP-085 identity exchange and recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchMaintenanceRequest {
    pub action: MatchMaintenanceAction,
    /// Bundle, recovery-bundle, or sidecar path depending on
    /// the closed action vocabulary. The service resolves and confines it.
    #[serde(default)]
    pub path: Option<String>,
    /// Exact stable Media key for one optional XMP sidecar projection.
    #[serde(default)]
    pub media_key: Option<String>,
    /// Stable source root ID -> existing destination directory. Bundle paths
    /// never carry an absolute/container path of their own.
    #[serde(default)]
    pub relocations: BTreeMap<String, String>,
    /// State/content/plan digest returned by the corresponding preview.
    #[serde(default)]
    pub expected_digest: Option<String>,
    /// Exact state-bound token returned by a dry-run/destructive preview.
    #[serde(default)]
    pub confirmation_token: Option<String>,
    /// reject (default) | replace. Replace is accepted only after dry-run and
    /// an exact state-bound confirmation.
    #[serde(default)]
    pub conflict_policy: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
}

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
    /// Read-only, privacy-redacted Match domain/index/job health.
    MatchStatus,
    /// Independently verify the frozen Match calibration evidence graph.
    MatchCalibrationVerify {
        contract: String,
        eval_root: String,
    },
    IdentityProvision {
        model: String,
        #[serde(default)]
        detector: String,
    },
    MatchFaces {
        image: String,
    },
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
    /// Exact, read-only row counts for clean-baseline and recovery proof.
    MediaDbStatus,
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
        /// Explicit authorization for an unchanged exact-frame capture while
        /// a Match surface is visible. Ordinary captures fail closed.
        #[serde(default)]
        include_sensitive_match: bool,
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
    /// Receipt-backed Match catalog, root, and job operations applied by the
    /// live GUI while it owns the embedded database.
    MatchIntent {
        /// open_people | open_suggestions | open_unidentified | open_settings |
        /// open_media_faces | open_person_faces | person_edit_preflight | refresh |
        /// create_person | update_person | set_person_preferences |
        /// open_person | configure_root | remove_root | start | pause |
        /// resume | cancel | retry | pause_all | resume_all
        action: String,
        #[serde(default)]
        id: Option<String>,
        /// Optional target Person for an exact `person_edit_preflight` merge
        /// preview. `id` remains the source Person.
        #[serde(default)]
        target_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        aliases: Vec<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        exclusions: Vec<String>,
        #[serde(default)]
        expected_revision: Option<u64>,
        #[serde(default)]
        cover_media_key: Option<String>,
        #[serde(default)]
        hidden: Option<bool>,
        #[serde(default)]
        favorite: Option<bool>,
        /// Explicit bounded page offset for People, Person-gallery, and Match
        /// Settings processing-history reads.
        /// `open_person` defaults to zero so one Person can never inherit the
        /// ambient page of a previously opened Person.
        #[serde(default)]
        offset: Option<u64>,
    },
    /// Typed corrections and manual-face mutations applied by the live GUI
    /// while it owns the embedded Match store.
    MatchCorrection(MatchCorrectionRequest),
    /// Read-only exact dry run for one supported multi-Face correction. The
    /// terminal receipt contains the full preview that must be echoed by the
    /// subsequent `match_correction` apply request.
    MatchBatchCorrectionPreflight(MatchCorrectionRequest),
    /// Exact, read-only split preview applied by the live GUI while it owns
    /// the embedded Match store. Its terminal receipt supplies the only valid
    /// `operation_id` for the subsequent split mutation.
    MatchSplitPersonPreflight(MatchSplitPersonPreflightRequest),
    /// Versioned identity exchange, sidecar interoperability, and the two
    /// explicitly non-overlapping Match recovery operations (WP-085).
    MatchMaintenance(MatchMaintenanceRequest),
    MatchVideo(MatchVideoRequest),
    MatchClusterReview(MatchClusterReviewRequest),
    MatchMediaContextGet(MatchMediaContextGetRequest),
    MatchMediaContextReplace(MatchMediaContextReplaceRequest),
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
            CommandKind::MatchStatus => "match_status",
            CommandKind::MatchCalibrationVerify { .. } => "match_calibration_verify",
            CommandKind::IdentityProvision { .. } => "identity_provision",
            CommandKind::MatchFaces { .. } => "match_faces",
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
            CommandKind::MediaDbStatus => "media_db_status",
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
            CommandKind::MatchIntent { .. } => "match_intent",
            CommandKind::MatchCorrection(..) => "match_correction",
            CommandKind::MatchBatchCorrectionPreflight(..) => "match_batch_correction_preflight",
            CommandKind::MatchSplitPersonPreflight(..) => "match_split_person_preflight",
            CommandKind::MatchMaintenance(..) => "match_maintenance",
            CommandKind::MatchVideo(..) => "match_video",
            CommandKind::MatchClusterReview(..) => "match_cluster_review",
            CommandKind::MatchMediaContextGet(..) => "match_media_context_get",
            CommandKind::MatchMediaContextReplace(..) => "match_media_context_replace",
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
                | CommandKind::MatchIntent { .. }
                | CommandKind::MatchCorrection(..)
                | CommandKind::MatchBatchCorrectionPreflight(..)
                | CommandKind::MatchSplitPersonPreflight(..)
                | CommandKind::MatchMaintenance(..)
                | CommandKind::MatchVideo(..)
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
    /// Privacy-redacted Match execution/catalog status. The live UI keeps raw
    /// paths and per-media detail on its operator-only cached projection.
    #[serde(default)]
    pub match_state: Value,
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
        CommandKind::MatchMediaContextGet(request) => {
            match service.match_media_context_get(request) {
                Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MatchMediaContextReplace(request) => {
            match service.match_media_context_replace(request) {
                Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
                Err(error) => make_receipt(
                    cmd,
                    ActionStatus::Error,
                    started_at,
                    Value::Null,
                    Some(error),
                    None,
                ),
            }
        }
        CommandKind::MatchClusterReview(request) => match service.match_cluster_review(request) {
            Ok(result) => make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None),
            Err(error) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                Value::Null,
                Some(error),
                None,
            ),
        },
        CommandKind::MatchStatus => match service.match_status() {
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
        CommandKind::MatchCalibrationVerify {
            contract,
            eval_root,
        } => match service.match_calibration_verify(contract, eval_root) {
            Ok(result) if result["strict_automatic_enabled"].as_bool() == Some(true) => {
                make_receipt(cmd, ActionStatus::Ok, started_at, result, None, None)
            }
            Ok(result) => make_receipt(
                cmd,
                ActionStatus::Error,
                started_at,
                result,
                Some("match_calibration_failed_closed".to_string()),
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
        CommandKind::IdentityProvision { model, detector } => {
            match service.set_identity_paths(model, detector) {
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
        CommandKind::MatchFaces { image } => match service.match_faces(image) {
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
        CommandKind::MediaDbStatus => {
            let workspace_root = service.config().workspace_root.clone();
            let db = MediaDb::open(&workspace_root);
            if !db.is_available() {
                return media_db_unavailable_receipt(cmd, started_at, &db);
            }
            let stats = match db.baseline_stats() {
                Ok(stats) => stats,
                Err(error) => {
                    return make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        Value::Null,
                        Some(format!("media database statistics failed: {error}")),
                        None,
                    )
                }
            };
            let clip_embeddings = match crate::media_clip::ClipIndex::open(&workspace_root)
                .and_then(|index| index.try_len())
                .and_then(|count| {
                    u64::try_from(count)
                        .map_err(|_| "CLIP embedding count overflowed u64".to_string())
                }) {
                Ok(count) => count,
                Err(error) => {
                    return make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        serde_json::json!({ "stats": stats }),
                        Some(format!("media CLIP statistics failed: {error}")),
                        None,
                    )
                }
            };
            let marker_path = MediaDb::media_state_dir(&workspace_root).join("engine.json");
            let engine_marker = match fs::read(&marker_path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string())
                }) {
                Ok(marker) => marker,
                Err(error) => {
                    return make_receipt(
                        cmd,
                        ActionStatus::Error,
                        started_at,
                        serde_json::json!({ "stats": stats, "clip_embeddings": clip_embeddings }),
                        Some(format!(
                            "media engine marker {} is unreadable: {error}",
                            marker_path.display()
                        )),
                        None,
                    )
                }
            };
            let clean_user_state = stats.notes == 0
                && stats.tags == 0
                && stats.label_rows == 0
                && stats.label_assignments == 0
                && stats.favorites == 0
                && stats.settings_user == 0
                && !stats.color_label_catalog_customized
                && stats.inventory_manifests == 0
                && stats.inventory_items == 0
                && stats.inventory_staging == 0
                && clip_embeddings == 0;
            make_receipt(
                cmd,
                ActionStatus::Ok,
                started_at,
                serde_json::json!({
                    "workspace_root": workspace_root,
                    "store_path": MediaDb::db_path(&workspace_root),
                    "marker_path": marker_path,
                    "engine_marker": engine_marker,
                    "stats": stats,
                    "clip_embeddings": clip_embeddings,
                    "clean_user_state": clean_user_state,
                    "db_status": db.status(),
                }),
                None,
                None,
            )
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
    if let CommandKind::UiSnapshot {
        output: Some(output),
        ..
    } = &cmd.command
    {
        if let Err(error) = validate_ui_snapshot_output_syntax(output) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                None,
                Some(error),
            );
        }
    }

    // Light validation: SelectTab vocab must be one of the known tabs.
    if let CommandKind::SelectTab { tab } = &cmd.command {
        const TAB_VOCAB: [&str; 10] = [
            "project",
            "quality_iq",
            "identity",
            "duplicates",
            "run_debug",
            "manual",
            "media",
            "match",
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

    if let CommandKind::MatchIntent {
        action,
        id,
        target_id,
        name,
        path,
        expected_revision,
        offset,
        ..
    } = &cmd.command
    {
        const ACTIONS: [&str; 20] = [
            "open_people",
            "open_suggestions",
            "open_unidentified",
            "open_settings",
            "open_media_faces",
            "open_person_faces",
            "person_edit_preflight",
            "refresh",
            "create_person",
            "update_person",
            "set_person_preferences",
            "open_person",
            "configure_root",
            "remove_root",
            "start",
            "pause",
            "resume",
            "cancel",
            "retry",
            "pause_all",
        ];
        let invalid = !ACTIONS.contains(&action.as_str()) && action != "resume_all"
            || (target_id.is_some() && action != "person_edit_preflight")
            || (action == "person_edit_preflight"
                && target_id
                    .as_ref()
                    .is_some_and(|target_id| id.as_ref() == Some(target_id)))
            || offset.is_some_and(|value| value > 10_000_000)
            || (offset.is_some()
                && !matches!(
                    action.as_str(),
                    "open_people" | "open_settings" | "open_person" | "open_person_faces"
                ))
            || (action == "create_person" && name.is_none())
            || (action == "update_person"
                && (id.is_none() || name.is_none() || expected_revision.is_none()))
            || (action == "set_person_preferences"
                && (id.is_none() || expected_revision.is_none()))
            || (action == "person_edit_preflight" && id.is_none())
            || (matches!(
                action.as_str(),
                "open_person"
                    | "open_media_faces"
                    | "open_person_faces"
                    | "remove_root"
                    | "start"
                    | "pause"
                    | "resume"
                    | "cancel"
                    | "retry"
            ) && id.is_none())
            || (action == "configure_root" && path.is_none());
        if invalid {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some("invalid Match intent fields or action vocabulary".to_string()),
                None,
            );
        }
    }

    if let CommandKind::MatchCorrection(correction) = &cmd.command {
        if let Err(error) = validate_match_correction(correction) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
                None,
            );
        }
    }

    if let CommandKind::MatchBatchCorrectionPreflight(preflight) = &cmd.command {
        if let Err(error) = validate_match_batch_correction_preflight(preflight) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
                None,
            );
        }
    }

    if let CommandKind::MatchSplitPersonPreflight(preflight) = &cmd.command {
        if let Err(error) = validate_match_split_person_preflight(preflight) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
                None,
            );
        }
    }

    if let CommandKind::MatchMaintenance(request) = &cmd.command {
        if let Err(error) = validate_match_maintenance(request) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
                None,
            );
        }
    }

    if let CommandKind::MatchClusterReview(request) = &cmd.command {
        if let Err(error) = validate_match_cluster_review(request) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
                None,
            );
        }
    }
    if let CommandKind::MatchVideo(request) = &cmd.command {
        if let Err(error) = validate_match_video(request) {
            return make_receipt(
                cmd,
                ActionStatus::Rejected,
                started_at,
                Value::Null,
                Some(error),
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

fn validate_match_maintenance(request: &MatchMaintenanceRequest) -> Result<(), String> {
    use MatchMaintenanceAction as Action;

    if request.media_key.as_ref().is_some_and(|media_key| {
        media_key.trim().is_empty() || media_key.trim() != media_key || media_key.len() > 4096
    }) {
        return Err(
            "match_maintenance media_key must be canonical non-empty text at most 4096 bytes"
                .to_string(),
        );
    }
    if request.relocations.len() > 256 {
        return Err("match_maintenance relocations exceed the 256-root ceiling".to_string());
    }
    for (root_id, path) in &request.relocations {
        if root_id.trim().is_empty()
            || root_id.len() > 1024
            || path.trim().is_empty()
            || path.len() > 4096
        {
            return Err(
                "match_maintenance relocation IDs/paths are empty or exceed their byte ceilings"
                    .to_string(),
            );
        }
    }
    if request
        .path
        .as_ref()
        .is_some_and(|path| path.trim().is_empty() || path.len() > 4096)
    {
        return Err("match_maintenance path must be non-empty and at most 4096 bytes".to_string());
    }
    if request.expected_digest.as_ref().is_some_and(|value| {
        value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(
            "match_maintenance expected_digest must be a 64-character hex digest".to_string(),
        );
    }
    if request
        .confirmation_token
        .as_ref()
        .is_some_and(|value| value.trim().is_empty() || value.len() > 1024)
    {
        return Err("match_maintenance confirmation_token is invalid".to_string());
    }
    if request
        .conflict_policy
        .as_deref()
        .is_some_and(|policy| !matches!(policy, "reject" | "replace"))
    {
        return Err("match_maintenance conflict_policy must be reject or replace".to_string());
    }

    let needs_path = matches!(
        request.action,
        Action::IdentityExport
            | Action::IdentityImportDryRun
            | Action::IdentityImport
            | Action::IdentityImportRollback
            | Action::XmpExportPreview
            | Action::XmpExport
            | Action::XmpImportDryRun
            | Action::XmpImport
            | Action::ClearAllMatchDataPreview
            | Action::ClearAllMatchData
            | Action::RestoreRecoveryBundle
    );
    if needs_path && request.path.is_none() {
        return Err("match_maintenance action requires path".to_string());
    }
    if !needs_path && request.path.is_some() {
        return Err("path is not accepted by this match_maintenance action".to_string());
    }
    let xmp_export = matches!(request.action, Action::XmpExportPreview | Action::XmpExport);
    if xmp_export && request.media_key.is_none() {
        return Err("XMP export actions require an exact stable media_key".to_string());
    }
    if !xmp_export && request.media_key.is_some() {
        return Err("media_key is accepted only by XMP export actions".to_string());
    }
    let import_action = matches!(
        request.action,
        Action::IdentityImportDryRun
            | Action::IdentityImport
            | Action::IdentityImportRollback
            | Action::RestoreRecoveryBundle
    );
    if !import_action && !request.relocations.is_empty() {
        return Err("relocations are accepted only by identity import/restore actions".to_string());
    }
    let mutating = matches!(
        request.action,
        Action::IdentityExport
            | Action::IdentityImport
            | Action::IdentityImportRollback
            | Action::XmpExport
            | Action::XmpImport
            | Action::RebuildMatchAnalysis
            | Action::ClearAllMatchData
            | Action::RestoreRecoveryBundle
    );
    if mutating && !request.confirmed {
        return Err(
            "mutating match_maintenance actions require confirmed=true after preview".to_string(),
        );
    }
    let token_required = matches!(
        request.action,
        Action::IdentityImport
            | Action::IdentityImportRollback
            | Action::XmpExport
            | Action::XmpImport
            | Action::RebuildMatchAnalysis
            | Action::ClearAllMatchData
            | Action::RestoreRecoveryBundle
    );
    if token_required && request.confirmation_token.is_none() {
        return Err(
            "match_maintenance action requires its exact preview confirmation_token".to_string(),
        );
    }
    if request.action == Action::IdentityExport && request.expected_digest.is_none() {
        return Err("identity_export requires the exact preview expected_digest".to_string());
    }
    if request.action != Action::IdentityExport && request.expected_digest.is_some() {
        return Err("expected_digest is accepted only by identity_export".to_string());
    }
    if !token_required && request.confirmation_token.is_some() {
        return Err("confirmation_token is accepted only by token-bound apply actions".to_string());
    }
    let policy_action = matches!(
        request.action,
        Action::IdentityImportDryRun | Action::IdentityImport | Action::IdentityImportRollback
    );
    if !policy_action && request.conflict_policy.is_some() {
        return Err(
            "conflict_policy is accepted only by identity import/rollback actions".to_string(),
        );
    }
    if request.action == Action::IdentityImportRollback
        && request.conflict_policy.as_deref() != Some("replace")
    {
        return Err("identity_import_rollback requires conflict_policy=replace".to_string());
    }
    if !mutating && request.confirmed {
        return Err(
            "preview/dry-run match_maintenance actions require confirmed=false".to_string(),
        );
    }
    Ok(())
}

// The transactional correction delta envelope can safely retain at most
// 1024 selected faces (and their related rows) for restart-safe undo.
const MATCH_CORRECTION_MAX_FACE_IDS: usize = 1024;
const MATCH_CORRECTION_MAX_ID_BYTES: usize = 512;
const MATCH_CORRECTION_MAX_MEDIA_BYTES: usize = 4096;
const MATCH_CORRECTION_MAX_SOURCE_DIMENSION: u32 = 1_000_000;

fn validate_match_correction_text(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), String> {
    if value.is_empty() || value.trim() != value {
        return Err(format!(
            "{label} must be non-empty canonical text without surrounding whitespace"
        ));
    }
    if value.len() > max_bytes {
        return Err(format!("{label} exceeds {max_bytes} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} contains control characters"));
    }
    Ok(())
}

fn validate_match_correction(request: &MatchCorrectionRequest) -> Result<(), String> {
    validate_match_correction_inner(request, false)
}

fn validate_match_batch_correction_preflight(
    request: &MatchCorrectionRequest,
) -> Result<(), String> {
    validate_match_correction_inner(request, true)
}

fn match_batch_action(
    action: MatchCorrectionAction,
) -> Option<crate::match_store::BatchCorrectionAction> {
    use crate::match_store::BatchCorrectionAction as Batch;
    use MatchCorrectionAction as Action;
    Some(match action {
        Action::Same => Batch::Same,
        Action::Different => Batch::Different,
        Action::NotSure => Batch::NotSure,
        Action::ThisIsNot => Batch::ThisIsNot,
        Action::ChangePerson => Batch::ChangePerson,
        Action::RemoveAssignment => Batch::RemoveAssignment,
        Action::IgnoreFace => Batch::IgnoreFace,
        Action::NotAFace => Batch::NotAFace,
        Action::DeleteFaceAnalysis => Batch::DeleteFaceAnalysis,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchCorrectionPersonPolicy {
    Forbidden,
    Optional,
    Required,
}

fn match_correction_person_policy(action: MatchCorrectionAction) -> MatchCorrectionPersonPolicy {
    use MatchCorrectionAction as Action;
    use MatchCorrectionPersonPolicy as Policy;

    match action {
        Action::Same
        | Action::Different
        | Action::NotSure
        | Action::ThisIsNot
        | Action::ChangePerson
        | Action::RemoveAssignment
        | Action::MoveToLook
        | Action::SamePersonNewLook
        | Action::MergePeople
        | Action::SplitPerson
        | Action::RemovePerson => Policy::Required,
        Action::ManualFace => Policy::Optional,
        Action::IgnoreFace | Action::NotAFace | Action::DeleteFaceAnalysis | Action::Undo => {
            Policy::Forbidden
        }
    }
}

fn validate_match_correction_inner(
    request: &MatchCorrectionRequest,
    batch_preflight: bool,
) -> Result<(), String> {
    use MatchCorrectionAction as Action;

    if request.face_ids.len() > MATCH_CORRECTION_MAX_FACE_IDS {
        return Err(format!(
            "match_correction face_ids exceeds {} entries",
            MATCH_CORRECTION_MAX_FACE_IDS
        ));
    }
    for face_id in &request.face_ids {
        validate_match_correction_text("face_id", face_id, MATCH_CORRECTION_MAX_ID_BYTES)?;
    }
    if request.face_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("match_correction face_ids must be sorted and unique".to_string());
    }

    for (label, value) in [
        ("person_id", request.person_id.as_deref()),
        ("target_person_id", request.target_person_id.as_deref()),
        ("look_id", request.look_id.as_deref()),
        ("operation_id", request.operation_id.as_deref()),
    ] {
        if let Some(value) = value {
            validate_match_correction_text(label, value, MATCH_CORRECTION_MAX_ID_BYTES)?;
        }
    }
    if request.face_media.len() > MATCH_CORRECTION_MAX_FACE_IDS {
        return Err(format!(
            "match_correction face_media exceeds {} entries",
            MATCH_CORRECTION_MAX_FACE_IDS
        ));
    }
    for (face_id, media) in &request.face_media {
        validate_match_correction_text(
            "face_media FaceId",
            face_id,
            MATCH_CORRECTION_MAX_ID_BYTES,
        )?;
        validate_match_correction_text(
            "face_media media_key",
            &media.media_key,
            MATCH_CORRECTION_MAX_MEDIA_BYTES,
        )?;
        validate_match_correction_text(
            "face_media media_fingerprint",
            &media.media_fingerprint,
            MATCH_CORRECTION_MAX_MEDIA_BYTES,
        )?;
    }
    if let Some(look_name) = request.look_name.as_deref() {
        validate_match_correction_text("look_name", look_name, MATCH_CORRECTION_MAX_MEDIA_BYTES)?;
    }
    for (label, value) in [
        ("media_key", request.media_key.as_deref()),
        ("media_fingerprint", request.media_fingerprint.as_deref()),
    ] {
        if let Some(value) = value {
            validate_match_correction_text(label, value, MATCH_CORRECTION_MAX_MEDIA_BYTES)?;
        }
    }

    validate_match_correction_text(
        "expected_revisions.schema_generation",
        &request.expected_revisions.schema_generation,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    validate_match_correction_text(
        "expected_revisions.model_generation",
        &request.expected_revisions.model_generation,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    if request.expected_revisions.catalog_revision == 0 {
        return Err("expected_revisions.catalog_revision must be nonzero".to_string());
    }
    if request.expected_revisions.person_revisions.len() > 2 {
        return Err("expected_revisions.person_revisions exceeds two entries".to_string());
    }
    if request.expected_revisions.face_revisions.len() > MATCH_CORRECTION_MAX_FACE_IDS {
        return Err(format!(
            "expected_revisions.face_revisions exceeds {} entries",
            MATCH_CORRECTION_MAX_FACE_IDS
        ));
    }
    for (person_id, revision) in &request.expected_revisions.person_revisions {
        validate_match_correction_text(
            "expected person revision id",
            person_id,
            MATCH_CORRECTION_MAX_ID_BYTES,
        )?;
        if *revision == 0 {
            return Err("expected person revisions must be nonzero".to_string());
        }
    }
    for (face_id, revision) in &request.expected_revisions.face_revisions {
        validate_match_correction_text(
            "expected face revision id",
            face_id,
            MATCH_CORRECTION_MAX_ID_BYTES,
        )?;
        if *revision == 0 {
            return Err("expected face revisions must be nonzero".to_string());
        }
    }

    let requires_faces = matches!(
        request.action,
        Action::Same
            | Action::Different
            | Action::NotSure
            | Action::ThisIsNot
            | Action::ChangePerson
            | Action::RemoveAssignment
            | Action::IgnoreFace
            | Action::NotAFace
            | Action::DeleteFaceAnalysis
            | Action::MoveToLook
            | Action::SamePersonNewLook
            | Action::SplitPerson
    );
    if requires_faces != !request.face_ids.is_empty() {
        return Err(if requires_faces {
            "match_correction action requires one or more face_ids".to_string()
        } else {
            "match_correction action does not accept face_ids".to_string()
        });
    }

    let person_policy = match_correction_person_policy(request.action);
    if person_policy == MatchCorrectionPersonPolicy::Required && request.person_id.is_none() {
        return Err("match_correction action requires person_id".to_string());
    }
    if person_policy == MatchCorrectionPersonPolicy::Forbidden && request.person_id.is_some() {
        return Err("match_correction action does not accept person_id".to_string());
    }

    let requires_target = matches!(
        request.action,
        Action::ChangePerson | Action::MergePeople | Action::SplitPerson
    );
    if requires_target != request.target_person_id.is_some() {
        return Err(if requires_target {
            "match_correction action requires target_person_id".to_string()
        } else {
            "match_correction action does not accept target_person_id".to_string()
        });
    }
    if request.person_id.is_some()
        && request.person_id.as_deref() == request.target_person_id.as_deref()
    {
        return Err("person_id and target_person_id must differ".to_string());
    }

    let requires_look = request.action == Action::MoveToLook;
    if requires_look != request.look_id.is_some() {
        return Err(if requires_look {
            "move_to_look requires look_id".to_string()
        } else {
            "match_correction action does not accept look_id".to_string()
        });
    }
    let requires_look_name = request.action == Action::SamePersonNewLook;
    if requires_look_name != request.look_name.is_some() {
        return Err(if requires_look_name {
            "same_person_new_look requires look_name".to_string()
        } else {
            "match_correction action does not accept look_name".to_string()
        });
    }

    let face_or_manual_scope = requires_faces || request.action == Action::ManualFace;
    let has_media_scope = request.media_key.is_some() && request.media_fingerprint.is_some();
    if request.media_key.is_some() != request.media_fingerprint.is_some() {
        return Err("media_key and media_fingerprint must be supplied together".to_string());
    }
    if request.action == Action::ManualFace {
        if !has_media_scope || !request.face_media.is_empty() {
            return Err(
                "manual_face requires the single media_key/media_fingerprint scope".to_string(),
            );
        }
    } else if requires_faces {
        if request.face_ids.len() == 1 {
            let media_face_ids = request.face_media.keys().cloned().collect::<Vec<_>>();
            let compact = has_media_scope && request.face_media.is_empty();
            let mapped = !has_media_scope && media_face_ids == request.face_ids;
            if !compact && !mapped {
                return Err(
                    "single-face match_correction requires one exact compact or per-Face media fence"
                        .to_string(),
                );
            }
        } else {
            let media_face_ids = request.face_media.keys().cloned().collect::<Vec<_>>();
            if has_media_scope || media_face_ids != request.face_ids {
                return Err(
                    "batch match_correction face_media must exactly cover canonical face_ids"
                        .to_string(),
                );
            }
        }
    } else if face_or_manual_scope || has_media_scope || !request.face_media.is_empty() {
        return Err("match_correction action does not accept media scope".to_string());
    }

    if request.action == Action::ManualFace {
        let bounds = request
            .normalized_bounds
            .as_ref()
            .ok_or_else(|| "manual_face requires normalized_bounds".to_string())?;
        let orientation = request
            .exif_orientation
            .ok_or_else(|| "manual_face requires exif_orientation".to_string())?;
        if !(1..=8).contains(&orientation) {
            return Err("manual_face exif_orientation must be in 1..=8".to_string());
        }
        if !bounds.left.is_finite()
            || !bounds.top.is_finite()
            || !bounds.width.is_finite()
            || !bounds.height.is_finite()
            || bounds.left < 0.0
            || bounds.top < 0.0
            || bounds.width <= 0.0
            || bounds.height <= 0.0
            || bounds.left + bounds.width > 1.0
            || bounds.top + bounds.height > 1.0
        {
            return Err(
                "manual_face normalized_bounds must be finite, positive, and inside [0,1]"
                    .to_string(),
            );
        }
        if bounds.source_width == 0
            || bounds.source_height == 0
            || bounds.source_width > MATCH_CORRECTION_MAX_SOURCE_DIMENSION
            || bounds.source_height > MATCH_CORRECTION_MAX_SOURCE_DIMENSION
        {
            return Err(format!(
                "manual_face source dimensions must be in 1..={}",
                MATCH_CORRECTION_MAX_SOURCE_DIMENSION
            ));
        }
    } else if request.normalized_bounds.is_some() || request.exif_orientation.is_some() {
        return Err(
            "normalized_bounds and exif_orientation are accepted only by manual_face".to_string(),
        );
    }

    let batch_apply = request.face_ids.len() >= 2 && match_batch_action(request.action).is_some();
    let requires_operation = !batch_preflight
        && (batch_apply
            || matches!(
                request.action,
                Action::Undo | Action::MergePeople | Action::SplitPerson | Action::RemovePerson
            ));
    if requires_operation != request.operation_id.is_some() {
        return Err(if requires_operation {
            "undo, Person edits, and exact batch corrections require operation_id".to_string()
        } else {
            "match_correction action does not accept operation_id".to_string()
        });
    }

    let destructive = matches!(
        request.action,
        Action::Different
            | Action::ThisIsNot
            | Action::ChangePerson
            | Action::RemoveAssignment
            | Action::IgnoreFace
            | Action::NotAFace
            | Action::DeleteFaceAnalysis
            | Action::MoveToLook
            | Action::SamePersonNewLook
            | Action::MergePeople
            | Action::SplitPerson
            | Action::RemovePerson
    );
    if !batch_preflight && (destructive || request.face_ids.len() > 1) && !request.confirmed {
        return Err(
            "destructive or batch match_correction requires confirmed=true after preview"
                .to_string(),
        );
    }

    let expected_face_ids = request
        .expected_revisions
        .face_revisions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    if expected_face_ids != request.face_ids {
        return Err(
            "expected_revisions.face_revisions must exactly cover canonical face_ids".to_string(),
        );
    }
    let mut expected_person_ids = request
        .person_id
        .iter()
        .chain(request.target_person_id.iter())
        .cloned()
        .collect::<Vec<_>>();
    expected_person_ids.sort();
    expected_person_ids.dedup();
    let actual_person_ids = request
        .expected_revisions
        .person_revisions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    if actual_person_ids != expected_person_ids {
        return Err(
            "expected_revisions.person_revisions must exactly cover referenced Person IDs"
                .to_string(),
        );
    }

    if batch_preflight {
        if request.face_ids.len() < 2 || match_batch_action(request.action).is_none() {
            return Err(
                "match_batch_correction_preflight requires a supported action and at least two FaceIds"
                    .to_string(),
            );
        }
        if request.operation_id.is_some() || request.batch_preview.is_some() || request.confirmed {
            return Err(
                "match_batch_correction_preflight is preview-only: omit operation_id/batch_preview and set confirmed=false"
                    .to_string(),
            );
        }
        return Ok(());
    }

    if batch_apply {
        let preview = request
            .batch_preview
            .as_ref()
            .ok_or("multi-Face correction requires the full exact batch_preview")?;
        if request.operation_id.as_deref() != Some(preview.preview_id.as_str()) {
            return Err(
                "batch operation_id must exactly equal batch_preview.preview_id".to_string(),
            );
        }
        if Some(preview.action) != match_batch_action(request.action)
            || preview.source_person_id.as_deref() != request.person_id.as_deref()
            || preview.target_person_id.as_deref() != request.target_person_id.as_deref()
            || preview.face_ids != request.face_ids
            || preview.schema_generation != request.expected_revisions.schema_generation
            || preview.model_generation != request.expected_revisions.model_generation
            || preview.catalog_revision != request.expected_revisions.catalog_revision
        {
            return Err(
                "batch_preview action, scope, or generation fence differs from request".to_string(),
            );
        }
        if !request.confirmed {
            return Err("batch correction apply requires confirmed=true".to_string());
        }
        let preview_face_revisions = preview
            .fences
            .iter()
            .map(|fence| (fence.face_id.clone(), fence.face_revision))
            .collect::<BTreeMap<_, _>>();
        let preview_face_ids = preview
            .fences
            .iter()
            .map(|fence| fence.face_id.clone())
            .collect::<Vec<_>>();
        let preview_face_media = preview
            .fences
            .iter()
            .map(|fence| {
                (
                    fence.face_id.clone(),
                    MatchFaceMediaFence {
                        media_key: fence.media_key.clone(),
                        media_fingerprint: fence.media_fingerprint.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        if preview_face_ids != request.face_ids
            || preview_face_revisions != request.expected_revisions.face_revisions
            || preview_face_media != request.face_media
            || preview.fences.iter().any(|fence| {
                fence.person_revisions != request.expected_revisions.person_revisions
                    || fence.schema_generation != request.expected_revisions.schema_generation
                    || fence.model_generation != request.expected_revisions.model_generation
                    || fence.catalog_revision != request.expected_revisions.catalog_revision
            })
        {
            return Err(
                "batch_preview exact Face/media/Person fences differ from request".to_string(),
            );
        }
        if !preview.within_limit
            || preview.required_reversible_rows > preview.correction_delta_row_limit
        {
            return Err("batch_preview exceeds the correction delta row limit".to_string());
        }
    } else if request.batch_preview.is_some() {
        return Err(
            "batch_preview is accepted only for supported multi-Face corrections".to_string(),
        );
    }

    Ok(())
}

fn validate_match_split_person_preflight(
    request: &MatchSplitPersonPreflightRequest,
) -> Result<(), String> {
    validate_match_correction_text(
        "source_person_id",
        &request.source_person_id,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    validate_match_correction_text(
        "target_person_id",
        &request.target_person_id,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    if request.source_person_id == request.target_person_id {
        return Err("split preflight source and target Person IDs must differ".to_string());
    }
    if request.face_ids.is_empty() {
        return Err("split preflight requires one or more FaceIds".to_string());
    }
    if request.face_ids.len() > MATCH_CORRECTION_MAX_FACE_IDS {
        return Err(format!(
            "split preflight face_ids exceeds {} entries",
            MATCH_CORRECTION_MAX_FACE_IDS
        ));
    }
    for face_id in &request.face_ids {
        validate_match_correction_text("face_id", face_id, MATCH_CORRECTION_MAX_ID_BYTES)?;
    }
    if request.face_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("split preflight face_ids must be sorted and unique".to_string());
    }

    let media_face_ids = request.face_media.keys().cloned().collect::<Vec<_>>();
    if media_face_ids != request.face_ids {
        return Err("split preflight face_media must exactly cover canonical face_ids".to_string());
    }
    for (face_id, media) in &request.face_media {
        validate_match_correction_text(
            "face_media FaceId",
            face_id,
            MATCH_CORRECTION_MAX_ID_BYTES,
        )?;
        validate_match_correction_text(
            "face_media media_key",
            &media.media_key,
            MATCH_CORRECTION_MAX_MEDIA_BYTES,
        )?;
        validate_match_correction_text(
            "face_media media_fingerprint",
            &media.media_fingerprint,
            MATCH_CORRECTION_MAX_MEDIA_BYTES,
        )?;
    }

    validate_match_correction_text(
        "expected_revisions.schema_generation",
        &request.expected_revisions.schema_generation,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    validate_match_correction_text(
        "expected_revisions.model_generation",
        &request.expected_revisions.model_generation,
        MATCH_CORRECTION_MAX_ID_BYTES,
    )?;
    if request.expected_revisions.catalog_revision == 0 {
        return Err("expected_revisions.catalog_revision must be nonzero".to_string());
    }
    let expected_face_ids = request
        .expected_revisions
        .face_revisions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    if expected_face_ids != request.face_ids {
        return Err(
            "split preflight expected face revisions must exactly cover canonical face_ids"
                .to_string(),
        );
    }
    if request
        .expected_revisions
        .face_revisions
        .values()
        .any(|revision| *revision == 0)
    {
        return Err("split preflight expected face revisions must be nonzero".to_string());
    }
    let expected_person_ids = request
        .expected_revisions
        .person_revisions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut referenced_person_ids = vec![
        request.source_person_id.clone(),
        request.target_person_id.clone(),
    ];
    referenced_person_ids.sort();
    if expected_person_ids != referenced_person_ids {
        return Err(
            "split preflight expected Person revisions must exactly cover source and target"
                .to_string(),
        );
    }
    if request
        .expected_revisions
        .person_revisions
        .values()
        .any(|revision| *revision == 0)
    {
        return Err("split preflight expected Person revisions must be nonzero".to_string());
    }
    Ok(())
}

fn validate_ui_snapshot_output_syntax(output: &str) -> Result<(), String> {
    let requested = output.trim();
    if requested.is_empty() {
        return Ok(());
    }
    let candidate = Path::new(requested);
    if candidate.is_absolute() {
        return Err("ui_snapshot --out must stay inside .facial/ui-snapshots/live-ui".to_string());
    }
    let components = candidate.components().collect::<Vec<_>>();
    let valid = matches!(components.as_slice(), [std::path::Component::Normal(_)])
        || matches!(
            components.as_slice(),
            [
                std::path::Component::Normal(facial),
                std::path::Component::Normal(snapshots),
                std::path::Component::Normal(live_ui),
                std::path::Component::Normal(_)
            ] if *facial == std::ffi::OsStr::new(".facial")
                && *snapshots == std::ffi::OsStr::new("ui-snapshots")
                && *live_ui == std::ffi::OsStr::new("live-ui")
        );
    if valid {
        Ok(())
    } else {
        Err(
            "ui_snapshot --out must be a single filename in .facial/ui-snapshots/live-ui"
                .to_string(),
        )
    }
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
        match_state: service
            .match_public_snapshot()
            .unwrap_or_else(|error| serde_json::json!({ "availability": "error", "code": error })),
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
    fn wp086_appearance_pagination_keeps_person_and_track_scopes_separate() {
        let mut request: MatchVideoRequest = serde_json::from_value(serde_json::json!({
            "action":"person_appearance_list", "media_key":"media", "person_id":"person-a"
        }))
        .unwrap();
        assert!(validate_match_video(&request).is_ok());
        request.after_track_id = Some("track-cursor".into());
        assert!(validate_match_video(&request).is_err());
        request.after_track_id = None;
        request.track_id = Some("track".into());
        assert!(validate_match_video(&request).is_err());
        request.track_id = None;
        request.person_id = None;
        assert!(validate_match_video(&request).is_err());
        request.action = MatchVideoAction::AppearanceList;
        request.after_track_id = Some("track-cursor".into());
        assert!(validate_match_video(&request).is_ok());
        request.person_cursor = Some(crate::match_store::PersonAppearanceCursor {
            person_id: "person-a".into(),
            person_revision: 1,
            identity_revision: 1,
            catalog_revision: 1,
            after_assignment_id: "assignment".into(),
        });
        assert!(validate_match_video(&request).is_err());
    }

    #[test]
    fn video_appearance_intents_reject_ambiguous_or_unconfirmed_scope() {
        let mut request: MatchVideoRequest = serde_json::from_value(serde_json::json!({
            "action":"seek_appearance", "media_key":"media", "track_id":"track", "track_revision":1,
            "timestamp":{"pts":1500,"numerator":1,"denominator":1000}
        }))
        .unwrap();
        assert!(validate_match_video(&request).is_ok());
        assert!(CommandKind::MatchVideo(request.clone()).is_ui_intent());
        request.track_revision = Some(0);
        assert!(validate_match_video(&request).is_err());
        request.track_revision = Some(1);
        request.action = MatchVideoAction::CorrectionApply;
        request.correction_action = Some("assign".into());
        request.target_person_id = Some("person".into());
        assert!(validate_match_video(&request).is_err());
        request.confirmed = true;
        request.preview_token = Some("exact-preview".into());
        assert!(validate_match_video(&request).is_ok());
        request.correction_action = Some("infer".into());
        assert!(validate_match_video(&request).is_err());
        request.action = MatchVideoAction::SplitPreview;
        request.correction_action = None;
        request.target_person_id = None;
        request.confirmed = false;
        request.preview_token = None;
        request.split_observation_ids = vec!["observation".into(), "observation".into()];
        assert!(validate_match_video(&request).is_err());
        request.split_observation_ids.pop();
        assert!(validate_match_video(&request).is_ok());
        request.timestamp.as_mut().unwrap().denominator = 0;
        assert!(validate_match_video(&request).is_err());
    }

    #[test]
    fn live_ui_snapshot_is_a_receipt_backed_ui_intent() {
        let command = CommandKind::UiSnapshot {
            output: Some(".facial/ui-snapshots/live-ui/proof.png".to_string()),
            include_sensitive_match: false,
        };
        assert!(command.is_ui_intent());
        assert_eq!(command.id_str(), "ui_snapshot");
    }

    #[test]
    fn ui_snapshot_rejects_escaping_output_before_intent_publication() {
        let root = test_root("ui_snapshot_output_boundary");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        for output in [
            "../victim.png",
            "..\\victim.png",
            "nested/victim.png",
            "C:\\victim.png",
        ] {
            let cmd = command(CommandKind::UiSnapshot {
                output: Some(output.to_string()),
                include_sensitive_match: false,
            });
            let receipt = dispatch_ui_intent(&paths, &cmd);
            assert_eq!(receipt.status, ActionStatus::Rejected, "{output}");
            assert!(!paths.intent_path(&cmd.action_id).exists(), "{output}");
            assert!(!paths.receipt_path(&cmd.action_id).exists(), "{output}");
        }

        let accepted = command(CommandKind::UiSnapshot {
            output: Some(".facial/ui-snapshots/live-ui/proof.png".to_string()),
            include_sensitive_match: false,
        });
        assert_eq!(
            dispatch_ui_intent(&paths, &accepted).status,
            ActionStatus::Accepted
        );
        assert!(paths.intent_path(&accepted.action_id).is_file());
        assert!(paths.receipt_path(&accepted.action_id).is_file());
        fs::remove_dir_all(root).unwrap();
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
            settings_path_override: None,
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
            identity_manifest_path: None,
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

    fn correction_revisions(
        face_ids: &[&str],
        person_ids: &[&str],
    ) -> MatchCorrectionExpectedRevisions {
        MatchCorrectionExpectedRevisions {
            schema_generation: "match-schema-v12".to_string(),
            model_generation: "model-generation-1".to_string(),
            catalog_revision: 7,
            person_revisions: person_ids
                .iter()
                .enumerate()
                .map(|(index, id)| ((*id).to_string(), index as u64 + 11))
                .collect(),
            face_revisions: face_ids
                .iter()
                .enumerate()
                .map(|(index, id)| ((*id).to_string(), index as u64 + 21))
                .collect(),
        }
    }

    fn same_correction(face_ids: &[&str], confirmed: bool) -> MatchCorrectionRequest {
        MatchCorrectionRequest {
            action: MatchCorrectionAction::Same,
            face_ids: face_ids.iter().map(|id| (*id).to_string()).collect(),
            person_id: Some("person-1".to_string()),
            target_person_id: None,
            look_id: None,
            look_name: None,
            operation_id: None,
            batch_preview: None,
            media_key: (face_ids.len() == 1).then(|| "media-1".to_string()),
            media_fingerprint: (face_ids.len() == 1).then(|| "fingerprint-1".to_string()),
            face_media: if face_ids.len() > 1 {
                face_ids
                    .iter()
                    .map(|face_id| {
                        (
                            (*face_id).to_string(),
                            MatchFaceMediaFence {
                                media_key: format!("media-{face_id}"),
                                media_fingerprint: format!("fingerprint-{face_id}"),
                            },
                        )
                    })
                    .collect()
            } else {
                BTreeMap::new()
            },
            normalized_bounds: None,
            exif_orientation: None,
            expected_revisions: correction_revisions(face_ids, &["person-1"]),
            confirmed,
        }
    }

    fn authorize_batch(request: &mut MatchCorrectionRequest) {
        let preview_id = "batch-preview-1".to_string();
        let fences = request
            .face_ids
            .iter()
            .map(|face_id| {
                let media = &request.face_media[face_id];
                crate::match_store::CorrectionFence {
                    face_id: face_id.clone(),
                    face_revision: request.expected_revisions.face_revisions[face_id],
                    media_key: media.media_key.clone(),
                    media_fingerprint: media.media_fingerprint.clone(),
                    assignment_operation_id: None,
                    schema_generation: request.expected_revisions.schema_generation.clone(),
                    model_generation: request.expected_revisions.model_generation.clone(),
                    identity_revision: 5,
                    catalog_revision: request.expected_revisions.catalog_revision,
                    person_revisions: request.expected_revisions.person_revisions.clone(),
                }
            })
            .collect::<Vec<_>>();
        request.operation_id = Some(preview_id.clone());
        request.batch_preview = Some(crate::match_store::BatchCorrectionPreview {
            preview_id,
            action: crate::match_store::BatchCorrectionAction::Same,
            source_person_id: request.person_id.clone(),
            target_person_id: None,
            face_ids: request.face_ids.clone(),
            person_ids: vec!["person-1".to_string()],
            look_ids: Vec::new(),
            media_keys: request
                .face_media
                .values()
                .map(|media| media.media_key.clone())
                .collect(),
            affected_counts: Default::default(),
            delta_counts: Default::default(),
            required_reversible_rows: 0,
            correction_delta_row_limit: 4_096,
            within_limit: true,
            schema_generation: request.expected_revisions.schema_generation.clone(),
            model_generation: request.expected_revisions.model_generation.clone(),
            identity_revision: 5,
            catalog_revision: request.expected_revisions.catalog_revision,
            fences,
            provenance_digest: "provenance".to_string(),
            delta_digest: "delta".to_string(),
            planned_operation_id: "operation-1".to_string(),
            planned_at: "2026-08-24T00:00:00Z".to_string(),
        });
    }

    fn split_person_preflight(face_ids: &[&str]) -> MatchSplitPersonPreflightRequest {
        MatchSplitPersonPreflightRequest {
            source_person_id: "person-source".to_string(),
            target_person_id: "person-target".to_string(),
            face_ids: face_ids.iter().map(|id| (*id).to_string()).collect(),
            face_media: face_ids
                .iter()
                .map(|face_id| {
                    (
                        (*face_id).to_string(),
                        MatchFaceMediaFence {
                            media_key: format!("media-{face_id}"),
                            media_fingerprint: format!("fingerprint-{face_id}"),
                        },
                    )
                })
                .collect(),
            expected_revisions: correction_revisions(face_ids, &["person-source", "person-target"]),
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
    fn xmp_staging_result_is_addressable_in_the_terminal_applied_receipt() {
        let root = test_root("xmp-staging-terminal-receipt");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let command = command(CommandKind::MatchMaintenance(MatchMaintenanceRequest {
            action: MatchMaintenanceAction::XmpImport,
            path: Some("staged-sidecar.xmp".to_string()),
            media_key: None,
            relocations: BTreeMap::new(),
            expected_digest: None,
            confirmation_token: Some("preview-token".to_string()),
            conflict_policy: None,
            confirmed: true,
        }));
        let accepted = dispatch(&mut service, &paths, &command);
        assert_eq!(accepted.status, ActionStatus::Accepted);
        assert!(poll_pending_intent(&paths).is_some());

        let mut terminal = terminal_ui_receipt(&command, ActionStatus::Applied);
        terminal.result = serde_json::json!({
            "kind": "xmp_region_staging",
            "stage_status": "staged_only",
            "match_truth_applied": false,
            "staged_region_count": 1,
            "staging_artifact": {
                "action_id": command.action_id,
                "receipt_relative_path": format!("receipts/{}.json", command.action_id),
                "result_pointer": "/result"
            },
            "next_action_contract": {
                "kind": "xmp_region_manual_mapping",
                "regions": [{
                    "manual_face_correction": {
                        "kind": "match_correction",
                        "action": "manual_face"
                    }
                }]
            }
        });
        mark_intent_applied(&mut service, &paths, &terminal).unwrap();

        let receipt_path = paths.receipt_path(&command.action_id);
        let persisted: Receipt =
            serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
        assert_eq!(persisted.status, ActionStatus::Applied);
        assert_eq!(persisted.result["stage_status"], "staged_only");
        assert_eq!(persisted.result["match_truth_applied"], false);
        assert_eq!(
            persisted.result["staging_artifact"]["receipt_relative_path"],
            format!("receipts/{}.json", command.action_id)
        );
        assert_eq!(
            persisted.result["next_action_contract"]["regions"][0]["manual_face_correction"]
                ["action"],
            "manual_face"
        );
        let audit: Receipt = serde_json::from_str(
            &fs::read_to_string(
                paths
                    .intents_applied
                    .join(format!("{}.json", command.action_id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(audit.result, persisted.result);

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
    fn media_db_status_distinguishes_internal_schema_from_user_state() {
        let root = test_root("media-db-status");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();

        let fresh = dispatch(&mut service, &paths, &command(CommandKind::MediaDbStatus));
        assert_eq!(fresh.status, ActionStatus::Ok);
        assert_eq!(fresh.result["engine_marker"]["engine"], "surrealdb");
        assert_eq!(fresh.result["engine_marker"]["namespace"], "facial");
        assert_eq!(fresh.result["engine_marker"]["database"], "application");
        assert_eq!(fresh.result["engine_marker"]["schema_version"], 1);
        // A fresh store writes the v2 catalog plus its schema marker. The v1
        // catalog key remains classified as internal for upgraded stores, but
        // is not created on a clean baseline.
        assert_eq!(fresh.result["stats"]["settings_internal"], 2);
        assert_eq!(fresh.result["stats"]["settings_user"], 0);
        assert_eq!(
            fresh.result["stats"]["color_label_catalog_customized"],
            false
        );
        assert_eq!(fresh.result["clip_embeddings"], 0);
        assert_eq!(fresh.result["clean_user_state"], true);

        let created = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaLabelCreate {
                name: "Operator label".to_string(),
                hex: "#123ABC".to_string(),
                path: None,
            }),
        );
        assert_eq!(created.status, ActionStatus::Ok);
        let customized = dispatch(&mut service, &paths, &command(CommandKind::MediaDbStatus));
        assert_eq!(
            customized.result["stats"]["color_label_catalog_customized"],
            true
        );
        assert_eq!(customized.result["clean_user_state"], false);

        let asset = root.join("asset.jpg").to_string_lossy().to_string();
        let written = dispatch(
            &mut service,
            &paths,
            &command(CommandKind::MediaMetaSet {
                path: asset,
                notes: Some("new baseline value".to_string()),
                tags: None,
                label: None,
            }),
        );
        assert_eq!(written.status, ActionStatus::Ok);

        let populated = dispatch(&mut service, &paths, &command(CommandKind::MediaDbStatus));
        assert_eq!(populated.status, ActionStatus::Ok);
        assert_eq!(populated.result["stats"]["notes"], 1);
        assert_eq!(populated.result["clean_user_state"], false);

        crate::surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn media_db_status_rejects_a_malformed_clip_row_instead_of_reporting_zero() {
        let root = test_root("media-db-status-corrupt-clip");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let db_path = MediaDb::db_path(&root);
        let raw_db = crate::surreal_kv::Database::create(&db_path).unwrap();
        let malformed: crate::surreal_kv::TableDefinition<&str, &str> =
            crate::surreal_kv::TableDefinition::new("clip_embeddings");
        {
            let txn = raw_db.begin_write().unwrap();
            txn.open_table(malformed)
                .unwrap()
                .insert("broken.jpg", "not-hex")
                .unwrap();
            txn.commit().unwrap();
        }
        drop(raw_db);
        crate::surreal_store::wait_until_closed(&db_path).unwrap();

        let receipt = dispatch(&mut service, &paths, &command(CommandKind::MediaDbStatus));
        assert_eq!(receipt.status, ActionStatus::Error);
        assert!(
            receipt
                .error
                .as_deref()
                .is_some_and(|error| error.contains("media CLIP statistics failed")),
            "unexpected error: {:?}",
            receipt.error
        );

        crate::surreal_store::wait_until_closed(&db_path).unwrap();
        fs::remove_dir_all(&root).unwrap();
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
    fn match_intents_are_receipt_backed_and_validate_operation_fields() {
        let root = test_root("match-intents");
        let mut service = FacialService::new(test_config(&root));
        let paths = ApiPaths::from_config(service.config());
        paths.ensure_dirs().unwrap();
        let match_intent = |action: &str, id: Option<&str>, name: Option<&str>| {
            command(CommandKind::MatchIntent {
                action: action.to_string(),
                id: id.map(str::to_string),
                target_id: None,
                name: name.map(str::to_string),
                aliases: vec!["alias".to_string()],
                path: None,
                exclusions: Vec::new(),
                expected_revision: None,
                cover_media_key: None,
                hidden: None,
                favorite: None,
                offset: None,
            })
        };

        let start = match_intent("start", Some("root-1"), None);
        let accepted = dispatch(&mut service, &paths, &start);
        assert_eq!(accepted.status, ActionStatus::Accepted);
        assert_eq!(accepted.kind, "match_intent");
        assert_eq!(accepted.result["action"], "start");
        assert!(paths.intent_path(&start.action_id).is_file());

        let missing_id = match_intent("retry", None, None);
        let rejected = dispatch(&mut service, &paths, &missing_id);
        assert_eq!(rejected.status, ActionStatus::Rejected);
        assert!(!paths.intent_path(&missing_id.action_id).exists());

        let missing_name = match_intent("create_person", None, None);
        assert_eq!(
            dispatch(&mut service, &paths, &missing_name).status,
            ActionStatus::Rejected
        );
        let unknown = match_intent("launch_everything", None, None);
        assert_eq!(
            dispatch(&mut service, &paths, &unknown).status,
            ActionStatus::Rejected
        );
        let mut paged = match_intent("open_people", None, None);
        if let CommandKind::MatchIntent { offset, .. } = &mut paged.command {
            *offset = Some(10_000_000);
        }
        assert_eq!(
            dispatch(&mut service, &paths, &paged).status,
            ActionStatus::Accepted
        );
        let mut too_far = match_intent("open_people", None, None);
        if let CommandKind::MatchIntent { offset, .. } = &mut too_far.command {
            *offset = Some(10_000_001);
        }
        assert_eq!(
            dispatch(&mut service, &paths, &too_far).status,
            ActionStatus::Rejected
        );
        let mut mutation_offset = match_intent("pause_all", None, None);
        if let CommandKind::MatchIntent { offset, .. } = &mut mutation_offset.command {
            *offset = Some(1);
        }
        assert_eq!(
            dispatch(&mut service, &paths, &mutation_offset).status,
            ActionStatus::Rejected
        );
        for action in ["open_suggestions", "open_unidentified", "refresh"] {
            let mut unpaged_offset = match_intent(action, None, None);
            if let CommandKind::MatchIntent { offset, .. } = &mut unpaged_offset.command {
                *offset = Some(1);
            }
            assert_eq!(
                dispatch(&mut service, &paths, &unpaged_offset).status,
                ActionStatus::Rejected,
                "{action} must not accept an offset it does not apply"
            );
        }
        let mut settings_page = match_intent("open_settings", None, None);
        if let CommandKind::MatchIntent { offset, .. } = &mut settings_page.command {
            *offset = Some(200);
        }
        assert_eq!(
            dispatch(&mut service, &paths, &settings_page).status,
            ActionStatus::Accepted
        );
        assert_eq!(
            dispatch(
                &mut service,
                &paths,
                &match_intent("open_media_faces", Some("media/key.jpg"), None),
            )
            .status,
            ActionStatus::Accepted
        );
        assert_eq!(
            dispatch(
                &mut service,
                &paths,
                &match_intent("open_person_faces", None, None),
            )
            .status,
            ActionStatus::Rejected
        );
        let mut person_faces = match_intent("open_person_faces", Some("person-1"), None);
        if let CommandKind::MatchIntent { offset, .. } = &mut person_faces.command {
            *offset = Some(256);
        }
        assert_eq!(
            dispatch(&mut service, &paths, &person_faces).status,
            ActionStatus::Accepted
        );
        assert_eq!(
            dispatch(
                &mut service,
                &paths,
                &match_intent("person_edit_preflight", None, None),
            )
            .status,
            ActionStatus::Rejected
        );
        let mut person_edit_preflight =
            match_intent("person_edit_preflight", Some("person-source"), None);
        if let CommandKind::MatchIntent { target_id, .. } = &mut person_edit_preflight.command {
            *target_id = Some("person-target".to_string());
        }
        assert_eq!(
            dispatch(&mut service, &paths, &person_edit_preflight).status,
            ActionStatus::Accepted
        );
        let mut invalid_target = match_intent("open_person_faces", Some("person-source"), None);
        if let CommandKind::MatchIntent { target_id, .. } = &mut invalid_target.command {
            *target_id = Some("person-target".to_string());
        }
        assert_eq!(
            dispatch(&mut service, &paths, &invalid_target).status,
            ActionStatus::Rejected
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn match_correction_action_and_payload_wire_schema_are_closed() {
        let actions = [
            (MatchCorrectionAction::Same, "same"),
            (MatchCorrectionAction::Different, "different"),
            (MatchCorrectionAction::NotSure, "not_sure"),
            (MatchCorrectionAction::ThisIsNot, "this_is_not"),
            (MatchCorrectionAction::ChangePerson, "change_person"),
            (MatchCorrectionAction::RemoveAssignment, "remove_assignment"),
            (MatchCorrectionAction::IgnoreFace, "ignore_face"),
            (MatchCorrectionAction::NotAFace, "not_a_face"),
            (
                MatchCorrectionAction::DeleteFaceAnalysis,
                "delete_face_analysis",
            ),
            (MatchCorrectionAction::ManualFace, "manual_face"),
            (MatchCorrectionAction::MoveToLook, "move_to_look"),
            (
                MatchCorrectionAction::SamePersonNewLook,
                "same_person_new_look",
            ),
            (MatchCorrectionAction::MergePeople, "merge_people"),
            (MatchCorrectionAction::SplitPerson, "split_person"),
            (MatchCorrectionAction::RemovePerson, "remove_person"),
            (MatchCorrectionAction::Undo, "undo"),
        ];
        for (action, wire) in actions {
            assert_eq!(serde_json::to_value(action).unwrap(), wire);
        }
        assert!(serde_json::from_str::<MatchCorrectionAction>("\"same_person\"").is_err());

        let mut value = serde_json::to_value(command(CommandKind::MatchCorrection(
            same_correction(&["face-1"], false),
        )))
        .unwrap();
        value["ambiguous_extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Command>(value).is_err());
    }

    #[test]
    fn match_correction_person_policy_matches_service_and_store_actions() {
        use MatchCorrectionAction as Action;
        use MatchCorrectionPersonPolicy as Policy;

        for action in [
            Action::Same,
            Action::Different,
            Action::NotSure,
            Action::ThisIsNot,
            Action::ChangePerson,
            Action::RemoveAssignment,
            Action::MoveToLook,
            Action::SamePersonNewLook,
            Action::MergePeople,
            Action::SplitPerson,
            Action::RemovePerson,
        ] {
            assert_eq!(match_correction_person_policy(action), Policy::Required);
        }
        assert_eq!(
            match_correction_person_policy(Action::ManualFace),
            Policy::Optional
        );
        for action in [
            Action::IgnoreFace,
            Action::NotAFace,
            Action::DeleteFaceAnalysis,
            Action::Undo,
        ] {
            assert_eq!(match_correction_person_policy(action), Policy::Forbidden);
        }
    }

    #[test]
    fn personless_face_actions_reject_stray_person_before_accepting_single_or_batch_intents() {
        let root = test_root("match-correction-personless-actions");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        let personless_request =
            |action: MatchCorrectionAction, face_ids: &[&str], confirmed: bool| {
                let mut request = same_correction(face_ids, confirmed);
                request.action = action;
                request.person_id = None;
                request.expected_revisions.person_revisions.clear();
                request
            };
        let add_stray_person = |request: &mut MatchCorrectionRequest| {
            request.person_id = Some("person-1".to_string());
            request
                .expected_revisions
                .person_revisions
                .insert("person-1".to_string(), 11);
            if let Some(preview) = request.batch_preview.as_mut() {
                preview.source_person_id = Some("person-1".to_string());
                preview.person_ids = vec!["person-1".to_string()];
                for fence in &mut preview.fences {
                    fence.person_revisions.insert("person-1".to_string(), 11);
                }
            }
        };
        let assert_rejected_before_accept = |command: Command| {
            let intent_path = paths.intent_path(&command.action_id);
            let receipt = dispatch_ui_intent(&paths, &command);
            assert_eq!(receipt.status, ActionStatus::Rejected);
            assert!(receipt.error.as_deref().is_some_and(|error| {
                error.contains("match_correction action does not accept person_id")
            }));
            assert!(!intent_path.exists());
        };

        for action in [
            MatchCorrectionAction::IgnoreFace,
            MatchCorrectionAction::NotAFace,
            MatchCorrectionAction::DeleteFaceAnalysis,
        ] {
            let single = personless_request(action, &["face-1"], true);
            assert_eq!(
                dispatch_ui_intent(
                    &paths,
                    &command(CommandKind::MatchCorrection(single.clone())),
                )
                .status,
                ActionStatus::Accepted
            );
            let mut malformed_single = single;
            add_stray_person(&mut malformed_single);
            assert_rejected_before_accept(command(CommandKind::MatchCorrection(malformed_single)));

            let preflight = personless_request(action, &["face-1", "face-2"], false);
            assert_eq!(
                dispatch_ui_intent(
                    &paths,
                    &command(CommandKind::MatchBatchCorrectionPreflight(
                        preflight.clone(),
                    )),
                )
                .status,
                ActionStatus::Accepted
            );
            let mut malformed_preflight = preflight;
            add_stray_person(&mut malformed_preflight);
            assert_rejected_before_accept(command(CommandKind::MatchBatchCorrectionPreflight(
                malformed_preflight,
            )));

            let mut batch = personless_request(action, &["face-1", "face-2"], true);
            authorize_batch(&mut batch);
            let preview = batch.batch_preview.as_mut().unwrap();
            preview.action = match_batch_action(action).unwrap();
            preview.person_ids.clear();
            assert_eq!(
                dispatch_ui_intent(
                    &paths,
                    &command(CommandKind::MatchCorrection(batch.clone())),
                )
                .status,
                ActionStatus::Accepted
            );
            let mut malformed_batch = batch;
            add_stray_person(&mut malformed_batch);
            assert_rejected_before_accept(command(CommandKind::MatchCorrection(malformed_batch)));
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_correction_is_a_stably_named_receipt_backed_ui_intent() {
        let root = test_root("match-correction-intent");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();
        let cmd = command(CommandKind::MatchCorrection(same_correction(
            &["face-1"],
            false,
        )));

        assert!(cmd.command.is_ui_intent());
        assert_eq!(cmd.command.id_str(), "match_correction");
        let receipt = dispatch_ui_intent(&paths, &cmd);
        assert_eq!(receipt.status, ActionStatus::Accepted);
        assert_eq!(receipt.kind, "match_correction");
        assert_eq!(receipt.result["kind"], "match_correction");
        assert_eq!(receipt.result["action"], "same");
        assert!(paths.intent_path(&cmd.action_id).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn split_person_preflight_is_closed_fenced_and_receipt_backed() {
        let root = test_root("match-split-person-preflight");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        let cmd = command(CommandKind::MatchSplitPersonPreflight(
            split_person_preflight(&["face-1", "face-2"]),
        ));
        assert!(cmd.command.is_ui_intent());
        assert_eq!(cmd.command.id_str(), "match_split_person_preflight");
        let receipt = dispatch_ui_intent(&paths, &cmd);
        assert_eq!(receipt.status, ActionStatus::Accepted);
        assert_eq!(receipt.result["kind"], "match_split_person_preflight");
        assert!(paths.intent_path(&cmd.action_id).is_file());

        let mut unsorted = split_person_preflight(&["face-2", "face-1"]);
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchSplitPersonPreflight(unsorted.clone())),
            )
            .status,
            ActionStatus::Rejected
        );
        unsorted.face_ids.sort();
        unsorted.face_media.remove("face-1");
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchSplitPersonPreflight(unsorted)),
            )
            .status,
            ActionStatus::Rejected
        );

        let mut missing_face_revision = split_person_preflight(&["face-1"]);
        missing_face_revision
            .expected_revisions
            .face_revisions
            .clear();
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchSplitPersonPreflight(
                    missing_face_revision,
                )),
            )
            .status,
            ActionStatus::Rejected
        );

        let mut same_person = split_person_preflight(&["face-1"]);
        same_person.target_person_id = same_person.source_person_id.clone();
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchSplitPersonPreflight(same_person)),
            )
            .status,
            ActionStatus::Rejected
        );

        let mut value = serde_json::to_value(command(CommandKind::MatchSplitPersonPreflight(
            split_person_preflight(&["face-1"]),
        )))
        .unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Command>(value).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_correction_rejects_noncanonical_or_unfenced_face_sets() {
        let root = test_root("match-correction-face-fences");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        let unsorted = command(CommandKind::MatchCorrection(same_correction(
            &["face-2", "face-1"],
            true,
        )));
        let receipt = dispatch_ui_intent(&paths, &unsorted);
        assert_eq!(receipt.status, ActionStatus::Rejected);
        assert!(receipt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("sorted and unique")));

        let duplicate = command(CommandKind::MatchCorrection(same_correction(
            &["face-1", "face-1"],
            true,
        )));
        assert_eq!(
            dispatch_ui_intent(&paths, &duplicate).status,
            ActionStatus::Rejected
        );

        let mut missing_fence = same_correction(&["face-1"], false);
        missing_fence.expected_revisions.face_revisions.clear();
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(missing_fence)),
            )
            .status,
            ActionStatus::Rejected
        );

        let mut batch_without_confirmation = same_correction(&["face-1", "face-2"], false);
        authorize_batch(&mut batch_without_confirmation);
        let batch_without_confirmation =
            command(CommandKind::MatchCorrection(batch_without_confirmation));
        let receipt = dispatch_ui_intent(&paths, &batch_without_confirmation);
        assert_eq!(receipt.status, ActionStatus::Rejected);
        assert!(receipt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("confirmed=true")));

        let preflight = same_correction(&["face-1", "face-2"], false);
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchBatchCorrectionPreflight(preflight)),
            )
            .status,
            ActionStatus::Accepted
        );
        let mut batch = same_correction(&["face-1", "face-2"], true);
        authorize_batch(&mut batch);
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(batch.clone())),
            )
            .status,
            ActionStatus::Accepted
        );
        let mut missing_media_fence = batch;
        missing_media_fence.face_media.remove("face-2");
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(missing_media_fence)),
            )
            .status,
            ActionStatus::Rejected
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn destructive_merge_and_undo_enforce_confirmation_and_exact_fields() {
        let root = test_root("match-correction-destructive");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        let merge = |confirmed| MatchCorrectionRequest {
            action: MatchCorrectionAction::MergePeople,
            face_ids: Vec::new(),
            person_id: Some("person-source".to_string()),
            target_person_id: Some("person-target".to_string()),
            look_id: None,
            look_name: None,
            operation_id: Some("merge-operation-1".to_string()),
            batch_preview: None,
            media_key: None,
            media_fingerprint: None,
            face_media: BTreeMap::new(),
            normalized_bounds: None,
            exif_orientation: None,
            expected_revisions: correction_revisions(&[], &["person-source", "person-target"]),
            confirmed,
        };
        assert_eq!(
            dispatch_ui_intent(&paths, &command(CommandKind::MatchCorrection(merge(false))),)
                .status,
            ActionStatus::Rejected
        );
        assert_eq!(
            dispatch_ui_intent(&paths, &command(CommandKind::MatchCorrection(merge(true))),).status,
            ActionStatus::Accepted
        );

        // A receipt-backed split is a confirmed preview execution, not an
        // unpreviewed batch correction. It therefore needs both the exact
        // Person-operation preview token and the selected Face/media/revision
        // fences. This is the wire shape consumed by the service's
        // verify_person_preview_fence path.
        let split = MatchCorrectionRequest {
            action: MatchCorrectionAction::SplitPerson,
            face_ids: vec!["face-split".to_string()],
            person_id: Some("person-source".to_string()),
            target_person_id: Some("person-target".to_string()),
            look_id: None,
            look_name: None,
            operation_id: Some("split-preview-token".to_string()),
            batch_preview: None,
            media_key: Some("media-split".to_string()),
            media_fingerprint: Some("fingerprint-split".to_string()),
            face_media: BTreeMap::new(),
            normalized_bounds: None,
            exif_orientation: None,
            expected_revisions: correction_revisions(
                &["face-split"],
                &["person-source", "person-target"],
            ),
            confirmed: true,
        };
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(split.clone())),
            )
            .status,
            ActionStatus::Accepted
        );
        let mut split_without_preview = split.clone();
        split_without_preview.operation_id = None;
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(split_without_preview)),
            )
            .status,
            ActionStatus::Rejected
        );
        let mut split_without_media_fence = split;
        split_without_media_fence.media_fingerprint = None;
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(split_without_media_fence)),
            )
            .status,
            ActionStatus::Rejected
        );

        let undo = MatchCorrectionRequest {
            action: MatchCorrectionAction::Undo,
            face_ids: Vec::new(),
            person_id: None,
            target_person_id: None,
            look_id: None,
            look_name: None,
            operation_id: None,
            batch_preview: None,
            media_key: None,
            media_fingerprint: None,
            face_media: BTreeMap::new(),
            normalized_bounds: None,
            exif_orientation: None,
            expected_revisions: correction_revisions(&[], &[]),
            confirmed: false,
        };
        let receipt = dispatch_ui_intent(&paths, &command(CommandKind::MatchCorrection(undo)));
        assert_eq!(receipt.status, ActionStatus::Rejected);
        assert!(receipt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("require operation_id")));

        let mut stray_operation = same_correction(&["face-1"], false);
        stray_operation.operation_id = Some("not-allowed".to_string());
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(stray_operation)),
            )
            .status,
            ActionStatus::Rejected
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_correction_bounds_strings_counts_and_action_fields_are_bounded() {
        let root = test_root("match-correction-bounds");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();

        let reject = |request| {
            dispatch_ui_intent(&paths, &command(CommandKind::MatchCorrection(request))).status
        };

        let mut whitespace_id = same_correction(&["face-1"], false);
        whitespace_id.person_id = Some(" person-1".to_string());
        assert_eq!(reject(whitespace_id), ActionStatus::Rejected);

        let mut oversized_id = same_correction(&["face-1"], false);
        oversized_id.operation_id = Some("x".repeat(MATCH_CORRECTION_MAX_ID_BYTES + 1));
        assert_eq!(reject(oversized_id), ActionStatus::Rejected);

        let mut missing_person = same_correction(&["face-1"], false);
        missing_person.person_id = None;
        missing_person.expected_revisions.person_revisions.clear();
        assert_eq!(reject(missing_person), ActionStatus::Rejected);

        let mut missing_media = same_correction(&["face-1"], false);
        missing_media.media_key = None;
        assert_eq!(reject(missing_media), ActionStatus::Rejected);

        let mut ambiguous_look = same_correction(&["face-1"], false);
        ambiguous_look.look_id = Some("look-1".to_string());
        assert_eq!(reject(ambiguous_look), ActionStatus::Rejected);

        let mut new_look = same_correction(&["face-1"], true);
        new_look.action = MatchCorrectionAction::SamePersonNewLook;
        new_look.look_name = Some("Profile".to_string());
        assert_eq!(reject(new_look), ActionStatus::Accepted);

        let mut zero_revision = same_correction(&["face-1"], false);
        zero_revision.expected_revisions.catalog_revision = 0;
        assert_eq!(reject(zero_revision), ActionStatus::Rejected);

        let face_ids = (0..=MATCH_CORRECTION_MAX_FACE_IDS)
            .map(|index| format!("face-{index:05}"))
            .collect::<Vec<_>>();
        let face_refs = face_ids.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            reject(same_correction(&face_refs, true)),
            ActionStatus::Rejected
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_face_requires_bounded_orientation_safe_geometry() {
        let root = test_root("match-correction-manual-face");
        let paths = ApiPaths::from_config(&test_config(&root));
        paths.ensure_dirs().unwrap();
        let manual = |left: f32, orientation: u8| MatchCorrectionRequest {
            action: MatchCorrectionAction::ManualFace,
            face_ids: Vec::new(),
            person_id: Some("person-1".to_string()),
            target_person_id: None,
            look_id: None,
            look_name: None,
            operation_id: None,
            batch_preview: None,
            media_key: Some("media-1".to_string()),
            media_fingerprint: Some("fingerprint-1".to_string()),
            face_media: BTreeMap::new(),
            normalized_bounds: Some(MatchNormalizedFaceBounds {
                left,
                top: 0.2,
                width: 0.4,
                height: 0.5,
                source_width: 4032,
                source_height: 3024,
            }),
            exif_orientation: Some(orientation),
            expected_revisions: correction_revisions(&[], &["person-1"]),
            confirmed: false,
        };

        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchCorrection(manual(0.1, 6))),
            )
            .status,
            ActionStatus::Accepted
        );
        for invalid in [manual(0.7, 6), manual(f32::NAN, 6), manual(0.1, 0)] {
            assert_eq!(
                dispatch_ui_intent(&paths, &command(CommandKind::MatchCorrection(invalid)),).status,
                ActionStatus::Rejected
            );
        }

        fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn match_maintenance_is_typed_preview_bound_and_rejects_overloaded_fields() {
        let root = test_root("match_maintenance_validation");
        let cfg = test_config(&root);
        let paths = ApiPaths::from_config(&cfg);
        paths.ensure_dirs().unwrap();
        let request = |action| MatchMaintenanceRequest {
            action,
            path: None,
            media_key: None,
            relocations: BTreeMap::new(),
            expected_digest: None,
            confirmation_token: None,
            conflict_policy: None,
            confirmed: false,
        };

        let preview = command(CommandKind::MatchMaintenance(request(
            MatchMaintenanceAction::IdentityExportPreview,
        )));
        assert_eq!(
            dispatch_ui_intent(&paths, &preview).status,
            ActionStatus::Accepted
        );

        let mut export = request(MatchMaintenanceAction::IdentityExport);
        export.path = Some("identity.facial-identity.json".to_string());
        export.confirmed = true;
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchMaintenance(export.clone()))
            )
            .status,
            ActionStatus::Rejected
        );
        export.expected_digest = Some("a".repeat(64));
        assert_eq!(
            dispatch_ui_intent(&paths, &command(CommandKind::MatchMaintenance(export))).status,
            ActionStatus::Accepted
        );

        let mut clear = request(MatchMaintenanceAction::ClearAllMatchData);
        clear.path = Some("recovery.facial-identity.json".to_string());
        clear.confirmed = true;
        assert_eq!(
            dispatch_ui_intent(
                &paths,
                &command(CommandKind::MatchMaintenance(clear.clone()))
            )
            .status,
            ActionStatus::Rejected
        );
        clear.confirmation_token = Some("state-bound-token".to_string());
        assert_eq!(
            dispatch_ui_intent(&paths, &command(CommandKind::MatchMaintenance(clear))).status,
            ActionStatus::Accepted
        );

        let mut invalid = request(MatchMaintenanceAction::RebuildMatchAnalysisPreview);
        invalid.media_key = Some("not-valid-for-rebuild".to_string());
        assert_eq!(
            dispatch_ui_intent(&paths, &command(CommandKind::MatchMaintenance(invalid))).status,
            ActionStatus::Rejected
        );
    }
}
