//! Durable, bounded Match correction primitives (WP-084).
//!
//! This child module deliberately owns the correction vocabulary and typed
//! operation-delta format.  It does not expose embeddings, crops, database
//! handles, or filesystem mutation through the Viewer snapshot.

use super::*;
use std::path::PathBuf;
mod video_corrections;
pub use video_corrections::*;

pub(super) const FACE_DISPOSITION_TABLE: &str = "match_face_disposition";
pub(super) const CORRECTION_MEDIA_OPERATION_TABLE: &str = "match_correction_media_operation";
pub(super) const SUGGESTION_SOURCE_PROVENANCE_TABLE: &str = "match_suggestion_source_provenance";
const VIEWER_FACE_LIMIT: usize = 4096;
const VIEWER_SUGGESTION_SCAN_LIMIT: usize = 16384;
const VIEWER_PERSON_LIMIT: usize = 8192;
const VIEWER_UNDO_SCAN_LIMIT: usize = 256;
const VIEWER_UNDO_LIMIT: usize = 32;
const CORRECTION_ROW_LIMIT: usize = 4096;
// Reserve half of the exchange nested-string budget for restart-safe undo
// rematerialization (new undo operation IDs and revision metadata). A
// self-produced correction is therefore guaranteed not to consume the entire
// import/recovery allowance before its inverse is materialized.
const CORRECTION_OPERATION_JSON_MAX_BYTES: usize =
    super::exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES / 2;
/// Preview may cross the 4096 reversible-row boundary so the UI can show an
/// exact over-cap verdict instead of failing before it has counts. Durable
/// apply remains governed solely by `CORRECTION_ROW_LIMIT`.
const BATCH_CORRECTION_FACE_LIMIT: usize = CORRECTION_ROW_LIMIT * 2;
const PERSON_INVENTORY_DIGEST_PAGE_LIMIT: usize = 512;
const CORRECTION_DELTA_VERSION: u32 = 1;
const UNCONFIGURED_MODEL_GENERATION: &str = "match-model-unconfigured";

/// Parent integration: append this SQL to `MATCH_SCHEMA_SQL` and execute it in
/// the WP-084 schema migration before opening this module's write APIs.
pub const WP084_CORRECTIONS_SCHEMA_SQL: &str = r#"
DEFINE TABLE OVERWRITE match_face_disposition SCHEMAFULL;
DEFINE FIELD OVERWRITE face_id ON TABLE match_face_disposition TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_face_disposition TYPE string;
DEFINE FIELD OVERWRITE disposition ON TABLE match_face_disposition TYPE string ASSERT $value IN ['ignored', 'not_a_face'];
DEFINE FIELD OVERWRITE operation_id ON TABLE match_face_disposition TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_face_disposition TYPE int;
DEFINE FIELD OVERWRITE created_at ON TABLE match_face_disposition TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_face_disposition TYPE string;
DEFINE INDEX OVERWRITE match_face_disposition_face ON TABLE match_face_disposition FIELDS face_id UNIQUE;
DEFINE INDEX OVERWRITE match_face_disposition_media ON TABLE match_face_disposition FIELDS media_key, face_id;
DEFINE TABLE OVERWRITE match_correction_media_operation SCHEMAFULL;
DEFINE FIELD OVERWRITE mapping_id ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE kind ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_correction_media_operation TYPE string;
DEFINE INDEX OVERWRITE match_correction_media_operation_id ON TABLE match_correction_media_operation FIELDS mapping_id UNIQUE;
DEFINE INDEX OVERWRITE match_correction_media_operation_media ON TABLE match_correction_media_operation FIELDS media_key, created_at, operation_id;
DEFINE TABLE OVERWRITE match_suggestion_source_provenance SCHEMAFULL;
DEFINE FIELD OVERWRITE provenance_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE operation_kind ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE suggestion_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE candidate_person_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_suggestion_source_provenance TYPE int;
DEFINE FIELD OVERWRITE person_revision ON TABLE match_suggestion_source_provenance TYPE int;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE job_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE suggestion_created_at ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE similarity_bits ON TABLE match_suggestion_source_provenance TYPE int;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_suggestion_source_provenance TYPE option<string>;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_suggestion_source_provenance TYPE option<string>;
DEFINE FIELD OVERWRITE embedding_id ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE embedding_created_at ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE FIELD OVERWRITE operation_created_at ON TABLE match_suggestion_source_provenance TYPE string;
DEFINE INDEX OVERWRITE match_suggestion_source_provenance_id ON TABLE match_suggestion_source_provenance FIELDS provenance_id UNIQUE;
DEFINE INDEX OVERWRITE match_suggestion_source_provenance_operation ON TABLE match_suggestion_source_provenance FIELDS operation_id, suggestion_id UNIQUE;
DEFINE INDEX OVERWRITE match_suggestion_source_provenance_media ON TABLE match_suggestion_source_provenance FIELDS media_key, operation_id, suggestion_id;
"#;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FaceDispositionKind {
    Ignored,
    NotAFace,
}

impl FaceDispositionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::NotAFace => "not_a_face",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct FaceDisposition {
    pub face_id: String,
    pub media_key: String,
    pub disposition: String,
    pub operation_id: String,
    pub face_revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ViewerFaceRow {
    pub face: FaceObservation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment: Option<Assignment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<Person>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Suggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_person: Option<Person>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<FaceDisposition>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ViewerFaceSnapshot {
    pub media_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_fingerprint: Option<String>,
    pub configured: bool,
    pub schema_generation: String,
    pub model_generation: String,
    pub catalog_revision: u64,
    pub total_faces: usize,
    pub rows: Vec<ViewerFaceRow>,
    pub looks: Vec<Look>,
    /// Recent reversible correction operations touching this canonical media
    /// asset and not already undone. This is durable discovery state, not a
    /// projection of the current GUI session.
    pub undo_candidates: Vec<CorrectionUndoCandidate>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CorrectionUndoCandidate {
    pub operation_id: String,
    pub kind: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub(super) struct CorrectionMediaOperation {
    pub(super) mapping_id: String,
    pub(super) media_key: String,
    pub(super) media_fingerprint: String,
    pub(super) operation_id: String,
    pub(super) kind: String,
    pub(super) created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuggestionSourceProvenance {
    pub provenance_id: String,
    pub operation_id: String,
    pub operation_kind: String,
    pub suggestion_id: String,
    pub face_id: String,
    pub candidate_person_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub face_revision: u64,
    pub person_revision: u64,
    pub model_generation: String,
    pub job_id: String,
    pub suggestion_created_at: String,
    pub similarity_bits: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_hash: Option<String>,
    pub embedding_id: String,
    pub embedding_created_at: String,
    pub schema_generation: String,
    pub operation_created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualMediaAuthority {
    pub source_path: PathBuf,
    pub root_path: PathBuf,
    pub media_key: String,
    pub media_fingerprint: String,
    pub fence: ManualMediaAuthorityFence,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualMediaAuthorityFence {
    pub asset_id: String,
    pub job_id: String,
    pub source_path: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonFacePageRow {
    pub face_id: String,
    pub face_revision: u64,
    pub media_key: String,
    pub media_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub look_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonFacePage {
    pub person: Person,
    pub schema_generation: String,
    pub model_generation: String,
    pub catalog_revision: u64,
    pub identity_revision: u64,
    /// Exact source-Person inventory token required by merge/remove. It
    /// changes whenever any identity mutation could invalidate the preview.
    pub preview_token: String,
    pub total_faces: usize,
    pub total_media: usize,
    pub total_looks: usize,
    pub offset: usize,
    pub limit: usize,
    pub rows: Vec<PersonFacePageRow>,
    pub looks: Vec<Look>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CorrectionFence {
    pub face_id: String,
    pub face_revision: u64,
    pub media_key: String,
    pub media_fingerprint: String,
    pub assignment_operation_id: Option<String>,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    #[serde(default)]
    pub person_revisions: BTreeMap<String, u64>,
}

/// Closed batch-correction vocabulary. A preview is bound to exactly one
/// action; callers cannot reuse a low-cost preview to authorize a more
/// destructive action over the same FaceIds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchCorrectionAction {
    Same,
    Different,
    NotSure,
    ThisIsNot,
    ChangePerson,
    RemoveAssignment,
    IgnoreFace,
    NotAFace,
    DeleteFaceAnalysis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RejectionTrigger {
    Different,
    ThisIsNot,
}

impl RejectionTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::Different => "Different",
            Self::ThisIsNot => "This-is-not",
        }
    }
}

impl BatchCorrectionAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Same => "batch_same",
            Self::Different => "batch_different",
            Self::NotSure => "batch_not_sure",
            Self::ThisIsNot => "batch_this_is_not",
            Self::ChangePerson => "batch_change_person",
            Self::RemoveAssignment => "batch_remove_assignments",
            Self::IgnoreFace => "batch_ignore_face",
            Self::NotAFace => "batch_not_a_face",
            Self::DeleteFaceAnalysis => "batch_delete_face_analysis",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BatchCorrectionDeltaCounts {
    pub persons: usize,
    pub looks: usize,
    pub template_sets: usize,
    pub faces: usize,
    pub embeddings: usize,
    pub assignments: usize,
    pub constraints: usize,
    pub trusted_members: usize,
    pub trusted_search: usize,
    pub dispositions: usize,
    pub suggestions: usize,
}

impl BatchCorrectionDeltaCounts {
    fn checked_total(&self) -> Result<usize, String> {
        [
            self.persons,
            self.looks,
            self.template_sets,
            self.faces,
            self.embeddings,
            self.assignments,
            self.constraints,
            self.trusted_members,
            self.trusted_search,
            self.dispositions,
            self.suggestions,
        ]
        .into_iter()
        .try_fold(0usize, |total, count| {
            total
                .checked_add(count)
                .ok_or_else(|| "batch correction reversible-row count overflow".to_string())
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BatchCorrectionAffectedCounts {
    pub persons: usize,
    pub looks: usize,
    pub faces: usize,
    pub media: usize,
}

/// Exact, action-specific authorization token for one batch correction.
/// The full current fence inventory is retained so apply can recompute the
/// same plan under the correction write lock and compare it byte-for-byte.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BatchCorrectionPreview {
    pub preview_id: String,
    pub action: BatchCorrectionAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_person_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_person_id: Option<String>,
    pub face_ids: Vec<String>,
    pub person_ids: Vec<String>,
    pub look_ids: Vec<String>,
    pub media_keys: Vec<String>,
    pub affected_counts: BatchCorrectionAffectedCounts,
    pub delta_counts: BatchCorrectionDeltaCounts,
    pub required_reversible_rows: usize,
    pub correction_delta_row_limit: usize,
    pub within_limit: bool,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub fences: Vec<CorrectionFence>,
    /// Digest of the complete current fences and typed reversible rows used
    /// by the action-specific planner.
    pub provenance_digest: String,
    pub delta_digest: String,
    /// Stable values generated at preflight and reused during apply so the
    /// planned row images do not drift merely because time advanced.
    pub planned_operation_id: String,
    pub planned_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExifOrientation {
    Normal,
    MirrorHorizontal,
    Rotate180,
    MirrorVertical,
    Transpose,
    Rotate90Clockwise,
    Transverse,
    Rotate90CounterClockwise,
}

impl ExifOrientation {
    fn code(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::MirrorHorizontal => 2,
            Self::Rotate180 => 3,
            Self::MirrorVertical => 4,
            Self::Transpose => 5,
            Self::Rotate90Clockwise => 6,
            Self::Transverse => 7,
            Self::Rotate90CounterClockwise => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl NormalizedRegion {
    fn validate(self) -> Result<Self, String> {
        let values = [self.x, self.y, self.width, self.height];
        if values.iter().any(|value| !value.is_finite())
            || self.x < 0.0
            || self.y < 0.0
            || self.width <= 0.0
            || self.height <= 0.0
            || self.x + self.width > 1.0
            || self.y + self.height > 1.0
        {
            return Err(
                "manual face region must be a positive finite normalized rectangle".to_string(),
            );
        }
        Ok(self)
    }

    pub fn display_to_source(self, orientation: ExifOrientation) -> Result<Self, String> {
        let region = self.validate()?;
        let corners = [
            (region.x, region.y),
            (region.x + region.width, region.y),
            (region.x, region.y + region.height),
            (region.x + region.width, region.y + region.height),
        ];
        let mapped = corners.map(|(x, y)| display_point_to_source(x, y, orientation));
        let min_x = mapped.iter().map(|point| point.0).fold(1.0_f32, f32::min);
        let max_x = mapped.iter().map(|point| point.0).fold(0.0_f32, f32::max);
        let min_y = mapped.iter().map(|point| point.1).fold(1.0_f32, f32::min);
        let max_y = mapped.iter().map(|point| point.1).fold(0.0_f32, f32::max);
        Self {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
        .validate()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManualEmbeddingInput {
    pub vector: Vec<f32>,
    pub model_generation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManualFaceInput {
    pub expected_schema_generation: String,
    pub expected_model_generation: String,
    pub expected_catalog_revision: u64,
    pub media_key: String,
    pub media_fingerprint: String,
    pub authority: ManualMediaAuthorityFence,
    pub source_width: u32,
    pub source_height: u32,
    pub display_region: NormalizedRegion,
    pub exif_orientation: ExifOrientation,
    #[serde(default)]
    pub display_landmarks: Vec<Vec<f32>>,
    pub alignment_valid: bool,
    pub quality: f32,
    pub pose_bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<ManualEmbeddingInput>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ManualFaceReceipt {
    pub operation: CorrectionReceipt,
    pub face: FaceObservation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersonEditKind {
    Merge {
        source_person_id: String,
        target_person_id: String,
    },
    Split {
        source_person_id: String,
        destination_person_id: String,
        destination_name: String,
    },
    SplitToPerson {
        source_person_id: String,
        target_person_id: String,
    },
    Remove {
        person_id: String,
    },
}

/// Exact row kinds that a Person edit must record for restart-safe undo.
/// Candidate suggestions are included because merge/remove deletes them;
/// derived projections that can be rebuilt from canonical rows stay outside
/// the reversible correction envelope.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonEditDeltaCounts {
    pub persons: usize,
    pub assignments: usize,
    pub looks: usize,
    pub template_sets: usize,
    pub trusted_members: usize,
    pub trusted_search: usize,
    pub constraints: usize,
    pub suggestions: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonEditAffectedCounts {
    pub persons: usize,
    pub faces: usize,
    pub media: usize,
    pub looks: usize,
}

impl PersonEditDeltaCounts {
    fn checked_total(&self) -> Result<usize, String> {
        [
            self.persons,
            self.assignments,
            self.looks,
            self.template_sets,
            self.trusted_members,
            self.trusted_search,
            self.constraints,
            self.suggestions,
        ]
        .into_iter()
        .try_fold(0usize, |total, count| {
            total
                .checked_add(count)
                .ok_or_else(|| "Person edit reversible-row count overflow".to_string())
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PersonEditPreview {
    pub preview_id: String,
    pub kind: PersonEditKind,
    pub person_ids: Vec<String>,
    pub face_ids: Vec<String>,
    pub media_keys: Vec<String>,
    pub look_ids: Vec<String>,
    pub assignment_count: usize,
    pub delta_counts: PersonEditDeltaCounts,
    pub required_reversible_rows: usize,
    /// Exact affected-entity counts remain available when a whole-Person
    /// operation exceeds the reversible-row limit and its ID inventories are
    /// therefore intentionally omitted from the bounded preview payload.
    pub affected_counts: PersonEditAffectedCounts,
    pub inventory_complete: bool,
    /// Action-specific digest over the complete streamed canonical inventory,
    /// including rows omitted from an over-cap preview payload.
    pub inventory_digest: String,
    /// Global atomic/restart-safe delta cap. This is not an assignment-page
    /// cap: preview inventories stream to completion, and execution checks the
    /// sum of every reversible row kind against this value.
    pub correction_delta_row_limit: usize,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub person_revisions: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CorrectionReceipt {
    pub operation_id: String,
    pub kind: String,
    pub changed_rows: usize,
    pub conflict_rows: usize,
    pub affected_face_ids: Vec<String>,
    pub affected_media_keys: Vec<String>,
    pub identity_revision: u64,
    pub catalog_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionTable {
    VideoObservation,
    Person,
    Look,
    TemplateSet,
    Face,
    Embedding,
    Assignment,
    Constraint,
    TrustedMember,
    TrustedSearch,
    Disposition,
    Suggestion,
}

impl CorrectionTable {
    fn name(&self) -> &'static str {
        match self {
            Self::VideoObservation => super::video::VIDEO_OBSERVATION_TABLE,
            Self::Person => PERSON_TABLE,
            Self::Look => LOOK_TABLE,
            Self::TemplateSet => TEMPLATE_SET_TABLE,
            Self::Face => FACE_TABLE,
            Self::Embedding => EMBEDDING_TABLE,
            Self::Assignment => ASSIGNMENT_TABLE,
            Self::Constraint => CONSTRAINT_TABLE,
            Self::TrustedMember => TRUSTED_MEMBER_TABLE,
            Self::TrustedSearch => TRUSTED_SEARCH_TABLE,
            Self::Disposition => FACE_DISPOSITION_TABLE,
            Self::Suggestion => SUGGESTION_TABLE,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CorrectionRowDelta {
    pub table: CorrectionTable,
    pub stable_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

struct BatchCorrectionPlan {
    preview: BatchCorrectionPreview,
    rows: Vec<CorrectionRowDelta>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct CorrectionDeltaEnvelope {
    pub(super) version: u32,
    pub(super) kind: String,
    pub(super) rows: Vec<CorrectionRowDelta>,
    pub(super) face_ids: Vec<String>,
    pub(super) media_keys: Vec<String>,
    pub(super) identity_changed: bool,
    pub(super) catalog_changed: bool,
}

impl MatchStore {
    /// Resolve a manual-face request through the canonical latest JobAsset,
    /// its owning IndexJob, and its enabled configured root. Caller-provided
    /// paths never enter this authority chain.
    pub fn manual_media_authority(
        &self,
        media_key: &str,
        media_fingerprint: &str,
    ) -> Result<ManualMediaAuthority, String> {
        validate_media_key(media_key)?;
        validate_text("media fingerprint", media_fingerprint)?;
        if media_fingerprint.starts_with("unavailable:") {
            return Err("manual face requires a current readable media fingerprint".to_string());
        }
        let _guard = self.database_read_guard("manual media authority lock is poisoned")?;
        let asset = self
            .canonical_job_asset_for_media_unlocked(media_key)?
            .ok_or("manual face media has no canonical JobAsset")?;
        if asset.media_fingerprint != media_fingerprint {
            return Err("stale manual face media fingerprint".to_string());
        }
        let source_path = asset
            .source_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .ok_or("manual face JobAsset has no canonical source path")?;
        let job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        let root: MatchIndexRoot =
            self.require_unlocked(ROOT_CONFIG_TABLE, &job.root_key, "MatchIndexRoot")?;
        if !root.enabled {
            return Err("manual face canonical index root is disabled".to_string());
        }
        let canonical_source_path = source_path.to_string_lossy().to_string();
        Ok(ManualMediaAuthority {
            source_path,
            root_path: PathBuf::from(root.path),
            media_key: asset.media_key,
            media_fingerprint: asset.media_fingerprint,
            fence: ManualMediaAuthorityFence {
                asset_id: asset.asset_id,
                job_id: asset.job_id,
                source_path: canonical_source_path,
            },
        })
    }

    fn validate_manual_media_authority_fence_unlocked(
        &self,
        media_key: &str,
        media_fingerprint: &str,
        fence: &ManualMediaAuthorityFence,
    ) -> Result<(), String> {
        validate_text("manual media authority asset", &fence.asset_id)?;
        validate_text("manual media authority job", &fence.job_id)?;
        validate_text("manual media authority source path", &fence.source_path)?;
        let asset = self
            .canonical_job_asset_for_media_unlocked(media_key)?
            .ok_or("manual face media has no canonical JobAsset")?;
        let current_source_path = asset
            .source_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .ok_or("manual face JobAsset has no canonical source path")?;
        if asset.asset_id != fence.asset_id
            || asset.job_id != fence.job_id
            || current_source_path != fence.source_path
            || asset.media_key != media_key
            || asset.media_fingerprint != media_fingerprint
        {
            return Err("stale manual-face canonical media authority".to_string());
        }
        let job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        let root: MatchIndexRoot =
            self.require_unlocked(ROOT_CONFIG_TABLE, &job.root_key, "MatchIndexRoot")?;
        if !root.enabled {
            return Err("manual face canonical index root is disabled".to_string());
        }
        Ok(())
    }

    pub fn correction_autocomplete(
        &self,
        query: &str,
        expected_catalog_revision: u64,
        limit: usize,
    ) -> Result<Vec<Person>, String> {
        let matches = match self.autocomplete(query, expected_catalog_revision, limit.min(100)) {
            Ok(matches) => matches,
            Err(error) if error == "stale autocomplete catalog revision" => {
                // Catalog mutations invalidate the in-memory projection before
                // their best-effort eager refresh. If that refresh temporarily
                // failed after the durable commit, the next read rebuilds from
                // canonical Person rows instead of leaving the GUI stranded on
                // a permanently stale cache.
                self.refresh_autocomplete()?;
                self.autocomplete(query, expected_catalog_revision, limit.min(100))?
            }
            Err(error) => return Err(error),
        };
        matches
            .into_iter()
            .map(|entry| self.require(PERSON_TABLE, &entry.person_id, "Person"))
            .collect()
    }

    /// A cache projection is never part of a correction's commit verdict.
    /// Invalidate first, recover a poisoned projection lock deterministically,
    /// then attempt an eager rebuild. A transient rebuild error is deliberately
    /// ignored after the canonical transaction committed; `correction_autocomplete`
    /// retries the rebuild on a later read.
    pub(super) fn refresh_autocomplete_after_committed_catalog_change(&self) {
        match self.caches.write() {
            Ok(mut caches) => {
                caches.autocomplete = AutocompleteIndex::default();
                caches.catalog_revision = 0;
            }
            Err(poisoned) => {
                let mut caches = poisoned.into_inner();
                caches.autocomplete = AutocompleteIndex::default();
                caches.catalog_revision = 0;
                drop(caches);
                self.caches.clear_poison();
            }
        }
        let _ = self.refresh_autocomplete();
    }

    /// Return one exact, bounded Viewer join. Embeddings and crop bytes never
    /// enter this API surface.
    pub fn media_faces(&self, media_key: &str) -> Result<ViewerFaceSnapshot, String> {
        validate_media_key(media_key)?;
        let _guard = self.database_read_guard("Match Viewer face snapshot lock is poisoned")?;
        let db = self.database();
        let key = media_key.to_string();
        let (mut faces, configured_rows, active_generations, assets): (
            Vec<FaceObservation>,
            Vec<Value>,
            Vec<ModelGeneration>,
            Vec<JobAsset>,
        ) = surreal_store::run(async move {
            let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_face_observation WHERE media_key = $media_key ORDER BY source_index ASC, face_id ASC LIMIT 4097;\
                         SELECT count() AS count FROM match_index_root WHERE enabled = true GROUP ALL;\
                         SELECT * OMIT id FROM match_model_generation WHERE state = 'active' ORDER BY generation ASC LIMIT 2;
                         SELECT * OMIT id FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key = $media_key ORDER BY updated_at DESC, asset_id ASC LIMIT 1;",
                    )
                    .bind(("media_key", key))
                    .await
                    .map_err(|error| format!("query bounded Match Viewer faces: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match Viewer faces: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match Viewer configuration: {error}"))?,
                response
                    .take(2)
                    .map_err(|error| format!("decode active Match model generation: {error}"))?,
                response
                    .take(3)
                    .map_err(|error| format!("decode Match Viewer media fingerprint: {error}"))?,
            ))
        })?;
        if active_generations.len() > 1 {
            return Err("multiple active Match model generations are invalid".to_string());
        }
        if faces.len() > VIEWER_FACE_LIMIT {
            return Err(format!(
                "media contains more than the bounded Viewer limit of {VIEWER_FACE_LIMIT} faces"
            ));
        }
        faces.sort_by(|left, right| {
            left.source_index
                .cmp(&right.source_index)
                .then_with(|| left.face_id.cmp(&right.face_id))
        });
        let face_ids = faces
            .iter()
            .map(|face| face.face_id.clone())
            .collect::<Vec<_>>();
        let active_generation_for_embeddings = active_generations
            .first()
            .filter(|generation| generation.validated)
            .map(|generation| generation.generation.clone())
            .unwrap_or_default();
        let (assignments, dispositions, suggestions, embeddings) = if face_ids.is_empty() {
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        } else {
            let db = self.database();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_assignment WHERE face_id IN $face_ids ORDER BY face_id ASC LIMIT 4096;\
                         SELECT * OMIT id FROM match_face_disposition WHERE face_id IN $face_ids ORDER BY face_id ASC LIMIT 4096;\
                         SELECT * OMIT id FROM match_suggestion WHERE face_id IN $face_ids ORDER BY face_id ASC, similarity DESC, candidate_person_id ASC LIMIT 16385;\
                         SELECT embedding_id, face_id, model_generation, schema_generation, media_fingerprint, face_revision, job_id, active, created_at FROM match_face_embedding WHERE face_id IN $face_ids AND active = true AND model_generation = $model_generation ORDER BY embedding_id ASC LIMIT 4097;",
                    )
                    .bind(("face_ids", face_ids))
                    .bind(("model_generation", active_generation_for_embeddings))
                    .await
                    .map_err(|error| format!("query Match Viewer face joins: {error}"))?;
                Ok((
                    response
                        .take::<Vec<Assignment>>(0)
                        .map_err(|error| format!("decode Match Viewer assignments: {error}"))?,
                    response
                        .take::<Vec<FaceDisposition>>(1)
                        .map_err(|error| format!("decode Match Viewer dispositions: {error}"))?,
                    response
                        .take::<Vec<Suggestion>>(2)
                        .map_err(|error| format!("decode Match Viewer suggestions: {error}"))?,
                    response
                        .take::<Vec<FaceEmbeddingProvenance>>(3)
                        .map_err(|error| {
                            format!("decode Match Viewer embedding provenance: {error}")
                        })?,
                ))
            })?
        };
        if suggestions.len() > VIEWER_SUGGESTION_SCAN_LIMIT {
            return Err(format!(
                "Viewer suggestion join exceeds bounded scan limit of {VIEWER_SUGGESTION_SCAN_LIMIT}"
            ));
        }
        if embeddings.len() > VIEWER_FACE_LIMIT {
            return Err(format!(
                "media contains more than the bounded Viewer limit of {VIEWER_FACE_LIMIT} active embedding provenance rows"
            ));
        }
        let raw_assignment_by_face = assignments
            .into_iter()
            .map(|assignment| (assignment.face_id.clone(), assignment))
            .collect::<BTreeMap<_, _>>();
        let disposition_by_face = dispositions
            .into_iter()
            .map(|disposition| (disposition.face_id.clone(), disposition))
            .collect::<BTreeMap<_, _>>();
        let mut person_ids = raw_assignment_by_face
            .values()
            .map(|assignment| assignment.person_id.clone())
            .collect::<BTreeSet<_>>();
        person_ids.extend(
            suggestions
                .iter()
                .map(|suggestion| suggestion.candidate_person_id.clone()),
        );
        let person_ids = person_ids.into_iter().collect::<Vec<_>>();
        if person_ids.len() > VIEWER_PERSON_LIMIT {
            return Err(format!(
                "Viewer Person join exceeds bounded limit of {VIEWER_PERSON_LIMIT}"
            ));
        }
        let people = if person_ids.is_empty() {
            Vec::new()
        } else {
            let db = self.database();
            surreal_store::run(async move {
                let mut response = db
                    .query("SELECT * OMIT id FROM match_person WHERE person_id IN $person_ids ORDER BY person_id ASC LIMIT 8192;")
                    .bind(("person_ids", person_ids))
                    .await
                    .map_err(|error| format!("query Match Viewer people: {error}"))?;
                response
                    .take::<Vec<Person>>(0)
                    .map_err(|error| format!("decode Match Viewer people: {error}"))
            })?
        };
        let people = people
            .into_iter()
            .map(|person| (person.person_id.clone(), person))
            .collect::<BTreeMap<_, _>>();
        let active_generation = active_generations
            .first()
            .filter(|generation| generation.validated)
            .map(|generation| generation.generation.as_str());
        let active_calibration = self.current_active_calibration_unlocked(active_generation)?;
        let canonical_asset = assets.first();
        let embedding_by_id = embeddings
            .iter()
            .map(|embedding| (embedding.embedding_id.as_str(), embedding))
            .collect::<BTreeMap<_, _>>();
        let face_by_id = faces
            .iter()
            .map(|face| (face.face_id.as_str(), face))
            .collect::<BTreeMap<_, _>>();
        let assignment_by_face = raw_assignment_by_face
            .into_iter()
            .filter(|(_, assignment)| {
                let Some(face) = face_by_id.get(assignment.face_id.as_str()).copied() else {
                    return false;
                };
                let Some(person) = people.get(&assignment.person_id) else {
                    return false;
                };
                assignment.state == AssignmentState::OperatorConfirmed.as_str()
                    || self.strict_assignment_provenance_is_current_unlocked(
                        assignment,
                        face,
                        person,
                        active_generation,
                        active_calibration.as_ref(),
                        canonical_asset,
                        assignment
                            .model_generation
                            .as_deref()
                            .and_then(|generation| {
                                embedding_by_id
                                    .get(embedding_id(&assignment.face_id, generation).as_str())
                                    .copied()
                            }),
                    )
            })
            .collect::<BTreeMap<_, _>>();
        let mut suggestion_by_face = BTreeMap::new();
        for suggestion in suggestions {
            let Some(face) = face_by_id.get(suggestion.face_id.as_str()).copied() else {
                continue;
            };
            let Some(person) = people.get(&suggestion.candidate_person_id) else {
                continue;
            };
            if self.suggestion_provenance_is_current_unlocked(
                &suggestion,
                face,
                person,
                active_generation,
                canonical_asset,
                embedding_by_id
                    .get(embedding_id(&suggestion.face_id, &suggestion.model_generation).as_str())
                    .copied(),
            ) {
                suggestion_by_face
                    .entry(suggestion.face_id.clone())
                    .or_insert(suggestion);
            }
        }
        let assigned_person_ids = assignment_by_face
            .values()
            .map(|assignment| assignment.person_id.clone())
            .collect::<BTreeSet<_>>();
        let looks = if assigned_person_ids.is_empty() {
            Vec::new()
        } else {
            let db = self.database();
            let assigned_person_ids = assigned_person_ids.into_iter().collect::<Vec<_>>();
            let rows: Vec<Look> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_look WHERE person_id IN $person_ids ORDER BY person_id ASC, name ASC, look_id ASC LIMIT 4097;",
                    )
                    .bind(("person_ids", assigned_person_ids))
                    .await
                    .map_err(|error| format!("query bounded Match Viewer Looks: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match Viewer Looks: {error}"))
            })?;
            if rows.len() > CORRECTION_ROW_LIMIT {
                return Err(format!(
                    "Viewer Look join exceeds bounded limit of {CORRECTION_ROW_LIMIT}"
                ));
            }
            rows
        };
        let execution = self.execution_state_unlocked()?;
        let recent_mappings: Vec<CorrectionMediaOperation> = {
            let db = self.database();
            let media_key = media_key.to_string();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_correction_media_operation WITH INDEX match_correction_media_operation_media WHERE media_key = $media_key ORDER BY created_at DESC, operation_id DESC LIMIT 257;",
                    )
                    .bind(("media_key", media_key))
                    .await
                    .map_err(|error| format!("query bounded Viewer undo candidates: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Viewer undo candidates: {error}"))
            })?
        };
        if recent_mappings.len() > VIEWER_UNDO_SCAN_LIMIT {
            return Err(format!(
                "Viewer undo candidate scan exceeds bounded limit of {VIEWER_UNDO_SCAN_LIMIT}"
            ));
        }
        let mut undo_candidates = Vec::new();
        for mapping in recent_mappings {
            if self
                .get_one_unlocked::<MatchOperation>(
                    OPERATION_TABLE,
                    &format!("undo-{}", mapping.operation_id),
                )?
                .is_some()
            {
                continue;
            }
            let operation: MatchOperation = self.require_unlocked(
                OPERATION_TABLE,
                &mapping.operation_id,
                "correction operation",
            )?;
            let Some(correction_kind) = operation.kind.strip_prefix("correction_") else {
                continue;
            };
            if correction_kind != mapping.kind {
                return Err(
                    "correction media mapping kind does not match its operation".to_string()
                );
            }
            if !operation.reversible {
                continue;
            }
            undo_candidates.push(CorrectionUndoCandidate {
                operation_id: operation.operation_id,
                kind: mapping.kind,
                created_at: operation.created_at,
            });
            if undo_candidates.len() == VIEWER_UNDO_LIMIT {
                break;
            }
        }
        let rows = faces
            .into_iter()
            .map(|face| {
                let assignment = assignment_by_face.get(&face.face_id).cloned();
                let person = assignment
                    .as_ref()
                    .and_then(|assignment| people.get(&assignment.person_id))
                    .cloned();
                let disposition = disposition_by_face.get(&face.face_id).cloned();
                let suggestion = suggestion_by_face.get(&face.face_id).cloned();
                let candidate_person = suggestion
                    .as_ref()
                    .and_then(|suggestion| people.get(&suggestion.candidate_person_id))
                    .cloned();
                ViewerFaceRow {
                    face,
                    assignment,
                    person,
                    disposition,
                    suggestion,
                    candidate_person,
                }
            })
            .collect::<Vec<_>>();
        Ok(ViewerFaceSnapshot {
            media_key: media_key.to_string(),
            media_fingerprint: rows
                .first()
                .map(|row| row.face.media_fingerprint.clone())
                .or_else(|| assets.first().map(|asset| asset.media_fingerprint.clone())),
            configured: configured_rows
                .first()
                .and_then(|row| row.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: active_generations
                .into_iter()
                .next()
                .map(|generation| generation.generation)
                .unwrap_or_else(|| UNCONFIGURED_MODEL_GENERATION.to_string()),
            catalog_revision: execution.catalog_revision,
            total_faces: rows.len(),
            rows,
            looks,
            undo_candidates,
        })
    }

    pub fn person_face_page(
        &self,
        person_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<PersonFacePage, String> {
        if limit == 0 || limit > 512 {
            return Err("Match Person face page limit must be between 1 and 512".to_string());
        }
        let offset = u64::try_from(offset).map_err(|_| "Person face offset overflow")?;
        let _guard = self.database_read_guard("Match Person face page lock is poisoned")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let db = self.database();
        let person_key = person_id.to_string();
        let (assignments, face_counts, media_counts, look_counts, looks): (
            Vec<Assignment>,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            Vec<Look>,
        ) = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id ORDER BY media_key ASC, face_id ASC LIMIT $limit START $offset;\
                     SELECT count() AS count FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP ALL;\
                     SELECT count() AS count FROM (SELECT media_key FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP BY media_key) GROUP ALL;\
                     SELECT count() AS count FROM match_look WHERE person_id = $person_id GROUP ALL;\
                     SELECT * OMIT id FROM match_look WHERE person_id = $person_id ORDER BY name ASC, look_id ASC LIMIT 4097;",
                )
                .bind(("person_id", person_key))
                .bind(("limit", limit as u64))
                .bind(("offset", offset))
                .await
                .map_err(|error| format!("query bounded Match Person face page: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match Person assignments: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match Person face count: {error}"))?,
                response
                    .take(2)
                    .map_err(|error| format!("decode Match Person media count: {error}"))?,
                response
                    .take(3)
                    .map_err(|error| format!("decode Match Person Look count: {error}"))?,
                response
                    .take(4)
                    .map_err(|error| format!("decode Match Person Looks: {error}"))?,
            ))
        })?;
        if looks.len() > CORRECTION_ROW_LIMIT {
            return Err(format!(
                "Person Look inventory exceeds bounded limit of {CORRECTION_ROW_LIMIT}"
            ));
        }
        let face_ids = assignments
            .iter()
            .map(|assignment| assignment.face_id.clone())
            .collect::<Vec<_>>();
        let faces = if face_ids.is_empty() {
            Vec::new()
        } else {
            let db = self.database();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_face_observation WHERE face_id IN $face_ids ORDER BY face_id ASC LIMIT 512;",
                    )
                    .bind(("face_ids", face_ids))
                    .await
                    .map_err(|error| format!("query Match Person page faces: {error}"))?;
                response
                    .take::<Vec<FaceObservation>>(0)
                    .map_err(|error| format!("decode Match Person page faces: {error}"))
            })?
        };
        let face_state = faces
            .into_iter()
            .map(|face| (face.face_id, (face.face_revision, face.media_fingerprint)))
            .collect::<BTreeMap<_, _>>();
        let rows = assignments
            .into_iter()
            .map(|assignment| {
                let (face_revision, media_fingerprint) = face_state
                    .get(&assignment.face_id)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "Assignment {} has no canonical FaceObservation",
                            assignment.assignment_id
                        )
                    })?;
                Ok(PersonFacePageRow {
                    face_id: assignment.face_id,
                    face_revision,
                    media_key: assignment.media_key,
                    media_fingerprint,
                    look_id: assignment.look_id,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let count = |values: &[Value]| {
            values
                .first()
                .and_then(|row| row.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        };
        let execution = self.execution_state_unlocked()?;
        // The visible page is intentionally not used for the confirmation
        // token. Merge/remove bind the complete canonical source-Person
        // assignment inventory, including strict-automatic assignments which
        // do not advance the operator identity revision. The digest streams
        // fixed-size pages so virtualized Person reads remain bounded even
        // when the Person has more rows than one correction can mutate.
        let preview_token =
            self.person_assignment_inventory_preview_token_unlocked(&person.person_id)?;
        Ok(PersonFacePage {
            person,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: self.current_model_generation_unlocked()?,
            catalog_revision: execution.catalog_revision,
            identity_revision: execution.identity_revision,
            preview_token,
            total_faces: count(&face_counts),
            total_media: count(&media_counts),
            total_looks: count(&look_counts),
            offset: offset as usize,
            limit,
            rows,
            looks,
        })
    }

    pub fn correction_fence(&self, face_id: &str) -> Result<CorrectionFence, String> {
        self.correction_fence_for(face_id, Vec::new())
    }

    pub fn correction_fence_for(
        &self,
        face_id: &str,
        mut person_ids: Vec<String>,
    ) -> Result<CorrectionFence, String> {
        if person_ids.len() > 256 {
            return Err("correction fence may bind at most 256 People".to_string());
        }
        person_ids.sort();
        person_ids.dedup();
        let _guard = self.database_read_guard("Match correction fence lock is poisoned")?;
        self.correction_fence_for_unlocked(face_id, person_ids)
    }

    fn correction_fence_for_unlocked(
        &self,
        face_id: &str,
        person_ids: Vec<String>,
    ) -> Result<CorrectionFence, String> {
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        let execution = self.execution_state_unlocked()?;
        let mut person_revisions = BTreeMap::new();
        if let Some(assignment) = &assignment {
            let person: Person =
                self.require_unlocked(PERSON_TABLE, &assignment.person_id, "Person")?;
            person_revisions.insert(person.person_id, person.revision);
        }
        for person_id in person_ids {
            let person: Person = self.require_unlocked(PERSON_TABLE, &person_id, "Person")?;
            person_revisions.insert(person.person_id, person.revision);
        }
        Ok(CorrectionFence {
            face_id: face.face_id,
            face_revision: face.face_revision,
            media_key: face.media_key,
            media_fingerprint: face.media_fingerprint,
            assignment_operation_id: assignment.map(|value| value.operation_id),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: self.current_model_generation_unlocked()?,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            person_revisions,
        })
    }

    pub fn change_person(
        &self,
        face_id: &str,
        new_person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        self.change_person_with_rejection(face_id, None, new_person_id, fence)
    }

    pub fn change_person_correction(
        &self,
        face_id: &str,
        rejected_person_id: &str,
        new_person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        if rejected_person_id == new_person_id {
            return Err("Change person source and target must differ".to_string());
        }
        self.change_person_with_rejection(face_id, Some(rejected_person_id), new_person_id, fence)
    }

    fn change_person_with_rejection(
        &self,
        face_id: &str,
        rejected_person_id: Option<&str>,
        new_person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("Change Person")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(new_person_id))?;
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        if prior.is_none() && rejected_person_id.is_none() {
            return Err(
                "Change person requires a committed source Assignment or an explicit rejected suggestion Person"
                    .to_string(),
            );
        }
        if prior
            .as_ref()
            .is_some_and(|assignment| assignment.person_id == new_person_id)
        {
            return Err("Change person source and target must differ".to_string());
        }
        let old_person_id = prior
            .as_ref()
            .map(|value| value.person_id.as_str())
            .or(rejected_person_id)
            .filter(|value| *value != new_person_id);
        if let Some(requested_old) = rejected_person_id {
            if prior
                .as_ref()
                .is_some_and(|assignment| assignment.person_id != requested_old)
            {
                return Err(
                    "Change person source does not match the committed assignment".to_string(),
                );
            }
            if prior.is_none() {
                self.require_review_candidate_unlocked(
                    "Change person",
                    face_id,
                    requested_old,
                    None,
                )?;
            }
            let old: Person = self.require_unlocked(PERSON_TABLE, requested_old, "Person")?;
            if fence.person_revisions.get(requested_old) != Some(&old.revision) {
                return Err("correction fence does not bind the rejected Person".to_string());
            }
        }
        let person: Person = self.require_unlocked(PERSON_TABLE, new_person_id, "Person")?;
        if self
            .get_one_unlocked::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &cannot_link_id(face_id, new_person_id),
            )?
            .is_some()
        {
            return Err("cannot-linked Person cannot be assigned to this face".to_string());
        }
        let operation_id = new_id("operation");
        let timestamp = now();
        let assignment = Assignment {
            assignment_id: face_id.to_string(),
            face_id: face_id.to_string(),
            person_id: new_person_id.to_string(),
            media_key: face.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "change_person".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: operation_id.clone(),
            created_at: prior
                .as_ref()
                .map(|value| value.created_at.clone())
                .unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp.clone(),
        };
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: face_id.to_string(),
            before: serialize_optional(&prior)?,
            after: Some(serde_json::to_value(&assignment).map_err(|error| error.to_string())?),
        }];
        if let Some(old_person_id) = old_person_id {
            let constraint_id = cannot_link_id(face_id, old_person_id);
            let before = self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?;
            let constraint = CannotLinkConstraint {
                constraint_id: constraint_id.clone(),
                face_id: face_id.to_string(),
                person_id: old_person_id.to_string(),
                operation_id: operation_id.clone(),
                operator_owned: true,
                created_at: timestamp,
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: constraint_id,
                before,
                after: Some(serde_json::to_value(constraint).map_err(|error| error.to_string())?),
            });
        }
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "change_person",
            Some(face_id),
            Some(new_person_id),
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn same_correction(
        &self,
        face_id: &str,
        person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("Same")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        self.require_review_candidate_unlocked("Same", face_id, person_id, prior.as_ref())?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let operation_id = new_id("operation");
        let timestamp = now();
        let assignment = Assignment {
            assignment_id: face_id.to_string(),
            face_id: face_id.to_string(),
            person_id: person_id.to_string(),
            media_key: face.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "same".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: operation_id.clone(),
            created_at: prior
                .as_ref()
                .map(|value| value.created_at.clone())
                .unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp,
        };
        let mut rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: face_id.to_string(),
            before: serialize_optional(&prior)?,
            after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
        }];
        self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
        validate_delta_bound(&rows)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "same",
            Some(face_id),
            Some(person_id),
            rows,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn not_sure_correction(
        &self,
        face_id: &str,
        person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("Not sure")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        self.require_review_candidate_unlocked("Not sure", face_id, person_id, prior.as_ref())?;
        let execution = self.execution_state_unlocked()?;
        let operation_id = new_id("operation");
        let envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: "not_sure".to_string(),
            rows: Vec::new(),
            face_ids: vec![face_id.to_string()],
            media_keys: vec![face.media_key.clone()],
            identity_changed: false,
            catalog_changed: false,
        };
        let created_at = now();
        let after_json = serialize_correction_operation_json(&envelope, "after_json")?;
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: "correction_not_sure".to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(person_id.to_string()),
            before_json: "[]".to_string(),
            after_json,
            reversible: false,
            created_at: created_at.clone(),
        };
        let canonical_fingerprint =
            canonical_correction_media_fingerprint(&face.media_fingerprint)?;
        let mapping = CorrectionMediaOperation {
            mapping_id: correction_media_mapping_id(&operation_id, &face.media_key),
            media_key: face.media_key.clone(),
            media_fingerprint: canonical_fingerprint,
            operation_id: operation_id.clone(),
            kind: "not_sure".to_string(),
            created_at: created_at.clone(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    OPERATION_TABLE,
                    operation_id.as_str(),
                    serde_json::to_value(operation).map_err(|error| error.to_string())?,
                ),
                (
                    CORRECTION_MEDIA_OPERATION_TABLE,
                    mapping.mapping_id.as_str(),
                    serde_json::to_value(&mapping).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        drop(_guard);
        Ok(CorrectionReceipt {
            operation_id,
            kind: "not_sure".to_string(),
            changed_rows: 0,
            conflict_rows: 0,
            affected_face_ids: vec![face_id.to_string()],
            affected_media_keys: vec![face.media_key],
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    pub fn move_to_look_correction(
        &self,
        face_id: &str,
        look_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("move to Look correction")?;
        let mut assignment: Assignment =
            self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(&assignment.person_id))?;
        let look: Look = self.require_unlocked(LOOK_TABLE, look_id, "Look")?;
        if look.person_id != assignment.person_id {
            return Err("Look does not belong to the assigned Person".to_string());
        }
        let before = serde_json::to_value(&assignment).map_err(|error| error.to_string())?;
        let operation_id = new_id("operation");
        assignment.look_id = Some(look_id.to_string());
        assignment.placement = "look".to_string();
        assignment.operation_id = operation_id.clone();
        assignment.updated_at = now();
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: face_id.to_string(),
            before: Some(before),
            after: Some(serde_json::to_value(&assignment).map_err(|error| error.to_string())?),
        }];
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "move_to_look",
            Some(face_id),
            Some(&assignment.person_id),
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn same_person_new_look_correction(
        &self,
        face_id: &str,
        person_id: &str,
        look_name: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        validate_text("Look name", look_name)?;
        let _guard = self.correction_write_guard("Same person, new Look")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
        let mut assignment: Assignment =
            self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
        if assignment.person_id != person_id {
            return Err("Same person, new Look does not match the assigned Person".to_string());
        }
        if self
            .query_typed_by_field_unlocked::<Look>(LOOK_TABLE, "person_id", person_id)?
            .iter()
            .any(|look| look.name.eq_ignore_ascii_case(look_name.trim()))
        {
            return Err("a Look with that name already exists for this Person".to_string());
        }
        let operation_id = new_id("operation");
        let timestamp = now();
        let look = Look {
            look_id: new_id("look"),
            person_id: person_id.to_string(),
            name: look_name.trim().to_string(),
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let before_assignment =
            serde_json::to_value(&assignment).map_err(|error| error.to_string())?;
        assignment.look_id = Some(look.look_id.clone());
        assignment.placement = "look".to_string();
        assignment.operation_id = operation_id.clone();
        assignment.updated_at = timestamp;
        let mut rows = vec![
            CorrectionRowDelta {
                table: CorrectionTable::Look,
                stable_id: look.look_id.clone(),
                before: None,
                after: Some(serde_json::to_value(look).map_err(|error| error.to_string())?),
            },
            CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.to_string(),
                before: Some(before_assignment),
                after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
            },
        ];
        self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
        validate_delta_bound(&rows)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "same_person_new_look",
            Some(face_id),
            Some(person_id),
            rows,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn different_correction(
        &self,
        face_id: &str,
        person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        self.reject_person_correction(face_id, person_id, fence, RejectionTrigger::Different)
    }

    pub fn this_is_not_correction(
        &self,
        face_id: &str,
        person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        self.reject_person_correction(face_id, person_id, fence, RejectionTrigger::ThisIsNot)
    }

    fn reject_person_correction(
        &self,
        face_id: &str,
        person_id: &str,
        fence: &CorrectionFence,
        trigger: RejectionTrigger,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("This-is-not")?;
        let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        if let Some(assignment) = &prior {
            self.validate_correction_fence_unlocked(fence, Some(&assignment.person_id))?;
        }
        let assignment_matches =
            self.validate_rejection_trigger_unlocked(trigger, face_id, person_id, prior.as_ref())?;
        let operation_id = new_id("operation");
        let constraint_id = cannot_link_id(face_id, person_id);
        let prior_constraint = self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?;
        let constraint = CannotLinkConstraint {
            constraint_id: constraint_id.clone(),
            face_id: face_id.to_string(),
            person_id: person_id.to_string(),
            operation_id: operation_id.clone(),
            operator_owned: true,
            created_at: now(),
        };
        let mut deltas = Vec::new();
        if assignment_matches {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.to_string(),
                before: serialize_optional(&prior)?,
                after: None,
            });
        }
        deltas.push(CorrectionRowDelta {
            table: CorrectionTable::Constraint,
            stable_id: constraint_id,
            before: prior_constraint,
            after: Some(serde_json::to_value(constraint).map_err(|error| error.to_string())?),
        });
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        if assignment_matches {
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            match trigger {
                RejectionTrigger::Different => "different",
                RejectionTrigger::ThisIsNot => "this_is_not",
            },
            Some(face_id),
            Some(person_id),
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn remove_assignment_correction(
        &self,
        face_id: &str,
        source_person_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("remove assignment")?;
        let prior = self
            .get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?
            .ok_or("face has no committed assignment to remove")?;
        if prior.person_id != source_person_id {
            return Err("remove-assignment source Person does not own the assignment".to_string());
        }
        let face = self.validate_correction_fence_unlocked(fence, Some(source_person_id))?;
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: face_id.to_string(),
            before: Some(serde_json::to_value(&prior).map_err(|error| error.to_string())?),
            after: None,
        }];
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        validate_delta_bound(&deltas)?;
        let operation_id = new_id("operation");
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "remove_assignment",
            Some(face_id),
            Some(source_person_id),
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    /// Produce an exact action-specific batch dry run. This method performs
    /// no writes and does not bump revisions; the returned preview is the only
    /// accepted input to `apply_batch_correction`.
    pub fn batch_correction_preflight(
        &self,
        action: BatchCorrectionAction,
        source_person_id: Option<&str>,
        target_person_id: Option<&str>,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<BatchCorrectionPreview, String> {
        let _guard =
            self.database_read_guard("Match batch correction preflight lock is poisoned")?;
        self.plan_batch_correction_unlocked(
            action,
            source_person_id.map(str::to_string),
            target_person_id.map(str::to_string),
            face_ids,
            fences,
            new_id("operation"),
            now(),
        )
        .map(|plan| plan.preview)
    }

    /// Recompute and atomically apply one exact batch preview. No alternate
    /// batch apply route is permitted to synthesize its own preview.
    pub fn apply_batch_correction(
        &self,
        preview: &BatchCorrectionPreview,
    ) -> Result<CorrectionReceipt, String> {
        if batch_preview_digest(preview)? != preview.preview_id {
            return Err("batch correction preview integrity mismatch".to_string());
        }
        let _guard = self.correction_write_guard("batch correction apply")?;
        if self
            .get_one_unlocked::<MatchOperation>(OPERATION_TABLE, &preview.planned_operation_id)?
            .is_some()
        {
            return Err("batch correction preview has already been applied".to_string());
        }
        let plan = self.plan_batch_correction_unlocked(
            preview.action,
            preview.source_person_id.clone(),
            preview.target_person_id.clone(),
            preview.face_ids.clone(),
            preview.fences.clone(),
            preview.planned_operation_id.clone(),
            preview.planned_at.clone(),
        )?;
        if plan.preview != *preview {
            return Err("stale or mismatched batch correction preview".to_string());
        }
        ensure_within_delta_limit(plan.preview.required_reversible_rows)?;
        if preview.action == BatchCorrectionAction::NotSure {
            let execution = self.execution_state_unlocked()?;
            let envelope = CorrectionDeltaEnvelope {
                version: CORRECTION_DELTA_VERSION,
                kind: preview.action.as_str().to_string(),
                rows: Vec::new(),
                face_ids: preview.face_ids.clone(),
                media_keys: preview.media_keys.clone(),
                identity_changed: false,
                catalog_changed: false,
            };
            let after_json = serialize_correction_operation_json(&envelope, "after_json")?;
            let operation = MatchOperation {
                operation_id: preview.planned_operation_id.clone(),
                kind: format!("correction_{}", preview.action.as_str()),
                face_id: None,
                person_id: preview.source_person_id.clone(),
                before_json: "[]".to_string(),
                after_json,
                reversible: false,
                created_at: preview.planned_at.clone(),
            };
            let mut upserts = vec![(
                OPERATION_TABLE.to_string(),
                preview.planned_operation_id.clone(),
                serde_json::to_value(operation).map_err(|error| error.to_string())?,
            )];
            let media_fingerprints = self.correction_media_fingerprint_map_unlocked(
                &preview.media_keys,
                &preview.face_ids,
                &plan.rows,
                &[],
            )?;
            for media_key in &preview.media_keys {
                let mapping = CorrectionMediaOperation {
                    mapping_id: correction_media_mapping_id(
                        &preview.planned_operation_id,
                        media_key,
                    ),
                    media_key: media_key.clone(),
                    media_fingerprint: media_fingerprints
                        .get(media_key)
                        .cloned()
                        .ok_or_else(|| {
                            format!(
                                "batch correction media {media_key} lacks exact fingerprint evidence"
                            )
                        })?,
                    operation_id: preview.planned_operation_id.clone(),
                    kind: preview.action.as_str().to_string(),
                    created_at: preview.planned_at.clone(),
                };
                upserts.push((
                    CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                    mapping.mapping_id.clone(),
                    serde_json::to_value(mapping).map_err(|error| error.to_string())?,
                ));
            }
            self.commit_owned_unlocked(&upserts, &[])?;
            drop(_guard);
            return Ok(CorrectionReceipt {
                operation_id: preview.planned_operation_id.clone(),
                kind: preview.action.as_str().to_string(),
                changed_rows: 0,
                conflict_rows: 0,
                affected_face_ids: preview.face_ids.clone(),
                affected_media_keys: preview.media_keys.clone(),
                identity_revision: execution.identity_revision,
                catalog_revision: execution.catalog_revision,
            });
        }

        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt_person_id = match preview.action {
            BatchCorrectionAction::Same => preview.source_person_id.as_deref(),
            BatchCorrectionAction::ChangePerson => preview.target_person_id.as_deref(),
            BatchCorrectionAction::Different
            | BatchCorrectionAction::ThisIsNot
            | BatchCorrectionAction::RemoveAssignment => preview.source_person_id.as_deref(),
            _ => None,
        };
        let receipt = self.commit_correction_unlocked(
            &preview.planned_operation_id,
            preview.action.as_str(),
            None,
            receipt_person_id,
            plan.rows,
            preview.face_ids.clone(),
            preview.media_keys.clone(),
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_batch_correction_unlocked(
        &self,
        action: BatchCorrectionAction,
        source_person_id: Option<String>,
        target_person_id: Option<String>,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
        operation_id: String,
        planned_at: String,
    ) -> Result<BatchCorrectionPlan, String> {
        validate_batch_selection(&face_ids, &fences)?;
        validate_text("planned batch operation ID", &operation_id)?;
        validate_text("planned batch timestamp", &planned_at)?;
        let execution = self.execution_state_unlocked()?;
        let model_generation = self.current_model_generation_unlocked()?;
        let source = match action {
            BatchCorrectionAction::Same
            | BatchCorrectionAction::Different
            | BatchCorrectionAction::NotSure
            | BatchCorrectionAction::ThisIsNot
            | BatchCorrectionAction::ChangePerson
            | BatchCorrectionAction::RemoveAssignment => {
                let source_id = source_person_id
                    .as_deref()
                    .ok_or_else(|| format!("{} requires a source Person", action.as_str()))?;
                Some(self.require_unlocked::<Person>(PERSON_TABLE, source_id, "Person")?)
            }
            _ => {
                if source_person_id.is_some() {
                    return Err(format!(
                        "{} does not accept a source Person",
                        action.as_str()
                    ));
                }
                None
            }
        };
        let target = match action {
            BatchCorrectionAction::ChangePerson => {
                let target_id = target_person_id
                    .as_deref()
                    .ok_or("batch Change person requires a target Person")?;
                if source_person_id.as_deref() == Some(target_id) {
                    return Err("batch Change person source and target must differ".to_string());
                }
                Some(self.require_unlocked::<Person>(PERSON_TABLE, target_id, "Person")?)
            }
            BatchCorrectionAction::Same => {
                if target_person_id.is_some() {
                    return Err(
                        "batch Same uses the source Person and no target Person".to_string()
                    );
                }
                source.clone()
            }
            _ => {
                if target_person_id.is_some() {
                    return Err(format!(
                        "{} does not accept a target Person",
                        action.as_str()
                    ));
                }
                None
            }
        };

        let mut rows = Vec::new();
        let mut media_keys = BTreeSet::new();
        let mut candidate_provenance =
            Vec::<(String, Option<Assignment>, Option<Suggestion>)>::new();
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            if matches!(
                action,
                BatchCorrectionAction::Same
                    | BatchCorrectionAction::Different
                    | BatchCorrectionAction::NotSure
                    | BatchCorrectionAction::ThisIsNot
            ) {
                let candidate_person_id = source_person_id
                    .as_deref()
                    .expect("review action source validated");
                candidate_provenance.push((
                    face_id.clone(),
                    assignment.clone(),
                    self.get_one_unlocked::<Suggestion>(
                        SUGGESTION_TABLE,
                        &suggestion_id(face_id, candidate_person_id),
                    )?,
                ));
            }
            let required_fence_person = match action {
                BatchCorrectionAction::Same
                | BatchCorrectionAction::Different
                | BatchCorrectionAction::NotSure
                | BatchCorrectionAction::ThisIsNot
                | BatchCorrectionAction::ChangePerson
                | BatchCorrectionAction::RemoveAssignment => source_person_id.as_deref(),
                _ => assignment
                    .as_ref()
                    .map(|assignment| assignment.person_id.as_str()),
            };
            let face = self.validate_correction_fence_unlocked(fence, required_fence_person)?;
            media_keys.insert(face.media_key.clone());
            match action {
                BatchCorrectionAction::Same => {
                    let person = target.as_ref().expect("batch Same target validated");
                    self.require_review_candidate_unlocked(
                        action.as_str(),
                        face_id,
                        &person.person_id,
                        assignment.as_ref(),
                    )?;
                    let replacement = Assignment {
                        assignment_id: face_id.clone(),
                        face_id: face_id.clone(),
                        person_id: person.person_id.clone(),
                        media_key: face.media_key.clone(),
                        look_id: None,
                        placement: "unsorted".to_string(),
                        state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                        provenance: "batch_same".to_string(),
                        locked: true,
                        model_generation: None,
                        calibration_generation: None,
                        envelope_hash: None,
                        face_revision: face.face_revision,
                        person_revision: person.revision,
                        operation_id: operation_id.clone(),
                        created_at: assignment
                            .as_ref()
                            .map(|value| value.created_at.clone())
                            .unwrap_or_else(|| planned_at.clone()),
                        updated_at: planned_at.clone(),
                    };
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Assignment,
                        stable_id: face_id.clone(),
                        before: serialize_optional(&assignment)?,
                        after: Some(
                            serde_json::to_value(replacement).map_err(|error| error.to_string())?,
                        ),
                    });
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                }
                BatchCorrectionAction::Different | BatchCorrectionAction::ThisIsNot => {
                    let person = source.as_ref().expect("batch rejection source validated");
                    let trigger = match action {
                        BatchCorrectionAction::Different => RejectionTrigger::Different,
                        BatchCorrectionAction::ThisIsNot => RejectionTrigger::ThisIsNot,
                        _ => unreachable!("batch rejection branch is closed"),
                    };
                    let removes_assignment = self.validate_rejection_trigger_unlocked(
                        trigger,
                        face_id,
                        &person.person_id,
                        assignment.as_ref(),
                    )?;
                    if removes_assignment {
                        rows.push(CorrectionRowDelta {
                            table: CorrectionTable::Assignment,
                            stable_id: face_id.clone(),
                            before: serialize_optional(&assignment)?,
                            after: None,
                        });
                    }
                    let constraint_id = cannot_link_id(face_id, &person.person_id);
                    let constraint = CannotLinkConstraint {
                        constraint_id: constraint_id.clone(),
                        face_id: face_id.clone(),
                        person_id: person.person_id.clone(),
                        operation_id: operation_id.clone(),
                        operator_owned: true,
                        created_at: planned_at.clone(),
                    };
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Constraint,
                        stable_id: constraint_id.clone(),
                        before: self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?,
                        after: Some(
                            serde_json::to_value(constraint).map_err(|error| error.to_string())?,
                        ),
                    });
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    if removes_assignment {
                        self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                    }
                }
                BatchCorrectionAction::NotSure => {
                    let person = source.as_ref().expect("batch Not sure source validated");
                    self.require_review_candidate_unlocked(
                        action.as_str(),
                        face_id,
                        &person.person_id,
                        assignment.as_ref(),
                    )?;
                }
                BatchCorrectionAction::ChangePerson => {
                    let source = source.as_ref().expect("batch change source validated");
                    let target = target.as_ref().expect("batch change target validated");
                    let prior = assignment.ok_or_else(|| {
                        format!("face {face_id} has no source assignment to change")
                    })?;
                    if prior.person_id != source.person_id {
                        return Err(format!(
                            "face {face_id} no longer belongs to the batch source Person"
                        ));
                    }
                    if fence.person_revisions.get(&target.person_id) != Some(&target.revision) {
                        return Err(
                            "batch Change person fence does not bind the target Person".to_string()
                        );
                    }
                    if self
                        .get_one_unlocked::<CannotLinkConstraint>(
                            CONSTRAINT_TABLE,
                            &cannot_link_id(face_id, &target.person_id),
                        )?
                        .is_some()
                    {
                        return Err(format!(
                            "face {face_id} is cannot-linked to the batch target Person"
                        ));
                    }
                    let constraint_id = cannot_link_id(face_id, &source.person_id);
                    let constraint = CannotLinkConstraint {
                        constraint_id: constraint_id.clone(),
                        face_id: face_id.clone(),
                        person_id: source.person_id.clone(),
                        operation_id: operation_id.clone(),
                        operator_owned: true,
                        created_at: planned_at.clone(),
                    };
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Constraint,
                        stable_id: constraint_id.clone(),
                        before: self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?,
                        after: Some(
                            serde_json::to_value(constraint).map_err(|error| error.to_string())?,
                        ),
                    });
                    let replacement = Assignment {
                        assignment_id: face_id.clone(),
                        face_id: face_id.clone(),
                        person_id: target.person_id.clone(),
                        media_key: face.media_key.clone(),
                        look_id: None,
                        placement: "unsorted".to_string(),
                        state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                        provenance: "batch_change_person".to_string(),
                        locked: true,
                        model_generation: None,
                        calibration_generation: None,
                        envelope_hash: None,
                        face_revision: face.face_revision,
                        person_revision: target.revision,
                        operation_id: operation_id.clone(),
                        created_at: prior.created_at.clone(),
                        updated_at: planned_at.clone(),
                    };
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Assignment,
                        stable_id: face_id.clone(),
                        before: Some(
                            serde_json::to_value(prior).map_err(|error| error.to_string())?,
                        ),
                        after: Some(
                            serde_json::to_value(replacement).map_err(|error| error.to_string())?,
                        ),
                    });
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                }
                BatchCorrectionAction::RemoveAssignment => {
                    let person = source.as_ref().expect("batch remove source validated");
                    let prior = assignment.ok_or_else(|| {
                        format!("face {face_id} has no committed assignment to remove")
                    })?;
                    if prior.person_id != person.person_id {
                        return Err(format!(
                            "face {face_id} is not assigned to the requested source Person"
                        ));
                    }
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Assignment,
                        stable_id: face_id.clone(),
                        before: Some(
                            serde_json::to_value(prior).map_err(|error| error.to_string())?,
                        ),
                        after: None,
                    });
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                }
                BatchCorrectionAction::IgnoreFace | BatchCorrectionAction::NotAFace => {
                    let disposition_kind = if action == BatchCorrectionAction::IgnoreFace {
                        FaceDispositionKind::Ignored
                    } else {
                        FaceDispositionKind::NotAFace
                    };
                    let prior_disposition =
                        self.get_value_unlocked(FACE_DISPOSITION_TABLE, face_id)?;
                    let disposition = FaceDisposition {
                        face_id: face_id.clone(),
                        media_key: face.media_key.clone(),
                        disposition: disposition_kind.as_str().to_string(),
                        operation_id: operation_id.clone(),
                        face_revision: face.face_revision,
                        created_at: prior_disposition
                            .as_ref()
                            .and_then(|value| value.get("created_at"))
                            .and_then(Value::as_str)
                            .unwrap_or(&planned_at)
                            .to_string(),
                        updated_at: planned_at.clone(),
                    };
                    rows.push(CorrectionRowDelta {
                        table: CorrectionTable::Disposition,
                        stable_id: face_id.clone(),
                        before: prior_disposition,
                        after: Some(
                            serde_json::to_value(disposition).map_err(|error| error.to_string())?,
                        ),
                    });
                    if let Some(prior) = assignment {
                        rows.push(CorrectionRowDelta {
                            table: CorrectionTable::Assignment,
                            stable_id: face_id.clone(),
                            before: Some(
                                serde_json::to_value(prior).map_err(|error| error.to_string())?,
                            ),
                            after: None,
                        });
                    }
                    if disposition_kind == FaceDispositionKind::NotAFace {
                        for (id, value) in
                            self.query_face_values_unlocked(EMBEDDING_TABLE, face_id)?
                        {
                            rows.push(CorrectionRowDelta {
                                table: CorrectionTable::Embedding,
                                stable_id: id,
                                before: Some(value),
                                after: None,
                            });
                        }
                    }
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                }
                BatchCorrectionAction::DeleteFaceAnalysis => {
                    self.push_existing_delta_unlocked(
                        &mut rows,
                        CorrectionTable::Face,
                        face_id,
                        None,
                    )?;
                    for (table, correction_table) in [
                        (EMBEDDING_TABLE, CorrectionTable::Embedding),
                        (ASSIGNMENT_TABLE, CorrectionTable::Assignment),
                        (CONSTRAINT_TABLE, CorrectionTable::Constraint),
                        (FACE_DISPOSITION_TABLE, CorrectionTable::Disposition),
                    ] {
                        for (id, value) in self.query_face_values_unlocked(table, face_id)? {
                            rows.push(CorrectionRowDelta {
                                table: correction_table.clone(),
                                stable_id: id,
                                before: Some(value),
                                after: None,
                            });
                        }
                    }
                    self.append_face_suggestion_removal_deltas_unlocked(&mut rows, face_id)?;
                    self.append_trust_removal_deltas_unlocked(&mut rows, face_id)?;
                }
            }
        }
        let delta_counts = batch_delta_counts(&rows)?;
        let required_reversible_rows = delta_counts.checked_total()?;
        if required_reversible_rows != rows.len() {
            return Err("batch correction delta-count accounting mismatch".to_string());
        }
        let (person_ids, look_ids) = batch_affected_identity_ids(
            source_person_id.as_deref(),
            target_person_id.as_deref(),
            &rows,
        );
        let media_keys = media_keys.into_iter().collect::<Vec<_>>();
        let affected_counts = BatchCorrectionAffectedCounts {
            persons: person_ids.len(),
            looks: look_ids.len(),
            faces: face_ids.len(),
            media: media_keys.len(),
        };
        let delta_digest = batch_delta_digest(&rows)?;
        let provenance_digest = batch_provenance_digest(
            action,
            source_person_id.as_deref(),
            target_person_id.as_deref(),
            &face_ids,
            &fences,
            &candidate_provenance,
            &delta_digest,
            execution.identity_revision,
            execution.catalog_revision,
            &model_generation,
        )?;
        let mut preview = BatchCorrectionPreview {
            preview_id: String::new(),
            action,
            source_person_id,
            target_person_id,
            face_ids,
            person_ids,
            look_ids,
            media_keys,
            affected_counts,
            delta_counts,
            required_reversible_rows,
            correction_delta_row_limit: CORRECTION_ROW_LIMIT,
            within_limit: required_reversible_rows <= CORRECTION_ROW_LIMIT,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            fences,
            provenance_digest,
            delta_digest,
            planned_operation_id: operation_id,
            planned_at,
        };
        preview.preview_id = batch_preview_digest(&preview)?;
        Ok(BatchCorrectionPlan { preview, rows })
    }

    pub fn batch_different_correction(
        &self,
        source_person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        let _ = (source_person_id, face_ids, fences);
        Err(
            "batch Different requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        )
    }

    pub fn batch_this_is_not_correction(
        &self,
        source_person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        let _ = (source_person_id, face_ids, fences);
        Err(
            "batch This-is-not requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        )
    }

    fn batch_reject_person_correction(
        &self,
        source_person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
        trigger: RejectionTrigger,
    ) -> Result<CorrectionReceipt, String> {
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch Different")?;
        let _: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let operation_id = new_id("operation");
        let timestamp = now();
        let mut deltas = Vec::with_capacity(face_ids.len() * 2);
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let face = self.validate_correction_fence_unlocked(fence, Some(source_person_id))?;
            let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            if let Some(assignment) = &assignment {
                self.validate_correction_fence_unlocked(fence, Some(&assignment.person_id))?;
            }
            let removes_assignment = self.validate_rejection_trigger_unlocked(
                trigger,
                face_id,
                source_person_id,
                assignment.as_ref(),
            )?;
            if removes_assignment {
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::Assignment,
                    stable_id: face_id.clone(),
                    before: serialize_optional(&assignment)?,
                    after: None,
                });
            }
            let constraint_id = cannot_link_id(face_id, source_person_id);
            let before_constraint = self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?;
            let constraint = CannotLinkConstraint {
                constraint_id: constraint_id.clone(),
                face_id: face_id.clone(),
                person_id: source_person_id.to_string(),
                operation_id: operation_id.clone(),
                operator_owned: true,
                created_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: constraint_id,
                before: before_constraint,
                after: Some(serde_json::to_value(constraint).map_err(|error| error.to_string())?),
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            if removes_assignment {
                self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            }
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            match trigger {
                RejectionTrigger::Different => "batch_different",
                RejectionTrigger::ThisIsNot => "batch_this_is_not",
            },
            None,
            Some(source_person_id),
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn batch_change_person_correction(
        &self,
        source_person_id: &str,
        target_person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        return Err(
            "batch Change person requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        );
        #[allow(unreachable_code)]
        validate_batch_selection(&face_ids, &fences)?;
        if source_person_id == target_person_id {
            return Err("batch Change person source and target must differ".to_string());
        }
        let _guard = self.correction_write_guard("batch Change person")?;
        let _: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let target: Person = self.require_unlocked(PERSON_TABLE, target_person_id, "Person")?;
        let operation_id = new_id("operation");
        let timestamp = now();
        let mut deltas = Vec::with_capacity(face_ids.len() * 2);
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let face = self.validate_correction_fence_unlocked(fence, Some(source_person_id))?;
            if fence.person_revisions.get(target_person_id) != Some(&target.revision) {
                return Err("batch Change person fence does not bind the target Person".to_string());
            }
            let prior: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            if prior.person_id != source_person_id {
                return Err(format!(
                    "face {face_id} no longer belongs to the batch source Person"
                ));
            }
            if self
                .get_one_unlocked::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &cannot_link_id(face_id, target_person_id),
                )?
                .is_some()
            {
                return Err(format!(
                    "face {face_id} is cannot-linked to the batch target Person"
                ));
            }
            let constraint_id = cannot_link_id(face_id, source_person_id);
            let before_constraint = self.get_value_unlocked(CONSTRAINT_TABLE, &constraint_id)?;
            let constraint = CannotLinkConstraint {
                constraint_id: constraint_id.clone(),
                face_id: face_id.clone(),
                person_id: source_person_id.to_string(),
                operation_id: operation_id.clone(),
                operator_owned: true,
                created_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: constraint_id,
                before: before_constraint,
                after: Some(serde_json::to_value(constraint).map_err(|error| error.to_string())?),
            });
            let assignment = Assignment {
                assignment_id: face_id.clone(),
                face_id: face_id.clone(),
                person_id: target_person_id.to_string(),
                media_key: face.media_key.clone(),
                look_id: None,
                placement: "unsorted".to_string(),
                state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                provenance: "batch_change_person".to_string(),
                locked: true,
                model_generation: None,
                calibration_generation: None,
                envelope_hash: None,
                face_revision: face.face_revision,
                person_revision: target.revision,
                operation_id: operation_id.clone(),
                created_at: prior.created_at.clone(),
                updated_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.clone(),
                before: Some(serde_json::to_value(prior).map_err(|error| error.to_string())?),
                after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "batch_change_person",
            None,
            Some(target_person_id),
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn batch_remove_assignments_correction(
        &self,
        source_person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        return Err(
            "batch remove assignments requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        );
        #[allow(unreachable_code)]
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch remove assignments")?;
        let _: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let operation_id = new_id("operation");
        let mut deltas = Vec::with_capacity(face_ids.len());
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let assignment: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            if assignment.person_id != source_person_id {
                return Err(format!(
                    "face {face_id} is not assigned to the requested source Person"
                ));
            }
            let face = self.validate_correction_fence_unlocked(fence, Some(source_person_id))?;
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.clone(),
                before: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
                after: None,
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "batch_remove_assignments",
            None,
            None,
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn batch_same_correction(
        &self,
        person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        return Err(
            "batch Same requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        );
        #[allow(unreachable_code)]
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch Same")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let operation_id = new_id("operation");
        let timestamp = now();
        let mut deltas = Vec::with_capacity(face_ids.len());
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
            let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            self.require_review_candidate_unlocked(
                "batch Same",
                face_id,
                person_id,
                prior.as_ref(),
            )?;
            let assignment = Assignment {
                assignment_id: face_id.clone(),
                face_id: face_id.clone(),
                person_id: person_id.to_string(),
                media_key: face.media_key.clone(),
                look_id: None,
                placement: "unsorted".to_string(),
                state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                provenance: "batch_same".to_string(),
                locked: true,
                model_generation: None,
                calibration_generation: None,
                envelope_hash: None,
                face_revision: face.face_revision,
                person_revision: person.revision,
                operation_id: operation_id.clone(),
                created_at: prior
                    .as_ref()
                    .map(|assignment| assignment.created_at.clone())
                    .unwrap_or_else(|| timestamp.clone()),
                updated_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.clone(),
                before: serialize_optional(&prior)?,
                after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "batch_same",
            None,
            Some(person_id),
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn batch_not_sure_correction(
        &self,
        person_id: &str,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        return Err(
            "batch Not sure requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        );
        #[allow(unreachable_code)]
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch Not sure")?;
        let _: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let face = self.validate_correction_fence_unlocked(fence, Some(person_id))?;
            let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            if let Some(assignment) = &assignment {
                self.validate_correction_fence_unlocked(fence, Some(&assignment.person_id))?;
            }
            let association_matches = assignment
                .as_ref()
                .is_some_and(|assignment| assignment.person_id == person_id)
                || self
                    .get_one_unlocked::<Suggestion>(
                        SUGGESTION_TABLE,
                        &suggestion_id(face_id, person_id),
                    )?
                    .is_some();
            if !association_matches {
                return Err(format!("face {face_id} has no matching Not-sure candidate"));
            }
            media_keys.push(face.media_key);
        }
        media_keys.sort();
        media_keys.dedup();
        let execution = self.execution_state_unlocked()?;
        let operation_id = new_id("operation");
        let envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: "batch_not_sure".to_string(),
            rows: Vec::new(),
            face_ids: face_ids.clone(),
            media_keys: media_keys.clone(),
            identity_changed: false,
            catalog_changed: false,
        };
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: "correction_batch_not_sure".to_string(),
            face_id: None,
            person_id: Some(person_id.to_string()),
            before_json: "[]".to_string(),
            after_json: serde_json::to_string(&envelope).map_err(|error| error.to_string())?,
            reversible: false,
            created_at: now(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                OPERATION_TABLE,
                operation_id.as_str(),
                serde_json::to_value(operation).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        drop(_guard);
        Ok(CorrectionReceipt {
            operation_id,
            kind: "batch_not_sure".to_string(),
            changed_rows: 0,
            conflict_rows: 0,
            affected_face_ids: face_ids,
            affected_media_keys: media_keys,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    pub fn batch_ignore_faces(
        &self,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        let _ = (face_ids, fences);
        Err(
            "batch Ignore face requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        )
    }

    pub fn batch_mark_not_a_face(
        &self,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        let _ = (face_ids, fences);
        Err(
            "batch Not-a-face requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        )
    }

    pub fn batch_delete_face_analysis(
        &self,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        return Err(
            "batch delete face analysis requires batch_correction_preflight plus apply_batch_correction"
                .to_string(),
        );
        #[allow(unreachable_code)]
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch delete face analysis")?;
        let operation_id = new_id("operation");
        let mut deltas = Vec::new();
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            let face = self.validate_correction_fence_unlocked(
                fence,
                assignment
                    .as_ref()
                    .map(|assignment| assignment.person_id.as_str()),
            )?;
            self.push_existing_delta_unlocked(&mut deltas, CorrectionTable::Face, face_id, None)?;
            for (table, correction_table) in [
                (EMBEDDING_TABLE, CorrectionTable::Embedding),
                (ASSIGNMENT_TABLE, CorrectionTable::Assignment),
                (CONSTRAINT_TABLE, CorrectionTable::Constraint),
                (FACE_DISPOSITION_TABLE, CorrectionTable::Disposition),
            ] {
                for (id, value) in self.query_face_values_unlocked(table, face_id)? {
                    deltas.push(CorrectionRowDelta {
                        table: correction_table.clone(),
                        stable_id: id,
                        before: Some(value),
                        after: None,
                    });
                }
            }
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "batch_delete_face_analysis",
            None,
            None,
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn ignore_face(
        &self,
        face_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        self.set_face_disposition(face_id, fence, FaceDispositionKind::Ignored)
    }

    pub fn mark_not_a_face(
        &self,
        face_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        self.set_face_disposition(face_id, fence, FaceDispositionKind::NotAFace)
    }

    pub fn delete_face_analysis(
        &self,
        face_id: &str,
        fence: &CorrectionFence,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("delete face analysis")?;
        let face = self.validate_correction_fence_unlocked(fence, None)?;
        let mut deltas = Vec::new();
        self.push_existing_delta_unlocked(&mut deltas, CorrectionTable::Face, face_id, None)?;
        for (table, correction_table) in [
            (EMBEDDING_TABLE, CorrectionTable::Embedding),
            (ASSIGNMENT_TABLE, CorrectionTable::Assignment),
            (CONSTRAINT_TABLE, CorrectionTable::Constraint),
            (FACE_DISPOSITION_TABLE, CorrectionTable::Disposition),
        ] {
            for (id, value) in self.query_face_values_unlocked(table, face_id)? {
                deltas.push(CorrectionRowDelta {
                    table: correction_table.clone(),
                    stable_id: id,
                    before: Some(value),
                    after: None,
                });
            }
        }
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        validate_delta_bound(&deltas)?;
        let operation_id = new_id("operation");
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "delete_face_analysis",
            Some(face_id),
            None,
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    pub fn create_manual_face(&self, input: ManualFaceInput) -> Result<ManualFaceReceipt, String> {
        validate_media_key(&input.media_key)?;
        validate_text("media fingerprint", &input.media_fingerprint)?;
        validate_text("pose bucket", &input.pose_bucket)?;
        if !input.quality.is_finite() {
            return Err("manual face quality must be finite".to_string());
        }
        if input.source_width == 0 || input.source_height == 0 {
            return Err("manual face source dimensions must be non-zero".to_string());
        }
        if !input.alignment_valid && input.embedding.is_some() {
            return Err("invalid manual alignment must not produce an embedding".to_string());
        }
        let source_region = input
            .display_region
            .display_to_source(input.exif_orientation)?;
        let source_index =
            manual_source_index(&input.media_key, &input.media_fingerprint, source_region)?;
        let landmarks = input
            .display_landmarks
            .iter()
            .map(|point| {
                if point.len() != 2
                    || point
                        .iter()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                {
                    return Err(
                        "manual landmarks must contain finite normalized x/y pairs".to_string()
                    );
                }
                let (x, y) = display_point_to_source(point[0], point[1], input.exif_orientation);
                Ok(vec![x, y])
            })
            .collect::<Result<Vec<_>, String>>()?;
        let face_id = manual_face_id(
            &input.media_key,
            &input.media_fingerprint,
            source_index,
            source_region,
        )?;
        let timestamp = now();
        let face = FaceObservation {
            face_id: face_id.clone(),
            media_key: input.media_key,
            media_fingerprint: input.media_fingerprint,
            source_index,
            source_width: Some(input.source_width),
            source_height: Some(input.source_height),
            exif_orientation: Some(input.exif_orientation.code()),
            bounds_normalized: vec![
                source_region.x,
                source_region.y,
                source_region.width,
                source_region.height,
            ],
            landmarks_normalized: landmarks,
            alignment_valid: input.alignment_valid,
            quality: input.quality,
            pose_bucket: input.pose_bucket,
            operator_owned: true,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        validate_face(&face)?;
        let _guard = self.correction_write_guard("manual face")?;
        let execution_before = self.execution_state_unlocked()?;
        if input.expected_schema_generation != MATCH_SCHEMA_GENERATION
            || input.expected_model_generation != self.current_model_generation_unlocked()?
            || input.expected_catalog_revision != execution_before.catalog_revision
        {
            return Err("stale manual-face revision fence".to_string());
        }
        self.validate_manual_media_authority_fence_unlocked(
            &face.media_key,
            &face.media_fingerprint,
            &input.authority,
        )?;
        if self
            .get_one_unlocked::<FaceObservation>(FACE_TABLE, &face_id)?
            .is_some()
        {
            return Err("manual face region already exists".to_string());
        }
        let operation_id = new_id("operation");
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Face,
            stable_id: face_id.clone(),
            before: None,
            after: Some(serde_json::to_value(&face).map_err(|error| error.to_string())?),
        }];
        let embedding_id = if let Some(input_embedding) = input.embedding {
            validate_text(
                "manual embedding model generation",
                &input_embedding.model_generation,
            )?;
            validate_vector(&input_embedding.vector)?;
            let generation: ModelGeneration = self.require_unlocked(
                GENERATION_TABLE,
                &input_embedding.model_generation,
                "model generation",
            )?;
            if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
                return Err("manual embedding generation is not usable".to_string());
            }
            let id = embedding_id(&face_id, &input_embedding.model_generation);
            let embedding = FaceEmbedding {
                embedding_id: id.clone(),
                face_id: face_id.clone(),
                vector: input_embedding.vector,
                model_generation: input_embedding.model_generation,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                media_fingerprint: face.media_fingerprint.clone(),
                face_revision: face.face_revision,
                job_id: operation_id.clone(),
                active: true,
                created_at: timestamp,
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Embedding,
                stable_id: id.clone(),
                before: None,
                after: Some(serde_json::to_value(embedding).map_err(|error| error.to_string())?),
            });
            Some(id)
        } else {
            None
        };
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let operation = self.commit_correction_unlocked(
            &operation_id,
            "manual_face",
            Some(&face_id),
            None,
            deltas,
            vec![face_id.clone()],
            vec![face.media_key.clone()],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(ManualFaceReceipt {
            operation,
            face,
            embedding_id,
        })
    }

    pub fn create_manual_face_and_assign(
        &self,
        input: ManualFaceInput,
        person_id: &str,
        expected_person_revision: u64,
    ) -> Result<ManualFaceReceipt, String> {
        let face = prepare_manual_observation(&input)?;
        let _guard = self.correction_write_guard("manual face assignment")?;
        let mut execution = self.execution_state_unlocked()?;
        if input.expected_schema_generation != MATCH_SCHEMA_GENERATION
            || input.expected_model_generation != self.current_model_generation_unlocked()?
            || input.expected_catalog_revision != execution.catalog_revision
        {
            return Err("stale manual-face revision fence".to_string());
        }
        self.validate_manual_media_authority_fence_unlocked(
            &face.media_key,
            &face.media_fingerprint,
            &input.authority,
        )?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        if person.revision != expected_person_revision {
            return Err("stale manual-face Person revision fence".to_string());
        }
        if self
            .get_one_unlocked::<FaceObservation>(FACE_TABLE, &face.face_id)?
            .is_some()
        {
            return Err("manual face region already exists".to_string());
        }
        let operation_id = new_id("operation");
        let timestamp = now();
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Face,
            stable_id: face.face_id.clone(),
            before: None,
            after: Some(serde_json::to_value(&face).map_err(|error| error.to_string())?),
        }];
        let embedding_id = if let Some(input_embedding) = input.embedding {
            if !face.alignment_valid {
                return Err("invalid manual alignment must not produce an embedding".to_string());
            }
            validate_text(
                "manual embedding model generation",
                &input_embedding.model_generation,
            )?;
            validate_vector(&input_embedding.vector)?;
            let generation: ModelGeneration = self.require_unlocked(
                GENERATION_TABLE,
                &input_embedding.model_generation,
                "model generation",
            )?;
            if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
                return Err("manual embedding generation is not usable".to_string());
            }
            let id = embedding_id(&face.face_id, &input_embedding.model_generation);
            let embedding = FaceEmbedding {
                embedding_id: id.clone(),
                face_id: face.face_id.clone(),
                vector: input_embedding.vector,
                model_generation: input_embedding.model_generation,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                media_fingerprint: face.media_fingerprint.clone(),
                face_revision: face.face_revision,
                job_id: operation_id.clone(),
                active: true,
                created_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Embedding,
                stable_id: id.clone(),
                before: None,
                after: Some(serde_json::to_value(embedding).map_err(|error| error.to_string())?),
            });
            Some(id)
        } else {
            None
        };
        let assignment = Assignment {
            assignment_id: face.face_id.clone(),
            face_id: face.face_id.clone(),
            person_id: person_id.to_string(),
            media_key: face.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "manual_face".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: operation_id.clone(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        deltas.push(CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: face.face_id.clone(),
            before: None,
            after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
        });
        self.bump_revisions(&mut execution, true, false)?;
        let operation = self.commit_correction_unlocked(
            &operation_id,
            "manual_face_and_assign",
            Some(&face.face_id),
            Some(person_id),
            deltas,
            vec![face.face_id.clone()],
            vec![face.media_key.clone()],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(ManualFaceReceipt {
            operation,
            face,
            embedding_id,
        })
    }

    pub fn preview_remove_person(&self, person_id: &str) -> Result<PersonEditPreview, String> {
        let _guard = self.database_read_guard("Match remove-Person preview lock is poisoned")?;
        self.preview_remove_person_unlocked(person_id)
    }

    pub fn remove_person(&self, preview: &PersonEditPreview) -> Result<CorrectionReceipt, String> {
        let PersonEditKind::Remove { person_id } = &preview.kind else {
            return Err("remove Person requires its exact preview".to_string());
        };
        let _guard = self.correction_write_guard("remove Person")?;
        let current = self.preview_remove_person_unlocked(person_id)?;
        if &current != preview {
            return Err("stale remove-Person preview".to_string());
        }
        ensure_within_delta_limit(preview.required_reversible_rows)?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Person,
            stable_id: person_id.clone(),
            before: Some(serde_json::to_value(person).map_err(|error| error.to_string())?),
            after: None,
        }];
        for assignment in self.person_assignments_streamed_unlocked(person_id)? {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: assignment.assignment_id.clone(),
                before: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
                after: None,
            });
        }
        let looks =
            self.query_typed_by_field_unlocked::<Look>(LOOK_TABLE, "person_id", person_id)?;
        for look in &looks {
            for set in self.query_typed_by_field_unlocked::<TrustedTemplateSet>(
                TEMPLATE_SET_TABLE,
                "look_id",
                &look.look_id,
            )? {
                for member in self.query_typed_by_field_unlocked::<TrustedTemplateMembership>(
                    TRUSTED_MEMBER_TABLE,
                    "set_id",
                    &set.set_id,
                )? {
                    let membership_id = member.membership_id.clone();
                    let search = self.get_value_unlocked(TRUSTED_SEARCH_TABLE, &membership_id)?;
                    deltas.push(CorrectionRowDelta {
                        table: CorrectionTable::TrustedMember,
                        stable_id: membership_id.clone(),
                        before: Some(
                            serde_json::to_value(member).map_err(|error| error.to_string())?,
                        ),
                        after: None,
                    });
                    if let Some(search) = search {
                        deltas.push(CorrectionRowDelta {
                            table: CorrectionTable::TrustedSearch,
                            stable_id: membership_id,
                            before: Some(search),
                            after: None,
                        });
                    }
                }
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::TemplateSet,
                    stable_id: set.set_id.clone(),
                    before: Some(serde_json::to_value(set).map_err(|error| error.to_string())?),
                    after: None,
                });
            }
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Look,
                stable_id: look.look_id.clone(),
                before: Some(serde_json::to_value(look).map_err(|error| error.to_string())?),
                after: None,
            });
        }
        for constraint in self.query_typed_by_field_unlocked::<CannotLinkConstraint>(
            CONSTRAINT_TABLE,
            "person_id",
            person_id,
        )? {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: constraint.constraint_id.clone(),
                before: Some(serde_json::to_value(constraint).map_err(|error| error.to_string())?),
                after: None,
            });
        }
        self.append_person_suggestion_removal_deltas_unlocked(&mut deltas, person_id)?;
        require_exact_preview_delta_count(preview, deltas.len())?;
        let operation_id = new_id("operation");
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, true)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "remove_person",
            None,
            Some(person_id),
            deltas,
            preview.face_ids.clone(),
            preview.media_keys.clone(),
            true,
            true,
            execution,
        )?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(receipt)
    }

    pub fn preview_merge_people(
        &self,
        source_person_id: &str,
        target_person_id: &str,
    ) -> Result<PersonEditPreview, String> {
        if source_person_id == target_person_id {
            return Err("merge source and target Person must differ".to_string());
        }
        let _guard = self.database_read_guard("Match merge preview lock is poisoned")?;
        self.preview_merge_people_unlocked(source_person_id, target_person_id)
    }

    pub fn merge_people(&self, preview: &PersonEditPreview) -> Result<CorrectionReceipt, String> {
        let PersonEditKind::Merge {
            source_person_id,
            target_person_id,
        } = &preview.kind
        else {
            return Err("merge requires a merge preview".to_string());
        };
        let _guard = self.correction_write_guard("merge People")?;
        let current = self.preview_merge_people_unlocked(source_person_id, target_person_id)?;
        if &current != preview {
            return Err("stale merge preview".to_string());
        }
        ensure_within_delta_limit(preview.required_reversible_rows)?;
        let source: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let mut target: Person = self.require_unlocked(PERSON_TABLE, target_person_id, "Person")?;
        let operation_id = new_id("operation");
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, true)?;
        let mut aliases = target.aliases.clone();
        aliases.push(source.name.clone());
        aliases.extend(source.aliases.clone());
        aliases.sort_by_key(|value| value.to_lowercase());
        aliases.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        target.aliases = aliases;
        target.revision = target
            .revision
            .checked_add(1)
            .ok_or("Person revision overflow")?;
        target.catalog_revision = execution.catalog_revision;
        target.updated_at = now();
        let mut deltas = vec![
            CorrectionRowDelta {
                table: CorrectionTable::Person,
                stable_id: source.person_id.clone(),
                before: Some(serde_json::to_value(&source).map_err(|error| error.to_string())?),
                after: None,
            },
            CorrectionRowDelta {
                table: CorrectionTable::Person,
                stable_id: target.person_id.clone(),
                before: Some(
                    serde_json::to_value(self.require_unlocked::<Person>(
                        PERSON_TABLE,
                        target_person_id,
                        "Person",
                    )?)
                    .map_err(|error| error.to_string())?,
                ),
                after: Some(serde_json::to_value(&target).map_err(|error| error.to_string())?),
            },
        ];
        for mut look in
            self.query_typed_by_field_unlocked::<Look>(LOOK_TABLE, "person_id", source_person_id)?
        {
            let before = serde_json::to_value(&look).map_err(|error| error.to_string())?;
            look.person_id = target_person_id.to_string();
            look.revision = look
                .revision
                .checked_add(1)
                .ok_or("Look revision overflow")?;
            look.updated_at = now();
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Look,
                stable_id: look.look_id.clone(),
                before: Some(before),
                after: Some(serde_json::to_value(look).map_err(|error| error.to_string())?),
            });
        }
        for mut assignment in self.person_assignments_streamed_unlocked(source_person_id)? {
            let assignment_id = assignment.assignment_id.clone();
            let before = serde_json::to_value(&assignment).map_err(|error| error.to_string())?;
            let after = match assignment.state.as_str() {
                state if state == AssignmentState::OperatorConfirmed.as_str() => {
                    if self
                        .get_one_unlocked::<CannotLinkConstraint>(
                            CONSTRAINT_TABLE,
                            &cannot_link_id(&assignment.face_id, target_person_id),
                        )?
                        .is_some()
                    {
                        return Err(format!(
                            "face {} is cannot-linked to the merge target",
                            assignment.face_id
                        ));
                    }
                    assignment.person_id = target_person_id.to_string();
                    assignment.person_revision = target.revision;
                    assignment.operation_id = operation_id.clone();
                    assignment.updated_at = now();
                    Some(serde_json::to_value(&assignment).map_err(|error| error.to_string())?)
                }
                state if state == AssignmentState::CommittedStrictAutomatic.as_str() => None,
                state => {
                    return Err(format!(
                        "merge source Assignment {} has unsupported state {state}",
                        assignment.assignment_id
                    ));
                }
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: assignment_id,
                before: Some(before),
                after,
            });
        }
        self.visit_merge_target_assignments_unlocked(target_person_id, |assignment| {
            let before = serde_json::to_value(assignment).map_err(|error| error.to_string())?;
            let after = if assignment.state == AssignmentState::OperatorConfirmed.as_str() {
                let mut assignment = assignment.clone();
                assignment.person_revision = target.revision;
                assignment.operation_id = operation_id.clone();
                assignment.updated_at = now();
                Some(serde_json::to_value(&assignment).map_err(|error| error.to_string())?)
            } else {
                None
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: assignment.assignment_id.clone(),
                before: Some(before),
                after,
            });
            Ok(())
        })?;
        for mut search in self.query_typed_by_field_unlocked::<TrustedSearchEmbedding>(
            TRUSTED_SEARCH_TABLE,
            "person_id",
            source_person_id,
        )? {
            let before = serde_json::to_value(&search).map_err(|error| error.to_string())?;
            search.person_id = target_person_id.to_string();
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::TrustedSearch,
                stable_id: search.membership_id.clone(),
                before: Some(before),
                after: Some(serde_json::to_value(search).map_err(|error| error.to_string())?),
            });
        }
        for constraint in self.query_typed_by_field_unlocked::<CannotLinkConstraint>(
            CONSTRAINT_TABLE,
            "person_id",
            source_person_id,
        )? {
            let target_id = cannot_link_id(&constraint.face_id, target_person_id);
            let existing_target = self.get_value_unlocked(CONSTRAINT_TABLE, &target_id)?;
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: constraint.constraint_id.clone(),
                before: Some(serde_json::to_value(&constraint).map_err(|error| error.to_string())?),
                after: None,
            });
            if existing_target.is_none() {
                let target_constraint = CannotLinkConstraint {
                    constraint_id: target_id.clone(),
                    face_id: constraint.face_id,
                    person_id: target_person_id.to_string(),
                    operation_id: operation_id.clone(),
                    operator_owned: constraint.operator_owned,
                    created_at: now(),
                };
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::Constraint,
                    stable_id: target_id,
                    before: None,
                    after: Some(
                        serde_json::to_value(target_constraint)
                            .map_err(|error| error.to_string())?,
                    ),
                });
            }
        }
        self.append_person_suggestion_removal_deltas_unlocked(&mut deltas, source_person_id)?;
        self.append_person_suggestion_removal_deltas_unlocked(&mut deltas, target_person_id)?;
        require_exact_preview_delta_count(preview, deltas.len())?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "merge_people",
            None,
            Some(target_person_id),
            deltas,
            preview.face_ids.clone(),
            preview.media_keys.clone(),
            true,
            true,
            execution,
        )?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(receipt)
    }

    pub fn preview_split_person(
        &self,
        source_person_id: &str,
        face_ids: Vec<String>,
        destination_name: &str,
    ) -> Result<PersonEditPreview, String> {
        validate_text("split destination Person name", destination_name)?;
        let _guard = self.database_read_guard("Match split preview lock is poisoned")?;
        self.preview_split_person_unlocked(source_person_id, face_ids, destination_name)
    }

    pub fn split_person(&self, preview: &PersonEditPreview) -> Result<CorrectionReceipt, String> {
        let PersonEditKind::Split {
            source_person_id,
            destination_person_id,
            destination_name,
        } = &preview.kind
        else {
            return Err("split requires a split preview".to_string());
        };
        let _guard = self.correction_write_guard("split Person")?;
        let current = self.preview_split_person_unlocked(
            source_person_id,
            preview.face_ids.clone(),
            destination_name,
        )?;
        if &current != preview || &current.person_ids[1] != destination_person_id {
            return Err("stale split preview".to_string());
        }
        ensure_within_delta_limit(preview.required_reversible_rows)?;
        let operation_id = new_id("operation");
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, true)?;
        let timestamp = now();
        let destination = Person {
            person_id: destination_person_id.clone(),
            name: destination_name.clone(),
            aliases: Vec::new(),
            cover_media_key: preview.media_keys.first().cloned(),
            hidden: false,
            favorite: false,
            revision: 1,
            catalog_revision: execution.catalog_revision,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Person,
            stable_id: destination_person_id.clone(),
            before: None,
            after: Some(serde_json::to_value(&destination).map_err(|error| error.to_string())?),
        }];
        for face_id in &preview.face_ids {
            let mut assignment: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            if assignment.person_id != *source_person_id {
                return Err("split face no longer belongs to the source Person".to_string());
            }
            let before = serde_json::to_value(&assignment).map_err(|error| error.to_string())?;
            assignment.person_id = destination_person_id.clone();
            assignment.person_revision = destination.revision;
            assignment.look_id = None;
            assignment.placement = "unsorted".to_string();
            assignment.operation_id = operation_id.clone();
            assignment.updated_at = now();
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.clone(),
                before: Some(before),
                after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        }
        require_exact_preview_delta_count(preview, deltas.len())?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "split_person",
            None,
            Some(destination_person_id),
            deltas,
            preview.face_ids.clone(),
            preview.media_keys.clone(),
            true,
            true,
            execution,
        )?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(receipt)
    }

    pub fn preview_split_to_person(
        &self,
        source_person_id: &str,
        target_person_id: &str,
        mut face_ids: Vec<String>,
    ) -> Result<PersonEditPreview, String> {
        if source_person_id == target_person_id {
            return Err("split source and target Person must differ".to_string());
        }
        if face_ids.is_empty() || face_ids.len() > BATCH_CORRECTION_FACE_LIMIT {
            return Err(format!(
                "split must select between 1 and {BATCH_CORRECTION_FACE_LIMIT} faces"
            ));
        }
        face_ids.sort();
        if face_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("split selection FaceIds must be unique".to_string());
        }
        let _guard = self.database_read_guard("Match split-to-Person preview lock is poisoned")?;
        self.preview_split_to_person_unlocked(source_person_id, target_person_id, face_ids)
    }

    pub fn split_to_person(
        &self,
        preview: &PersonEditPreview,
        fences: Vec<CorrectionFence>,
    ) -> Result<CorrectionReceipt, String> {
        let PersonEditKind::SplitToPerson {
            source_person_id,
            target_person_id,
        } = &preview.kind
        else {
            return Err("split-to-Person requires its exact preview".to_string());
        };
        let _guard = self.correction_write_guard("split to Person")?;
        validate_batch_selection(&preview.face_ids, &fences)?;
        // Bind the operation to the complete source-Person assignment
        // inventory before diagnosing any selected-face fence. An unrelated
        // late assignment invalidates the whole split preview even when every
        // selected face is otherwise unchanged. Both checks remain inside the
        // same correction write lock.
        let current = self.preview_split_to_person_unlocked(
            source_person_id,
            target_person_id,
            preview.face_ids.clone(),
        )?;
        if &current != preview {
            return Err("stale split-to-Person preview".to_string());
        }
        ensure_within_delta_limit(preview.required_reversible_rows)?;
        for fence in &fences {
            if fence.person_revisions != preview.person_revisions {
                return Err(
                    "split-to-Person fence must bind the exact preview Person revisions"
                        .to_string(),
                );
            }
            self.validate_correction_fence_unlocked(fence, Some(target_person_id))?;
        }
        let target: Person = self.require_unlocked(PERSON_TABLE, target_person_id, "Person")?;
        let operation_id = new_id("operation");
        let mut deltas = Vec::new();
        for face_id in &preview.face_ids {
            let mut assignment: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            let before = serde_json::to_value(&assignment).map_err(|error| error.to_string())?;
            assignment.person_id = target_person_id.clone();
            assignment.person_revision = target.revision;
            assignment.look_id = None;
            assignment.placement = "unsorted".to_string();
            assignment.state = AssignmentState::OperatorConfirmed.as_str().to_string();
            assignment.provenance = "split_to_person".to_string();
            assignment.model_generation = None;
            assignment.calibration_generation = None;
            assignment.envelope_hash = None;
            assignment.locked = true;
            assignment.operation_id = operation_id.clone();
            assignment.updated_at = now();
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.clone(),
                before: Some(before),
                after: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
            });
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        }
        require_exact_preview_delta_count(preview, deltas.len())?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            "split_to_person",
            None,
            Some(target_person_id),
            deltas,
            preview.face_ids.clone(),
            preview.media_keys.clone(),
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    fn undo_dependency_values_unlocked(
        &self,
        envelope: &CorrectionDeltaEnvelope,
        table: CorrectionTable,
        field: &str,
        value: &str,
    ) -> Result<Vec<Value>, String> {
        let id_field = match (&table, field) {
            (CorrectionTable::Assignment, "person_id" | "look_id" | "face_id") => "assignment_id",
            (CorrectionTable::Look, "person_id") => "look_id",
            (CorrectionTable::TemplateSet, "look_id") => "set_id",
            (CorrectionTable::Constraint, "person_id" | "face_id") => "constraint_id",
            (CorrectionTable::Suggestion, "candidate_person_id" | "face_id") => "suggestion_id",
            (CorrectionTable::Disposition, "face_id") => "face_id",
            (CorrectionTable::Embedding, "face_id") => "embedding_id",
            (CorrectionTable::TrustedMember, "look_id" | "face_id") => "membership_id",
            (CorrectionTable::TrustedSearch, "person_id" | "look_id" | "face_id") => {
                "membership_id"
            }
            _ => return Err("unsupported undo dependency selector".to_string()),
        };
        let db = self.database();
        let table_name = table.name();
        let sql = format!(
            "SELECT * OMIT id FROM {table_name} WHERE {field} = $value ORDER BY {id_field} ASC LIMIT {};",
            CORRECTION_ROW_LIMIT + 1
        );
        let bound_value = value.to_string();
        let current: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("value", bound_value))
                .await
                .map_err(|error| format!("query undo dependencies: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode undo dependencies: {error}"))
        })?;
        if current.len() > CORRECTION_ROW_LIMIT {
            return Err(format!(
                "correction undo dependency selection exceeds the bounded {CORRECTION_ROW_LIMIT}-row limit"
            ));
        }
        let mut by_id = BTreeMap::<String, Value>::new();
        for row in current {
            let stable_id = row
                .get(id_field)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{table_name} undo dependency lacks {id_field}"))?
                .to_string();
            by_id.insert(stable_id, row);
        }
        for delta in &envelope.rows {
            if delta.table == table
                && delta
                    .before
                    .as_ref()
                    .or(delta.after.as_ref())
                    .and_then(|row| row.get(field))
                    .and_then(Value::as_str)
                    .is_some_and(|candidate| candidate == value)
            {
                by_id.entry(delta.stable_id.clone()).or_insert(Value::Null);
            }
        }
        let mut projected = Vec::new();
        for (stable_id, current) in by_id {
            let value_after_undo = envelope
                .rows
                .iter()
                .find(|delta| delta.table == table && delta.stable_id == stable_id)
                .map(|delta| delta.before.clone())
                .unwrap_or_else(|| (current != Value::Null).then_some(current));
            if let Some(row) = value_after_undo.filter(|row| {
                row.get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|candidate| candidate == value)
            }) {
                projected.push(row);
            }
        }
        Ok(projected)
    }

    fn undo_projected_value_unlocked(
        &self,
        envelope: &CorrectionDeltaEnvelope,
        table: CorrectionTable,
        stable_id: &str,
    ) -> Result<Option<Value>, String> {
        if let Some(delta) = envelope
            .rows
            .iter()
            .find(|delta| delta.table == table && delta.stable_id == stable_id)
        {
            Ok(delta.before.clone())
        } else {
            self.get_value_unlocked(table.name(), stable_id)
        }
    }

    fn validate_undo_dependency_topology_unlocked(
        &self,
        envelope: &CorrectionDeltaEnvelope,
    ) -> Result<(), String> {
        for row in envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Person && row.before.is_none())
        {
            for (table, field) in [
                (CorrectionTable::Assignment, "person_id"),
                (CorrectionTable::Look, "person_id"),
                (CorrectionTable::TrustedSearch, "person_id"),
                (CorrectionTable::Constraint, "person_id"),
                (CorrectionTable::Suggestion, "candidate_person_id"),
            ] {
                if !self
                    .undo_dependency_values_unlocked(envelope, table, field, &row.stable_id)?
                    .is_empty()
                {
                    return Err(format!(
                        "correction undo dependency conflict: Person {} would be deleted while durable {field} references remain",
                        row.stable_id
                    ));
                }
            }
        }

        for row in envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Face && row.before.is_none())
        {
            for table in [
                CorrectionTable::Assignment,
                CorrectionTable::Constraint,
                CorrectionTable::Suggestion,
                CorrectionTable::Disposition,
                CorrectionTable::Embedding,
                CorrectionTable::TrustedMember,
                CorrectionTable::TrustedSearch,
            ] {
                if !self
                    .undo_dependency_values_unlocked(
                        envelope,
                        table.clone(),
                        "face_id",
                        &row.stable_id,
                    )?
                    .is_empty()
                {
                    return Err(format!(
                        "correction undo dependency conflict: Face {} would be deleted while {} references remain",
                        row.stable_id,
                        table.name()
                    ));
                }
            }
        }

        for row in envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Look && row.before != row.after)
        {
            let assignments = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::Assignment,
                "look_id",
                &row.stable_id,
            )?;
            let searches = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::TrustedSearch,
                "look_id",
                &row.stable_id,
            )?;
            let members = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::TrustedMember,
                "look_id",
                &row.stable_id,
            )?;
            let template_sets = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::TemplateSet,
                "look_id",
                &row.stable_id,
            )?;
            let Some(before) = row.before.as_ref() else {
                if !(assignments.is_empty()
                    && searches.is_empty()
                    && members.is_empty()
                    && template_sets.is_empty())
                {
                    return Err(format!(
                        "correction undo dependency conflict: Look {} would be deleted while durable references remain",
                        row.stable_id
                    ));
                }
                continue;
            };
            let restored: Look = serde_json::from_value(before.clone())
                .map_err(|error| format!("decode restored undo Look: {error}"))?;
            for assignment in assignments {
                let assignment: Assignment = serde_json::from_value(assignment)
                    .map_err(|error| format!("decode undo Look assignment: {error}"))?;
                if assignment.person_id != restored.person_id {
                    return Err(format!(
                        "correction undo dependency conflict: assignment {} would cross Person/Look ownership",
                        assignment.assignment_id
                    ));
                }
            }
            for search in searches {
                let person_id = search
                    .get("person_id")
                    .and_then(Value::as_str)
                    .ok_or("decode undo trusted-search dependency: missing person_id")?;
                let membership_id = search
                    .get("membership_id")
                    .and_then(Value::as_str)
                    .ok_or("decode undo trusted-search dependency: missing membership_id")?;
                if person_id != restored.person_id {
                    return Err(format!(
                        "correction undo dependency conflict: trusted search {} would cross Person/Look ownership",
                        membership_id
                    ));
                }
            }
            for member in members {
                let member: TrustedTemplateMembership = serde_json::from_value(member)
                    .map_err(|error| format!("decode undo trusted-member dependency: {error}"))?;
                let assignment = self
                    .undo_projected_value_unlocked(
                        envelope,
                        CorrectionTable::Assignment,
                        &member.face_id,
                    )?
                    .ok_or_else(|| {
                        format!(
                            "correction undo dependency conflict: trusted member {} would lack its assignment",
                            member.membership_id
                        )
                    })?;
                let assignment: Assignment = serde_json::from_value(assignment)
                    .map_err(|error| format!("decode undo trusted-member assignment: {error}"))?;
                if assignment.person_id != restored.person_id
                    || assignment.look_id.as_deref() != Some(restored.look_id.as_str())
                {
                    return Err(format!(
                        "correction undo dependency conflict: trusted member {} would cross Person/Look ownership",
                        member.membership_id
                    ));
                }
            }
        }

        for row in envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Assignment && row.before != row.after)
        {
            let members = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::TrustedMember,
                "face_id",
                &row.stable_id,
            )?;
            let searches = self.undo_dependency_values_unlocked(
                envelope,
                CorrectionTable::TrustedSearch,
                "face_id",
                &row.stable_id,
            )?;
            if members.is_empty() && searches.is_empty() {
                continue;
            }
            let Some(before) = row.before.as_ref() else {
                return Err(format!(
                    "correction undo dependency conflict: Face {} would lose its assignment while trusted dependencies remain",
                    row.stable_id
                ));
            };
            let assignment: Assignment = serde_json::from_value(before.clone())
                .map_err(|error| format!("decode restored undo Assignment: {error}"))?;
            for member in members {
                let member: TrustedTemplateMembership = serde_json::from_value(member)
                    .map_err(|error| format!("decode undo trusted-member dependency: {error}"))?;
                if assignment.look_id.as_deref() != Some(member.look_id.as_str()) {
                    return Err(format!(
                        "correction undo dependency conflict: trusted member {} would no longer match its assignment",
                        member.membership_id
                    ));
                }
            }
            for search in searches {
                let person_id = search
                    .get("person_id")
                    .and_then(Value::as_str)
                    .ok_or("decode undo trusted-search dependency: missing person_id")?;
                let look_id = search
                    .get("look_id")
                    .and_then(Value::as_str)
                    .ok_or("decode undo trusted-search dependency: missing look_id")?;
                let membership_id = search
                    .get("membership_id")
                    .and_then(Value::as_str)
                    .ok_or("decode undo trusted-search dependency: missing membership_id")?;
                if person_id != assignment.person_id
                    || assignment.look_id.as_deref() != Some(look_id)
                {
                    return Err(format!(
                        "correction undo dependency conflict: trusted search {} would no longer match its assignment",
                        membership_id
                    ));
                }
            }
        }
        Ok(())
    }

    /// Restart-safe all-or-nothing undo. Every durable row must still equal the
    /// operation's recorded after-state before any inverse row, revision, or
    /// undo marker is committed.
    pub fn undo_correction(&self, operation_id: &str) -> Result<CorrectionReceipt, String> {
        validate_text("correction operation id", operation_id)?;
        let _guard = self.correction_write_guard("undo correction")?;
        let operation: MatchOperation =
            self.require_unlocked(OPERATION_TABLE, operation_id, "MatchOperation")?;
        if !operation.reversible {
            return Err("Match operation is not reversible".to_string());
        }
        let envelope: CorrectionDeltaEnvelope = serde_json::from_str(&operation.after_json)
            .map_err(|error| format!("decode typed correction delta: {error}"))?;
        if envelope.version != CORRECTION_DELTA_VERSION
            || envelope.rows.len() > CORRECTION_ROW_LIMIT
        {
            return Err("unsupported or unbounded correction delta".to_string());
        }
        let undo_id = format!("undo-{operation_id}");
        if self
            .get_one_unlocked::<MatchOperation>(OPERATION_TABLE, &undo_id)?
            .is_some()
        {
            return Err("correction operation was already undone".to_string());
        }
        let mut current_values = Vec::with_capacity(envelope.rows.len());
        let mut conflicts = 0usize;
        for row in &envelope.rows {
            let current = self.get_value_unlocked(row.table.name(), &row.stable_id)?;
            let matches_after = if correction_row_omits_portable_vector(row) {
                match (&current, &row.after) {
                    (None, _) => true,
                    (Some(current), Some(after)) => {
                        portable_derived_row_value(&row.table, current)? == *after
                    }
                    (Some(_), None) => false,
                }
            } else {
                current == row.after
            };
            if !matches_after {
                conflicts = conflicts
                    .checked_add(1)
                    .ok_or("undo conflict count overflow")?;
            }
            current_values.push(current);
        }
        if conflicts != 0 {
            return Err(format!(
                "correction undo conflict: {conflicts} row(s) no longer match the operation after-state"
            ));
        }
        self.validate_undo_dependency_topology_unlocked(&envelope)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(
            &mut execution,
            envelope.identity_changed,
            envelope.catalog_changed,
        )?;
        let mut face_revisions = BTreeMap::new();
        let mut person_revisions = BTreeMap::new();
        for (row, current) in envelope.rows.iter().zip(&current_values) {
            match row.table {
                CorrectionTable::Face => {
                    let source = current.as_ref().or(row.before.as_ref());
                    if let Some(source) = source {
                        let face: FaceObservation = serde_json::from_value(source.clone())
                            .map_err(|error| format!("decode planned undo Face: {error}"))?;
                        face_revisions.insert(
                            face.face_id,
                            face.face_revision
                                .checked_add(1)
                                .ok_or("face revision overflow")?,
                        );
                    }
                }
                CorrectionTable::Person => {
                    let source = current.as_ref().or(row.before.as_ref());
                    if let Some(source) = source {
                        let person: Person = serde_json::from_value(source.clone())
                            .map_err(|error| format!("decode planned undo Person: {error}"))?;
                        person_revisions.insert(
                            person.person_id,
                            person
                                .revision
                                .checked_add(1)
                                .ok_or("Person revision overflow")?,
                        );
                    }
                }
                _ => {}
            }
        }
        let mut reverted = Vec::new();
        for (row, current) in envelope.rows.iter().zip(current_values) {
            // Portable history intentionally carries no vectors. If the
            // derived row was never regenerated on this installation there is
            // no safe inverse value to materialize and no durable row to
            // delete; the typed effect is therefore excluded from this undo.
            if current.is_none() && correction_row_omits_portable_vector(row) {
                continue;
            }
            reverted.push(CorrectionRowDelta {
                table: row.table.clone(),
                stable_id: row.stable_id.clone(),
                before: current,
                after: self.materialize_undo_value_unlocked(
                    row,
                    &undo_id,
                    &execution,
                    &face_revisions,
                    &person_revisions,
                )?,
            });
        }
        let undo_envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: "undo_correction".to_string(),
            rows: reverted.clone(),
            face_ids: envelope.face_ids.clone(),
            media_keys: envelope.media_keys.clone(),
            identity_changed: envelope.identity_changed,
            catalog_changed: envelope.catalog_changed,
        };
        let undo_after_json =
            serialize_identity_bundle_nested_json(&undo_envelope, "undo after_json")?;
        let undo_operation = MatchOperation {
            operation_id: undo_id.clone(),
            kind: "undo_correction".to_string(),
            face_id: operation.face_id,
            person_id: operation.person_id,
            before_json: serde_json::to_string(&json!({
                "operation_id": operation_id,
                "conflict_rows": 0,
            }))
            .map_err(|error| error.to_string())?,
            after_json: undo_after_json,
            reversible: false,
            created_at: now(),
        };
        let mut owned_upserts = Vec::<(String, String, Value)>::new();
        let mut owned_deletes = Vec::<(String, String)>::new();
        materialize_rows(&reverted, &mut owned_upserts, &mut owned_deletes);
        owned_upserts.push((
            OPERATION_TABLE.to_string(),
            undo_id.clone(),
            serde_json::to_value(&undo_operation).map_err(|error| error.to_string())?,
        ));
        owned_upserts.push((
            EXECUTION_TABLE.to_string(),
            "global".to_string(),
            serde_json::to_value(&execution).map_err(|error| error.to_string())?,
        ));
        for media_key in &envelope.media_keys {
            owned_deletes.push((PROJECTION_TABLE.to_string(), media_key.clone()));
        }
        if envelope.identity_changed {
            owned_deletes.push((TRUSTED_INDEX_BUILD_TABLE.to_string(), "global".to_string()));
        }
        let (calibration_upserts, _) = self.calibration_invalidation_rows_unlocked("wp084_undo")?;
        owned_upserts.extend(calibration_upserts);
        self.commit_owned_unlocked(&owned_upserts, &owned_deletes)?;
        if envelope.identity_changed {
            let _ = self.reconcile_trusted_search_unlocked();
        }
        drop(_guard);
        if envelope.catalog_changed {
            self.refresh_autocomplete_after_committed_catalog_change();
        }
        Ok(CorrectionReceipt {
            operation_id: undo_id,
            kind: "undo_correction".to_string(),
            changed_rows: reverted.len(),
            conflict_rows: 0,
            affected_face_ids: envelope.face_ids,
            affected_media_keys: envelope.media_keys,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    fn batch_face_disposition_correction(
        &self,
        face_ids: Vec<String>,
        fences: Vec<CorrectionFence>,
        kind: FaceDispositionKind,
    ) -> Result<CorrectionReceipt, String> {
        validate_batch_selection(&face_ids, &fences)?;
        let _guard = self.correction_write_guard("batch face disposition")?;
        let operation_id = new_id("operation");
        let timestamp = now();
        let mut deltas = Vec::new();
        let mut media_keys = Vec::with_capacity(face_ids.len());
        for (face_id, fence) in face_ids.iter().zip(&fences) {
            let prior_assignment =
                self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
            let face = self.validate_correction_fence_unlocked(
                fence,
                prior_assignment
                    .as_ref()
                    .map(|assignment| assignment.person_id.as_str()),
            )?;
            let prior_disposition = self.get_value_unlocked(FACE_DISPOSITION_TABLE, face_id)?;
            let disposition = FaceDisposition {
                face_id: face_id.clone(),
                media_key: face.media_key.clone(),
                disposition: kind.as_str().to_string(),
                operation_id: operation_id.clone(),
                face_revision: face.face_revision,
                created_at: prior_disposition
                    .as_ref()
                    .and_then(|value| value.get("created_at"))
                    .and_then(Value::as_str)
                    .unwrap_or(&timestamp)
                    .to_string(),
                updated_at: timestamp.clone(),
            };
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Disposition,
                stable_id: face_id.clone(),
                before: prior_disposition,
                after: Some(serde_json::to_value(disposition).map_err(|error| error.to_string())?),
            });
            if let Some(assignment) = prior_assignment {
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::Assignment,
                    stable_id: face_id.clone(),
                    before: Some(
                        serde_json::to_value(assignment).map_err(|error| error.to_string())?,
                    ),
                    after: None,
                });
            }
            if kind == FaceDispositionKind::NotAFace {
                for (id, value) in self.query_face_values_unlocked(EMBEDDING_TABLE, face_id)? {
                    deltas.push(CorrectionRowDelta {
                        table: CorrectionTable::Embedding,
                        stable_id: id,
                        before: Some(value),
                        after: None,
                    });
                }
            }
            self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
            self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
            media_keys.push(face.media_key);
        }
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt_kind = match kind {
            FaceDispositionKind::Ignored => "batch_ignore_face",
            FaceDispositionKind::NotAFace => "batch_not_a_face",
        };
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            receipt_kind,
            None,
            None,
            deltas,
            face_ids,
            media_keys,
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    fn set_face_disposition(
        &self,
        face_id: &str,
        fence: &CorrectionFence,
        kind: FaceDispositionKind,
    ) -> Result<CorrectionReceipt, String> {
        let _guard = self.correction_write_guard("face disposition")?;
        let face = self.validate_correction_fence_unlocked(fence, None)?;
        let operation_id = new_id("operation");
        let prior_disposition = self.get_value_unlocked(FACE_DISPOSITION_TABLE, face_id)?;
        let prior_assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        let timestamp = now();
        let disposition = FaceDisposition {
            face_id: face_id.to_string(),
            media_key: face.media_key.clone(),
            disposition: kind.as_str().to_string(),
            operation_id: operation_id.clone(),
            face_revision: face.face_revision,
            created_at: prior_disposition
                .as_ref()
                .and_then(|value| value.get("created_at"))
                .and_then(Value::as_str)
                .unwrap_or(&timestamp)
                .to_string(),
            updated_at: timestamp,
        };
        let mut deltas = vec![CorrectionRowDelta {
            table: CorrectionTable::Disposition,
            stable_id: face_id.to_string(),
            before: prior_disposition,
            after: Some(serde_json::to_value(disposition).map_err(|error| error.to_string())?),
        }];
        if let Some(assignment) = &prior_assignment {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: face_id.to_string(),
                before: Some(serde_json::to_value(assignment).map_err(|error| error.to_string())?),
                after: None,
            });
        }
        if kind == FaceDispositionKind::NotAFace {
            for (id, value) in self.query_face_values_unlocked(EMBEDDING_TABLE, face_id)? {
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::Embedding,
                    stable_id: id,
                    before: Some(value),
                    after: None,
                });
            }
        }
        self.append_face_suggestion_removal_deltas_unlocked(&mut deltas, face_id)?;
        self.append_trust_removal_deltas_unlocked(&mut deltas, face_id)?;
        validate_delta_bound(&deltas)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let receipt = self.commit_correction_unlocked(
            &operation_id,
            kind.as_str(),
            Some(face_id),
            prior_assignment
                .as_ref()
                .map(|value| value.person_id.as_str()),
            deltas,
            vec![face_id.to_string()],
            vec![face.media_key],
            true,
            false,
            execution,
        )?;
        drop(_guard);
        Ok(receipt)
    }

    fn correction_write_guard(
        &self,
        label: &str,
    ) -> Result<database::MatchMutationGuard<'_>, String> {
        self.mutation_write_guard(&format!("Match {label}"))
    }

    fn validate_rejection_trigger_unlocked(
        &self,
        trigger: RejectionTrigger,
        face_id: &str,
        person_id: &str,
        assignment: Option<&Assignment>,
    ) -> Result<bool, String> {
        let matching_assignment = assignment.filter(|assignment| assignment.person_id == person_id);
        match trigger {
            RejectionTrigger::Different => {
                if let Some(assignment) = matching_assignment {
                    if assignment.state != AssignmentState::CommittedStrictAutomatic.as_str() {
                        return Err(
                            "Different requires a matching current suggestion or committed_strict_automatic assignment"
                                .to_string(),
                        );
                    }
                    self.require_review_candidate_unlocked(
                        trigger.label(),
                        face_id,
                        person_id,
                        Some(assignment),
                    )?;
                    Ok(true)
                } else {
                    self.require_review_candidate_unlocked(
                        trigger.label(),
                        face_id,
                        person_id,
                        None,
                    )?;
                    Ok(false)
                }
            }
            RejectionTrigger::ThisIsNot => {
                let Some(assignment) = matching_assignment else {
                    return Err("This-is-not requires a matching committed assignment".to_string());
                };
                match assignment.state.as_str() {
                    state if state == AssignmentState::CommittedStrictAutomatic.as_str() => {
                        self.require_review_candidate_unlocked(
                            trigger.label(),
                            face_id,
                            person_id,
                            Some(assignment),
                        )?;
                    }
                    state if state == AssignmentState::OperatorConfirmed.as_str() => {}
                    _ => {
                        return Err(
                            "This-is-not requires a matching committed assignment".to_string()
                        );
                    }
                }
                Ok(true)
            }
        }
    }

    /// Review actions are only meaningful for an unresolved candidate. A
    /// matching suggestion is unresolved, as is a matching strict-automatic
    /// assignment awaiting operator review. Operator-confirmed truth is not a
    /// candidate and must not be silently rewritten into a new correction.
    fn require_review_candidate_unlocked(
        &self,
        action: &str,
        face_id: &str,
        person_id: &str,
        assignment: Option<&Assignment>,
    ) -> Result<(), String> {
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let active_generation = self.active_model_generation_unlocked()?;
        let active_calibration =
            self.current_active_calibration_unlocked(active_generation.as_deref())?;
        let canonical_asset = self.canonical_job_asset_for_media_unlocked(&face.media_key)?;
        if let Some(assignment) = assignment {
            if assignment.person_id != person_id {
                return Err(format!(
                    "{action} cannot target a Person different from the committed assignment"
                ));
            }
            if assignment.state == AssignmentState::CommittedStrictAutomatic.as_str() {
                let embedding = assignment
                    .model_generation
                    .as_deref()
                    .map(|generation| {
                        self.get_one_unlocked::<FaceEmbedding>(
                            EMBEDDING_TABLE,
                            &embedding_id(&assignment.face_id, generation),
                        )
                    })
                    .transpose()?
                    .flatten();
                let embedding = embedding.as_ref().map(FaceEmbeddingProvenance::from);
                return if self.strict_assignment_provenance_is_current_unlocked(
                    assignment,
                    &face,
                    &person,
                    active_generation.as_deref(),
                    active_calibration.as_ref(),
                    canonical_asset.as_ref(),
                    embedding.as_ref(),
                ) {
                    Ok(())
                } else {
                    Err(format!(
                        "{action} rejects stale committed_strict_automatic provenance"
                    ))
                };
            }
            if assignment.state == AssignmentState::OperatorConfirmed.as_str() {
                return Err(format!(
                    "{action} rejects an already operator-confirmed assignment"
                ));
            }
            return Err(format!(
                "{action} rejects assignment state {}",
                assignment.state
            ));
        }
        let Some(suggestion) = self
            .get_one_unlocked::<Suggestion>(SUGGESTION_TABLE, &suggestion_id(face_id, person_id))?
        else {
            return Err(format!(
                "{action} requires a matching suggestion or same-Person committed_strict_automatic assignment"
            ));
        };
        let embedding_key = embedding_id(&suggestion.face_id, &suggestion.model_generation);
        let embedding = self.get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &embedding_key)?;
        let embedding = embedding.as_ref().map(FaceEmbeddingProvenance::from);
        if self.suggestion_provenance_is_current_unlocked(
            &suggestion,
            &face,
            &person,
            active_generation.as_deref(),
            canonical_asset.as_ref(),
            embedding.as_ref(),
        ) {
            Ok(())
        } else {
            Err(format!("{action} rejects stale suggestion provenance"))
        }
    }

    fn validate_correction_fence_unlocked(
        &self,
        fence: &CorrectionFence,
        required_person_id: Option<&str>,
    ) -> Result<FaceObservation, String> {
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, &fence.face_id, "FaceObservation")?;
        let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, &fence.face_id)?;
        let execution = self.execution_state_unlocked()?;
        if face.face_revision != fence.face_revision
            || face.media_key != fence.media_key
            || face.media_fingerprint != fence.media_fingerprint
            || fence.schema_generation != MATCH_SCHEMA_GENERATION
            || fence.model_generation != self.current_model_generation_unlocked()?
            || assignment.as_ref().map(|value| value.operation_id.clone())
                != fence.assignment_operation_id
            || execution.identity_revision != fence.identity_revision
            || execution.catalog_revision != fence.catalog_revision
        {
            return Err("stale correction revision fence".to_string());
        }
        for (person_id, expected_revision) in &fence.person_revisions {
            let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
            if person.revision != *expected_revision {
                return Err("stale correction Person revision fence".to_string());
            }
        }
        if let Some(person_id) = required_person_id {
            let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
            if fence.person_revisions.get(person_id) != Some(&person.revision) {
                return Err("correction fence does not bind the requested Person".to_string());
            }
        }
        Ok(face)
    }

    fn current_model_generation_unlocked(&self) -> Result<String, String> {
        let db = self.database();
        let rows: Vec<ModelGeneration> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_model_generation WHERE state = 'active' ORDER BY generation ASC LIMIT 2;",
                )
                .await
                .map_err(|error| format!("query active Match model generation: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode active Match model generation: {error}"))
        })?;
        if rows.len() > 1 {
            return Err("multiple active Match model generations are invalid".to_string());
        }
        Ok(rows
            .into_iter()
            .next()
            .map(|generation| generation.generation)
            .unwrap_or_else(|| UNCONFIGURED_MODEL_GENERATION.to_string()))
    }

    pub(super) fn suggestion_source_provenance_unlocked(
        &self,
        operation_id: &str,
        operation_kind: &str,
        operation_created_at: &str,
        rows: &[CorrectionRowDelta],
    ) -> Result<Vec<SuggestionSourceProvenance>, String> {
        if !matches!(
            operation_kind,
            "same"
                | "batch_same"
                | "different"
                | "batch_different"
                | "change_person"
                | "batch_change_person"
        ) {
            return Ok(Vec::new());
        }
        let mut provenance = Vec::new();
        for row in rows.iter().filter(|row| {
            row.table == CorrectionTable::Suggestion && row.before.is_some() && row.after.is_none()
        }) {
            let suggestion: Suggestion = serde_json::from_value(row.before.clone().unwrap())
                .map_err(|error| format!("decode correction Suggestion provenance: {error}"))?;
            if suggestion.suggestion_id != row.stable_id {
                return Err("correction Suggestion provenance stable ID mismatch".to_string());
            }
            if !suggestion_is_provenance_source(operation_kind, rows, &suggestion) {
                continue;
            }
            let face: FaceObservation =
                self.require_unlocked(FACE_TABLE, &suggestion.face_id, "FaceObservation")?;
            let person: Person = self.require_unlocked(
                PERSON_TABLE,
                &suggestion.candidate_person_id,
                "Suggestion Person",
            )?;
            let embedding_key = embedding_id(&suggestion.face_id, &suggestion.model_generation);
            let embedding: FaceEmbedding =
                self.require_unlocked(EMBEDDING_TABLE, &embedding_key, "FaceEmbedding")?;
            let generation: ModelGeneration = self.require_unlocked(
                GENERATION_TABLE,
                &suggestion.model_generation,
                "Suggestion ModelGeneration",
            )?;
            let embedding_created_at = chrono::DateTime::parse_from_rfc3339(&embedding.created_at)
                .map_err(|error| format!("invalid FaceEmbedding provenance time: {error}"))?;
            let suggestion_created_at =
                chrono::DateTime::parse_from_rfc3339(&suggestion.created_at)
                    .map_err(|error| format!("invalid Suggestion provenance time: {error}"))?;
            let operation_created_at_parsed =
                chrono::DateTime::parse_from_rfc3339(operation_created_at)
                    .map_err(|error| format!("invalid correction provenance time: {error}"))?;
            if face.face_revision != suggestion.face_revision
                || face.media_fingerprint != suggestion.media_fingerprint
                || person.revision != suggestion.person_revision
                || suggestion.suggestion_id
                    != suggestion_id(&suggestion.face_id, &suggestion.candidate_person_id)
                || suggestion.face_revision == 0
                || suggestion.person_revision == 0
                || !suggestion.similarity.is_finite()
                || suggestion.job_id.trim().is_empty()
                || embedding.face_id != suggestion.face_id
                || embedding.embedding_id != embedding_key
                || embedding.face_revision != suggestion.face_revision
                || embedding.media_fingerprint != suggestion.media_fingerprint
                || embedding.model_generation != suggestion.model_generation
                || embedding.job_id != suggestion.job_id
                || embedding.schema_generation != face.schema_generation
                || !embedding.active
                || !generation.validated
                || !matches!(generation.state.as_str(), "usable" | "active")
                || embedding_created_at > suggestion_created_at
                || suggestion_created_at > operation_created_at_parsed
                || matches!(
                    (
                        suggestion.calibration_generation.as_deref(),
                        suggestion.envelope_hash.as_deref()
                    ),
                    (Some(_), None) | (None, Some(_))
                )
            {
                return Err(format!(
                    "Suggestion {} lacks exact live typed provenance",
                    suggestion.suggestion_id
                ));
            }
            let mut row = SuggestionSourceProvenance {
                provenance_id: String::new(),
                operation_id: operation_id.to_string(),
                operation_kind: operation_kind.to_string(),
                suggestion_id: suggestion.suggestion_id,
                face_id: suggestion.face_id,
                candidate_person_id: suggestion.candidate_person_id,
                media_key: face.media_key,
                media_fingerprint: suggestion.media_fingerprint,
                face_revision: suggestion.face_revision,
                person_revision: suggestion.person_revision,
                model_generation: suggestion.model_generation,
                job_id: suggestion.job_id,
                suggestion_created_at: suggestion.created_at,
                similarity_bits: suggestion.similarity.to_bits(),
                calibration_generation: suggestion.calibration_generation,
                envelope_hash: suggestion.envelope_hash,
                embedding_id: embedding.embedding_id,
                embedding_created_at: embedding.created_at,
                schema_generation: embedding.schema_generation,
                operation_created_at: operation_created_at.to_string(),
            };
            row.provenance_id = suggestion_source_provenance_id(&row);
            provenance.push(row);
        }
        provenance.sort_by(|left, right| left.provenance_id.cmp(&right.provenance_id));
        Ok(provenance)
    }

    fn correction_media_fingerprint_map_unlocked(
        &self,
        media_keys: &[String],
        face_ids: &[String],
        rows: &[CorrectionRowDelta],
        suggestion_source_provenance: &[SuggestionSourceProvenance],
    ) -> Result<BTreeMap<String, String>, String> {
        let expected = media_keys
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if expected.len() != media_keys.len() {
            return Err(
                "correction media keys must be unique before fingerprint binding".to_string(),
            );
        }
        let mut fingerprints = BTreeMap::<String, String>::new();
        let mut bind = |media_key: &str, media_fingerprint: &str, source: &str| {
            if !expected.contains(media_key) {
                return Ok(());
            }
            let media_fingerprint = canonical_media_sha256(media_fingerprint)
                .ok_or_else(|| format!("{source} media fingerprint for {media_key} must contain a lowercase SHA-256 digest"))?;
            match fingerprints.get(media_key) {
                Some(existing) if existing != media_fingerprint => Err(format!(
                    "correction media {media_key} has ambiguous fingerprint evidence"
                )),
                Some(_) => Ok(()),
                None => {
                    fingerprints.insert(media_key.to_string(), media_fingerprint.to_string());
                    Ok(())
                }
            }
        };

        if !face_ids.is_empty() {
            let db = self.database();
            let bounded_face_ids = face_ids.to_vec();
            let current_faces: Vec<FaceObservation> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_face_observation WHERE face_id IN $face_ids LIMIT 4097;",
                    )
                    .bind(("face_ids", bounded_face_ids))
                    .await
                    .map_err(|error| {
                        format!("query correction media fingerprint evidence: {error}")
                    })?;
                response.take(0).map_err(|error| {
                    format!("decode correction media fingerprint evidence: {error}")
                })
            })?;
            if current_faces.len() > CORRECTION_ROW_LIMIT {
                return Err(format!(
                    "correction media fingerprint evidence exceeds {CORRECTION_ROW_LIMIT} rows"
                ));
            }
            for face in &current_faces {
                bind(
                    &face.media_key,
                    &face.media_fingerprint,
                    "current FaceObservation",
                )?;
            }
        }

        for row in rows.iter().filter(|row| row.table == CorrectionTable::Face) {
            for snapshot in [&row.before, &row.after].into_iter().flatten() {
                let face: FaceObservation = serde_json::from_value(snapshot.clone()).map_err(
                    |error| {
                        format!(
                            "decode correction Face snapshot {} for media fingerprint binding: {error}",
                            row.stable_id
                        )
                    },
                )?;
                if face.face_id != row.stable_id {
                    return Err(format!(
                        "correction Face snapshot {} has mismatched FaceId",
                        row.stable_id
                    ));
                }
                bind(
                    &face.media_key,
                    &face.media_fingerprint,
                    "correction Face snapshot",
                )?;
            }
        }
        for provenance in suggestion_source_provenance {
            bind(
                &provenance.media_key,
                &provenance.media_fingerprint,
                "SuggestionSourceProvenance",
            )?;
        }
        for media_key in media_keys {
            if !fingerprints.contains_key(media_key) {
                return Err(format!(
                    "correction media {media_key} lacks exact fingerprint evidence"
                ));
            }
        }
        Ok(fingerprints)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_correction_unlocked(
        &self,
        operation_id: &str,
        kind: &str,
        face_id: Option<&str>,
        person_id: Option<&str>,
        rows: Vec<CorrectionRowDelta>,
        mut face_ids: Vec<String>,
        mut media_keys: Vec<String>,
        identity_changed: bool,
        catalog_changed: bool,
        execution: PersistedExecutionState,
    ) -> Result<CorrectionReceipt, String> {
        if rows.len() > CORRECTION_ROW_LIMIT {
            return Err(format!(
                "correction requires {} reversible rows, exceeding the global {CORRECTION_ROW_LIMIT}-row correction-delta limit; use bounded face selection/split actions",
                rows.len()
            ));
        }
        face_ids.sort();
        face_ids.dedup();
        media_keys.sort();
        media_keys.dedup();
        let created_at = now();
        let suggestion_source_provenance =
            self.suggestion_source_provenance_unlocked(operation_id, kind, &created_at, &rows)?;
        let media_fingerprints = self.correction_media_fingerprint_map_unlocked(
            &media_keys,
            &face_ids,
            &rows,
            &suggestion_source_provenance,
        )?;
        let envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: kind.to_string(),
            rows: rows.clone(),
            face_ids: face_ids.clone(),
            media_keys: media_keys.clone(),
            identity_changed,
            catalog_changed,
        };
        let before_json = serialize_correction_operation_json(
            &rows
                .iter()
                .map(|row| (&row.table, &row.stable_id, &row.before))
                .collect::<Vec<_>>(),
            "before_json",
        )?;
        let after_json = serialize_correction_operation_json(&envelope, "after_json")?;
        let operation = MatchOperation {
            operation_id: operation_id.to_string(),
            kind: format!("correction_{kind}"),
            face_id: face_id.map(str::to_string),
            person_id: person_id.map(str::to_string),
            before_json,
            after_json,
            reversible: true,
            created_at: created_at.clone(),
        };
        let mut owned_upserts = Vec::<(String, String, Value)>::new();
        let mut owned_deletes = Vec::<(String, String)>::new();
        materialize_rows(&rows, &mut owned_upserts, &mut owned_deletes);
        owned_upserts.push((
            OPERATION_TABLE.to_string(),
            operation_id.to_string(),
            serde_json::to_value(operation).map_err(|error| error.to_string())?,
        ));
        for media_key in &media_keys {
            let mapping = CorrectionMediaOperation {
                mapping_id: correction_media_mapping_id(operation_id, media_key),
                media_key: media_key.clone(),
                media_fingerprint: media_fingerprints.get(media_key).cloned().ok_or_else(|| {
                    format!("correction media {media_key} lacks exact fingerprint evidence")
                })?,
                operation_id: operation_id.to_string(),
                kind: kind.to_string(),
                created_at: created_at.clone(),
            };
            owned_upserts.push((
                CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                mapping.mapping_id.clone(),
                serde_json::to_value(mapping).map_err(|error| error.to_string())?,
            ));
        }
        for provenance in suggestion_source_provenance {
            owned_upserts.push((
                SUGGESTION_SOURCE_PROVENANCE_TABLE.to_string(),
                provenance.provenance_id.clone(),
                serde_json::to_value(provenance).map_err(|error| error.to_string())?,
            ));
        }
        owned_upserts.push((
            EXECUTION_TABLE.to_string(),
            "global".to_string(),
            serde_json::to_value(&execution).map_err(|error| error.to_string())?,
        ));
        for media_key in &media_keys {
            owned_deletes.push((PROJECTION_TABLE.to_string(), media_key.clone()));
        }
        if identity_changed {
            owned_deletes.push((TRUSTED_INDEX_BUILD_TABLE.to_string(), "global".to_string()));
        }
        if identity_changed {
            let (calibration_upserts, _) =
                self.calibration_invalidation_rows_unlocked("wp084_correction")?;
            owned_upserts.extend(calibration_upserts);
        }
        dedup_owned_deletes(&mut owned_deletes);
        self.commit_owned_unlocked(&owned_upserts, &owned_deletes)?;
        if identity_changed {
            let _ = self.reconcile_trusted_search_unlocked();
        }
        Ok(CorrectionReceipt {
            operation_id: operation_id.to_string(),
            kind: kind.to_string(),
            changed_rows: rows.len(),
            conflict_rows: 0,
            affected_face_ids: face_ids,
            affected_media_keys: media_keys,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    pub(super) fn commit_owned_unlocked(
        &self,
        upserts: &[(String, String, Value)],
        deletes: &[(String, String)],
    ) -> Result<(), String> {
        let borrowed_upserts = upserts
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        let borrowed_deletes = deletes
            .iter()
            .map(|(table, id)| (table.as_str(), id.as_str()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&borrowed_upserts, &borrowed_deletes)
    }

    fn get_value_unlocked(&self, table: &str, id: &str) -> Result<Option<Value>, String> {
        self.get_one_unlocked::<Value>(table, id)
    }

    fn query_face_values_unlocked(
        &self,
        table: &str,
        face_id: &str,
    ) -> Result<Vec<(String, Value)>, String> {
        let id_field = match table {
            EMBEDDING_TABLE => "embedding_id",
            ASSIGNMENT_TABLE => "assignment_id",
            SUGGESTION_TABLE => "suggestion_id",
            CONSTRAINT_TABLE => "constraint_id",
            TRUSTED_MEMBER_TABLE => "membership_id",
            TRUSTED_SEARCH_TABLE => "membership_id",
            FACE_DISPOSITION_TABLE => "face_id",
            other => return Err(format!("unsupported face correction table {other}")),
        };
        let db = self.database();
        let sql = format!(
            "SELECT * OMIT id FROM {table} WHERE face_id = $face_id ORDER BY {id_field} ASC LIMIT {};",
            CORRECTION_ROW_LIMIT + 1
        );
        let face_id = face_id.to_string();
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("face_id", face_id))
                .await
                .map_err(|error| format!("query face correction rows: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode face correction rows: {error}"))
        })?;
        if rows.len() > CORRECTION_ROW_LIMIT {
            return Err("face correction dependency set exceeds bounded limit".to_string());
        }
        rows.into_iter()
            .map(|row| {
                let id = row
                    .get(id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{table} row lacks stable {id_field}"))?
                    .to_string();
                Ok((id, row))
            })
            .collect()
    }

    fn query_typed_by_field_unlocked<T: DeserializeOwned>(
        &self,
        table: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<T>, String> {
        if !matches!(field, "person_id" | "look_id" | "set_id") {
            return Err("unsupported correction query field".to_string());
        }
        let db = self.database();
        let sql = format!(
            "SELECT * OMIT id FROM {table} WHERE {field} = $value LIMIT {};",
            CORRECTION_ROW_LIMIT + 1
        );
        let value = value.to_string();
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("value", value))
                .await
                .map_err(|error| format!("query correction rows: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode correction rows: {error}"))
        })?;
        if rows.len() > CORRECTION_ROW_LIMIT {
            return Err("correction selection exceeds bounded row limit".to_string());
        }
        rows.into_iter()
            .map(|row| serde_json::from_value(row).map_err(|error| error.to_string()))
            .collect()
    }

    /// Visit an exact field-filtered inventory in stable keyset pages. The
    /// callback sees every row, while no database response or retained Rust
    /// collection grows with a whole-Person inventory.
    fn visit_values_by_field_unlocked(
        &self,
        table: &str,
        id_field: &str,
        field: &str,
        value: &str,
        mut visit: impl FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<usize, String> {
        let allowed = matches!(
            (table, id_field, field),
            (LOOK_TABLE, "look_id", "person_id")
                | (TEMPLATE_SET_TABLE, "set_id", "look_id")
                | (TRUSTED_MEMBER_TABLE, "membership_id", "set_id")
                | (TRUSTED_MEMBER_TABLE, "membership_id", "face_id")
                | (TRUSTED_SEARCH_TABLE, "membership_id", "person_id")
                | (CONSTRAINT_TABLE, "constraint_id", "person_id")
                | (SUGGESTION_TABLE, "suggestion_id", "candidate_person_id")
        );
        if !allowed {
            return Err("unsupported streamed Person-edit inventory".to_string());
        }
        let mut after_id = String::new();
        let mut row_count = 0usize;
        loop {
            let db = self.database();
            let index_hint = if table == SUGGESTION_TABLE {
                " WITH INDEX match_suggestion_person_inventory"
            } else {
                ""
            };
            let sql = format!(
                "SELECT * OMIT id FROM {table}{index_hint} WHERE {field} = $value AND {id_field} > $after_id ORDER BY {id_field} ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
            );
            let value = value.to_string();
            let page_after = after_id.clone();
            let rows: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(sql)
                    .bind(("value", value))
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("query streamed Person-edit inventory: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode streamed Person-edit inventory: {error}"))
            })?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let stable_id = row
                    .get(id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{table} row lacks stable {id_field}"))?;
                if stable_id <= after_id.as_str() {
                    return Err("streamed Person-edit inventory did not advance".to_string());
                }
                visit(stable_id, row)?;
                row_count = row_count
                    .checked_add(1)
                    .ok_or("Person-edit inventory row count overflow")?;
            }
            after_id = rows
                .last()
                .and_then(|row| row.get(id_field))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{table} page lacks terminal {id_field}"))?
                .to_string();
            if rows.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(row_count)
    }

    fn visit_person_suggestion_faces_unlocked(
        &self,
        person_id: &str,
        mut visit: impl FnMut(&FaceObservation) -> Result<(), String>,
    ) -> Result<usize, String> {
        let mut after_suggestion_id = String::new();
        let mut row_count = 0usize;
        loop {
            let db = self.database();
            let bound_person_id = person_id.to_string();
            let page_after = after_suggestion_id.clone();
            let suggestion_rows: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(format!(
                        "SELECT suggestion_id, face_id, candidate_person_id FROM {SUGGESTION_TABLE} WITH INDEX match_suggestion_person_inventory WHERE candidate_person_id = $person_id AND suggestion_id > $after_suggestion_id ORDER BY suggestion_id ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
                    ))
                    .bind(("person_id", bound_person_id))
                    .bind(("after_suggestion_id", page_after))
                    .await
                    .map_err(|error| {
                        format!("query Person suggestion Face inventory page: {error}")
                    })?;
                response.take(0).map_err(|error| {
                    format!("decode Person suggestion Face inventory page: {error}")
                })
            })?;
            if suggestion_rows.is_empty() {
                break;
            }
            if suggestion_rows.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err("Person suggestion Face inventory page exceeded its bound".to_string());
            }

            let mut previous_suggestion_id = after_suggestion_id.clone();
            let mut face_ids = BTreeSet::new();
            for row in &suggestion_rows {
                let suggestion_id = row
                    .get("suggestion_id")
                    .and_then(Value::as_str)
                    .ok_or("Person suggestion Face inventory row lacks suggestion_id")?;
                let candidate_person_id = row
                    .get("candidate_person_id")
                    .and_then(Value::as_str)
                    .ok_or("Person suggestion Face inventory row lacks candidate_person_id")?;
                let face_id = row
                    .get("face_id")
                    .and_then(Value::as_str)
                    .ok_or("Person suggestion Face inventory row lacks face_id")?;
                if candidate_person_id != person_id
                    || suggestion_id <= previous_suggestion_id.as_str()
                {
                    return Err(
                        "Person suggestion Face inventory order is not canonical".to_string()
                    );
                }
                if !face_ids.insert(face_id.to_string()) {
                    return Err(format!(
                        "Person suggestion inventory contains duplicate FaceObservation reference {face_id}"
                    ));
                }
                previous_suggestion_id = suggestion_id.to_string();
            }

            let requested_face_ids = face_ids.into_iter().collect::<Vec<_>>();
            let db = self.database();
            let bound_face_ids = requested_face_ids.clone();
            let mut faces: Vec<FaceObservation> = surreal_store::run(async move {
                let mut response = db
                    .query(format!(
                        "SELECT * OMIT id FROM {FACE_TABLE} WITH INDEX match_face_id WHERE face_id IN $face_ids ORDER BY face_id ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
                    ))
                    .bind(("face_ids", bound_face_ids))
                    .await
                    .map_err(|error| {
                        format!("query canonical Person suggestion Faces: {error}")
                    })?;
                response
                    .take(0)
                    .map_err(|error| format!("decode canonical Person suggestion Faces: {error}"))
            })?;
            if faces.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err("canonical Person suggestion Face page exceeded its bound".to_string());
            }
            faces.sort_by(|left, right| left.face_id.cmp(&right.face_id));
            let observed_face_ids = faces
                .iter()
                .map(|face| face.face_id.clone())
                .collect::<Vec<_>>();
            if observed_face_ids != requested_face_ids {
                let observed = observed_face_ids.iter().collect::<BTreeSet<_>>();
                let missing = requested_face_ids
                    .iter()
                    .filter(|face_id| !observed.contains(face_id))
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    return Err(format!(
                        "Person suggestion inventory references {} missing FaceObservation row(s): {}",
                        missing.len(),
                        missing.join(", ")
                    ));
                }
                return Err(
                    "canonical Person suggestion Face inventory is not one-to-one".to_string(),
                );
            }
            for face in &faces {
                visit(face)?;
                row_count = row_count
                    .checked_add(1)
                    .ok_or("Person suggestion Face count overflow")?;
            }

            after_suggestion_id = suggestion_rows
                .last()
                .and_then(|row| row.get("suggestion_id"))
                .and_then(Value::as_str)
                .ok_or("Person suggestion Face page lacks terminal suggestion_id")?
                .to_string();
            if suggestion_rows.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(row_count)
    }

    fn person_edit_affected_face_media_counts_unlocked(
        &self,
        person_id: &str,
    ) -> Result<(usize, usize), String> {
        self.bounded_person_edit_affected_counts_unlocked(person_id, None)
    }

    fn merge_person_edit_affected_face_media_counts_unlocked(
        &self,
        source_person_id: &str,
        target_person_id: &str,
    ) -> Result<(usize, usize), String> {
        self.bounded_person_edit_affected_counts_unlocked(source_person_id, Some(target_person_id))
    }

    /// Count canonical Faces once; distinct media use disjoint lexical passes
    /// when the existing retained-key bound is exceeded. No whole-Person ID
    /// collection or correlated predicate runs ahead of the canonical page.
    fn bounded_person_edit_affected_counts_unlocked(
        &self,
        person_id: &str,
        target_person_id: Option<&str>,
    ) -> Result<(usize, usize), String> {
        let mut partitions = vec![(None::<String>, None::<String>, true)];
        let mut face_count = 0usize;
        let mut media_count = 0usize;
        let mut passes = 0usize;
        while let Some((lower, upper, count_faces)) = partitions.pop() {
            passes = passes.checked_add(1).ok_or("Person count pass overflow")?;
            if passes > 4096 {
                return Err(
                    "exact Person media counting exceeded its bounded partition work".into(),
                );
            }
            let mut media = BTreeSet::new();
            let mut overflow = false;
            let mut after_face_id = String::new();
            loop {
                let db = self.database();
                let after = after_face_id.clone();
                let page: Vec<Value> = surreal_store::run(async move {
                    let mut response = db.query(format!(
                        "SELECT face_id, media_key FROM {FACE_TABLE} WITH INDEX match_face_id WHERE face_id > $after ORDER BY face_id ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
                    )).bind(("after", after)).await
                        .map_err(|error| format!("page canonical Person-count Faces: {error}"))?;
                    response
                        .take(0)
                        .map_err(|error| format!("decode canonical Person-count Faces: {error}"))
                })?;
                if page.is_empty() {
                    break;
                }
                if page.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                    return Err("canonical Person-count Face page exceeded its bound".into());
                }
                let mut candidates = Vec::new();
                for face in &page {
                    let face_id = required_string(face, "face_id")?;
                    let media_key = required_string(face, "media_key")?;
                    validate_text("Face ID", &face_id)?;
                    validate_media_key(&media_key)?;
                    if face_id <= after_face_id {
                        return Err("canonical Person-count Face page did not advance".into());
                    }
                    after_face_id = face_id.clone();
                    if count_faces
                        || (lower.as_ref().is_none_or(|key| media_key > *key)
                            && upper.as_ref().is_none_or(|key| media_key <= *key))
                    {
                        candidates.push((face_id, media_key));
                    }
                }
                if !candidates.is_empty() {
                    let ids = candidates
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    let affected = self.person_edit_affected_face_page_unlocked(
                        person_id,
                        target_person_id,
                        ids,
                    )?;
                    for (face_id, media_key) in candidates {
                        if !affected.contains(&face_id) {
                            continue;
                        }
                        if count_faces {
                            face_count = face_count
                                .checked_add(1)
                                .ok_or("Person affected Face count overflow")?;
                        }
                        if !media.contains(&media_key) {
                            if media.len() == CORRECTION_ROW_LIMIT {
                                overflow = true;
                            } else {
                                media.insert(media_key);
                            }
                        }
                    }
                }
                if (overflow && !count_faces) || page.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                    break;
                }
            }
            if overflow {
                let pivot = media
                    .iter()
                    .nth(media.len() / 2)
                    .cloned()
                    .ok_or("Person media partition has no split pivot")?;
                if lower.as_ref().is_some_and(|key| pivot <= *key)
                    || upper.as_ref().is_some_and(|key| pivot >= *key)
                {
                    return Err("Person media partition failed to make strict progress".into());
                }
                drop(media);
                if partitions.len() > 62 {
                    return Err(
                        "exact Person media counting exceeded its bounded partition stack".into(),
                    );
                }
                partitions.push((Some(pivot.clone()), upper, false));
                partitions.push((lower, Some(pivot), false));
            } else {
                media_count = media_count
                    .checked_add(media.len())
                    .ok_or("Person affected media count overflow")?;
            }
        }
        Ok((face_count, media_count))
    }

    fn person_edit_affected_face_page_unlocked(
        &self,
        person_id: &str,
        target_person_id: Option<&str>,
        face_ids: Vec<String>,
    ) -> Result<BTreeSet<String>, String> {
        let requested = face_ids.iter().cloned().collect::<BTreeSet<_>>();
        if requested.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
            return Err("Person affected-reference Face batch exceeded its bound".into());
        }
        let mut assignment_people = vec![person_id.to_string()];
        if let Some(target) = target_person_id {
            assignment_people.push(target.to_string());
        }
        let source = person_id.to_string();
        let db = self.database();
        let sql = format!(
            "SELECT face_id FROM {ASSIGNMENT_TABLE} WITH INDEX match_assignment_face WHERE face_id IN $face_ids AND person_id IN $assignment_people GROUP BY face_id LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};\
             SELECT face_id FROM {SUGGESTION_TABLE} WITH INDEX match_suggestion_face_person WHERE face_id IN $face_ids AND candidate_person_id IN $assignment_people GROUP BY face_id LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};\
             SELECT face_id FROM {CONSTRAINT_TABLE} WITH INDEX match_constraint_face_person WHERE face_id IN $face_ids AND person_id = $person_id GROUP BY face_id LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};\
             SELECT face_id FROM {TRUSTED_SEARCH_TABLE} WITH INDEX match_trusted_search_face_person WHERE face_id IN $face_ids AND person_id = $person_id GROUP BY face_id LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};\
             SELECT face_id FROM {TRUSTED_MEMBER_TABLE} WITH INDEX match_trusted_member_face WHERE face_id IN $face_ids AND array::len((SELECT VALUE set_id FROM {TEMPLATE_SET_TABLE} WITH INDEX match_template_set_id WHERE set_id = $parent.set_id AND array::len((SELECT VALUE look_id FROM {LOOK_TABLE} WITH INDEX match_look_id WHERE look_id = $parent.look_id AND person_id = $person_id LIMIT 1)) > 0 LIMIT 1)) > 0 GROUP BY face_id LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
        );
        let families: Vec<Vec<Value>> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("face_ids", face_ids))
                .bind(("assignment_people", assignment_people))
                .bind(("person_id", source))
                .await
                .map_err(|error| format!("query bounded Person affected references: {error}"))?
                .check()
                .map_err(|error| format!("check bounded Person affected references: {error}"))?;
            (0..5)
                .map(|index| {
                    response.take(index).map_err(|error| {
                        format!("decode bounded Person reference family {index}: {error}")
                    })
                })
                .collect()
        })?;
        let mut affected = BTreeSet::new();
        for family in families {
            if family.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err("Person affected-reference family exceeded its bound".into());
            }
            for row in family {
                let face_id = required_string(&row, "face_id")?;
                if !requested.contains(&face_id) {
                    return Err("Person affected-reference query escaped its canonical page".into());
                }
                affected.insert(face_id);
            }
        }
        Ok(affected)
    }

    fn append_person_suggestion_inventory_unlocked(
        &self,
        person_id: &str,
        inventory_digest: &mut Sha256,
        affected_face_ids: &mut BTreeSet<String>,
        affected_media_keys: &mut BTreeSet<String>,
    ) -> Result<usize, String> {
        let suggestion_count = self.visit_values_by_field_unlocked(
            SUGGESTION_TABLE,
            "suggestion_id",
            "candidate_person_id",
            person_id,
            |suggestion_id, row| {
                hash_person_edit_inventory_row(
                    inventory_digest,
                    SUGGESTION_TABLE,
                    suggestion_id,
                    row,
                )
            },
        )?;
        let observed_face_count =
            self.visit_person_suggestion_faces_unlocked(person_id, |face| {
                let row = serde_json::to_value(face).map_err(|error| error.to_string())?;
                hash_person_edit_inventory_row(
                    inventory_digest,
                    "match_suggestion_face",
                    &face.face_id,
                    &row,
                )?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(face.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    affected_media_keys.insert(face.media_key.clone());
                }
                Ok(())
            })?;
        // `match_suggestion_face_person` is UNIQUE, so one Person can have at
        // most one suggestion per face. Comparing canonical Face rows directly
        // with the exact suggestion count therefore detects every orphan
        // without a redundant whole-inventory GROUP BY query.
        if observed_face_count != suggestion_count {
            return Err(format!(
                "Person suggestion inventory references {} missing FaceObservation row(s)",
                suggestion_count.saturating_sub(observed_face_count)
            ));
        }
        Ok(suggestion_count)
    }

    fn visit_remove_person_descendants_unlocked(
        &self,
        table: &str,
        id_field: &str,
        person_id: &str,
        mut visit: impl FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<usize, String> {
        let scope = match (table, id_field) {
            (TEMPLATE_SET_TABLE, "set_id") => format!(
                "look_id IN (SELECT VALUE look_id FROM {LOOK_TABLE} WHERE person_id = $person_id)"
            ),
            (TRUSTED_MEMBER_TABLE, "membership_id") => format!(
                "set_id IN (SELECT VALUE set_id FROM {TEMPLATE_SET_TABLE} WHERE look_id IN (SELECT VALUE look_id FROM {LOOK_TABLE} WHERE person_id = $person_id))"
            ),
            (TRUSTED_SEARCH_TABLE, "membership_id") => format!(
                "membership_id IN (SELECT VALUE membership_id FROM {TRUSTED_MEMBER_TABLE} WHERE set_id IN (SELECT VALUE set_id FROM {TEMPLATE_SET_TABLE} WHERE look_id IN (SELECT VALUE look_id FROM {LOOK_TABLE} WHERE person_id = $person_id)))"
            ),
            _ => return Err("unsupported remove-Person descendant inventory".to_string()),
        };
        let mut after_id = String::new();
        let mut row_count = 0usize;
        loop {
            let db = self.database();
            let sql = format!(
                "SELECT * OMIT id FROM {table} WHERE {scope} AND {id_field} > $after_id ORDER BY {id_field} ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
            );
            let person_id = person_id.to_string();
            let page_after = after_id.clone();
            let rows: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(sql)
                    .bind(("person_id", person_id))
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("query remove-Person descendants: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode remove-Person descendants: {error}"))
            })?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let stable_id = row
                    .get(id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{table} row lacks stable {id_field}"))?;
                visit(stable_id, row)?;
                row_count = row_count
                    .checked_add(1)
                    .ok_or("remove-Person descendant count overflow")?;
            }
            after_id = rows
                .last()
                .and_then(|row| row.get(id_field))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{table} page lacks terminal {id_field}"))?
                .to_string();
            if rows.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(row_count)
    }

    fn person_assignment_media_count_unlocked(&self, person_id: &str) -> Result<usize, String> {
        let db = self.database();
        let person_id = person_id.to_string();
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT count() AS count FROM (SELECT media_key FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP BY media_key) GROUP ALL;",
                )
                .bind(("person_id", person_id))
                .await
                .map_err(|error| format!("count Person assignment media: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Person assignment media count: {error}"))
        })?;
        let count = rows
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        usize::try_from(count).map_err(|_| "Person media count exceeds this runtime".to_string())
    }

    fn bounded_person_assignment_inventory_unlocked(
        &self,
        person_id: &str,
    ) -> Result<(Vec<Assignment>, usize, String), String> {
        let mut retained = Vec::new();
        let mut digest = person_inventory_digest(person_id);
        let mut after_assignment_id = String::new();
        let mut row_count = 0usize;
        loop {
            let db = self.database();
            let person_id = person_id.to_string();
            let page_after = after_assignment_id.clone();
            let rows: Vec<Assignment> = surreal_store::run(async move {
                let mut response = db
                    .query(format!(
                        "SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT {PERSON_INVENTORY_DIGEST_PAGE_LIMIT};"
                    ))
                    .bind(("person_id", person_id))
                    .bind(("after_assignment_id", page_after))
                    .await
                    .map_err(|error| format!("query bounded Person assignment inventory: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode bounded Person assignment inventory: {error}"))
            })?;
            if rows.is_empty() {
                break;
            }
            for assignment in &rows {
                hash_person_inventory_assignment(&mut digest, assignment)?;
                row_count = row_count
                    .checked_add(1)
                    .ok_or("Person assignment inventory row count overflow")?;
                if retained.len() < CORRECTION_ROW_LIMIT {
                    retained.push(assignment.clone());
                }
            }
            after_assignment_id = rows
                .last()
                .map(|assignment| assignment.assignment_id.clone())
                .ok_or("Person assignment inventory page unexpectedly empty")?;
            if rows.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok((
            retained,
            row_count,
            finish_person_inventory_digest(digest, row_count as u64),
        ))
    }

    /// Visit every target assignment through stable bounded pages. Operator-
    /// confirmed rows survive a Person merge and are rebound to the new target
    /// revision; strict-automatic rows are invalidated instead of being
    /// silently re-endorsed across that revision change.
    fn visit_merge_target_assignments_unlocked(
        &self,
        person_id: &str,
        mut visit: impl FnMut(&Assignment) -> Result<(), String>,
    ) -> Result<usize, String> {
        let mut after_assignment_id = String::new();
        let mut row_count = 0usize;
        loop {
            let db = self.database();
            let bound_person_id = person_id.to_string();
            let bound_after = after_assignment_id.clone();
            let page: Vec<Assignment> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT $limit;",
                    )
                    .bind(("person_id", bound_person_id))
                    .bind(("after_assignment_id", bound_after))
                    .bind(("limit", PERSON_INVENTORY_DIGEST_PAGE_LIMIT as u64))
                    .await
                    .map_err(|error| {
                        format!("query merge-target Person assignment page: {error}")
                    })?;
                response
                    .take(0)
                    .map_err(|error| format!("decode merge-target Person assignment page: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            if page.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err("merge-target Person assignment page exceeded its bound".to_string());
            }
            let mut previous = after_assignment_id.clone();
            for assignment in &page {
                if assignment.person_id != person_id || assignment.assignment_id <= previous {
                    return Err(
                        "merge-target Person assignment inventory is not canonical".to_string()
                    );
                }
                if assignment.state != AssignmentState::OperatorConfirmed.as_str()
                    && assignment.state != AssignmentState::CommittedStrictAutomatic.as_str()
                {
                    return Err(format!(
                        "merge target Assignment {} has unsupported state {}",
                        assignment.assignment_id, assignment.state
                    ));
                }
                visit(assignment)?;
                row_count = row_count
                    .checked_add(1)
                    .ok_or("merge-target Person assignment count overflow")?;
                previous = assignment.assignment_id.clone();
            }
            after_assignment_id = page
                .last()
                .map(|assignment| assignment.assignment_id.clone())
                .ok_or("merge-target Person assignment page unexpectedly empty")?;
            if page.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(row_count)
    }

    /// Read a complete Person assignment inventory through stable keyset
    /// pages. Whole-Person previews must remain exact beyond one 4096-row
    /// correction transaction, while every database response stays bounded.
    /// The separate correction-delta limit remains the honest atomic/restart-
    /// safe execution bound and is checked after all affected row kinds are
    /// known.
    fn person_assignments_streamed_unlocked(
        &self,
        person_id: &str,
    ) -> Result<Vec<Assignment>, String> {
        let mut assignments = Vec::new();
        let mut after_assignment_id = String::new();
        loop {
            let db = self.database();
            let bound_person_id = person_id.to_string();
            let bound_after = after_assignment_id.clone();
            let page: Vec<Assignment> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT $limit;",
                    )
                    .bind(("person_id", bound_person_id))
                    .bind(("after_assignment_id", bound_after))
                    .bind(("limit", PERSON_INVENTORY_DIGEST_PAGE_LIMIT as u64))
                    .await
                    .map_err(|error| format!("query Person assignment inventory page: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Person assignment inventory page: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            if page.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err("Person assignment inventory page exceeded its bound".to_string());
            }
            let mut previous = after_assignment_id.clone();
            for assignment in &page {
                if assignment.person_id != person_id || assignment.assignment_id <= previous {
                    return Err("Person assignment inventory order is not canonical".to_string());
                }
                previous = assignment.assignment_id.clone();
            }
            after_assignment_id = page
                .last()
                .map(|assignment| assignment.assignment_id.clone())
                .ok_or("Person assignment inventory page unexpectedly empty")?;
            if assignments.len().checked_add(page.len()).is_none() {
                return Err("Person assignment inventory row count overflow".to_string());
            }
            let page_len = page.len();
            assignments.extend(page);
            if page_len < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(assignments)
    }

    fn person_assignment_inventory_preview_token_unlocked(
        &self,
        person_id: &str,
    ) -> Result<String, String> {
        let mut digest = person_inventory_digest(person_id);
        let mut after_assignment_id = String::new();
        let mut row_count = 0_u64;
        loop {
            let db = self.database();
            let bound_person_id = person_id.to_string();
            let bound_after = after_assignment_id.clone();
            let rows: Vec<Assignment> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT $limit;",
                    )
                    .bind(("person_id", bound_person_id))
                    .bind(("after_assignment_id", bound_after))
                    .bind(("limit", PERSON_INVENTORY_DIGEST_PAGE_LIMIT as u64))
                    .await
                    .map_err(|error| format!("query Person assignment inventory digest page: {error}"))?;
                response.take(0).map_err(|error| {
                    format!("decode Person assignment inventory digest page: {error}")
                })
            })?;
            if rows.is_empty() {
                break;
            }
            if rows.len() > PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                return Err(
                    "Person assignment inventory digest page exceeded its bound".to_string()
                );
            }
            let mut previous = after_assignment_id.clone();
            for assignment in &rows {
                if assignment.person_id != person_id || assignment.assignment_id <= previous {
                    return Err(
                        "Person assignment inventory digest order is not canonical".to_string()
                    );
                }
                hash_person_inventory_assignment(&mut digest, assignment)?;
                previous = assignment.assignment_id.clone();
                row_count = row_count
                    .checked_add(1)
                    .ok_or("Person assignment inventory row count overflow")?;
            }
            after_assignment_id = rows
                .last()
                .map(|assignment| assignment.assignment_id.clone())
                .ok_or("Person assignment inventory page unexpectedly empty")?;
            if rows.len() < PERSON_INVENTORY_DIGEST_PAGE_LIMIT {
                break;
            }
        }
        Ok(finish_person_inventory_digest(digest, row_count))
    }

    fn push_existing_delta_unlocked(
        &self,
        deltas: &mut Vec<CorrectionRowDelta>,
        table: CorrectionTable,
        id: &str,
        after: Option<Value>,
    ) -> Result<(), String> {
        deltas.push(CorrectionRowDelta {
            before: self.get_value_unlocked(table.name(), id)?,
            table,
            stable_id: id.to_string(),
            after,
        });
        Ok(())
    }

    fn append_face_suggestion_removal_deltas_unlocked(
        &self,
        deltas: &mut Vec<CorrectionRowDelta>,
        face_id: &str,
    ) -> Result<(), String> {
        for (suggestion_id, suggestion) in
            self.query_face_values_unlocked(SUGGESTION_TABLE, face_id)?
        {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::Suggestion,
                stable_id: suggestion_id,
                before: Some(suggestion),
                after: None,
            });
        }
        Ok(())
    }

    fn append_trust_removal_deltas_unlocked(
        &self,
        deltas: &mut Vec<CorrectionRowDelta>,
        face_id: &str,
    ) -> Result<(), String> {
        for (membership_id, membership) in
            self.query_face_values_unlocked(TRUSTED_MEMBER_TABLE, face_id)?
        {
            deltas.push(CorrectionRowDelta {
                table: CorrectionTable::TrustedMember,
                stable_id: membership_id.clone(),
                before: Some(membership),
                after: None,
            });
            if let Some(search) = self.get_value_unlocked(TRUSTED_SEARCH_TABLE, &membership_id)? {
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::TrustedSearch,
                    stable_id: membership_id,
                    before: Some(search),
                    after: None,
                });
            }
        }
        Ok(())
    }

    fn trust_removal_delta_counts_unlocked(
        &self,
        face_ids: &[String],
        inventory_digest: &mut Sha256,
    ) -> Result<(usize, usize), String> {
        let mut trusted_members = 0usize;
        let mut trusted_search = 0usize;
        for face_id in face_ids {
            self.visit_values_by_field_unlocked(
                TRUSTED_MEMBER_TABLE,
                "membership_id",
                "face_id",
                face_id,
                |membership_id, member_row| {
                    hash_person_edit_inventory_row(
                        inventory_digest,
                        TRUSTED_MEMBER_TABLE,
                        membership_id,
                        member_row,
                    )?;
                    trusted_members = trusted_members
                        .checked_add(1)
                        .ok_or("trusted-member correction count overflow")?;
                    if let Some(search_row) =
                        self.get_value_unlocked(TRUSTED_SEARCH_TABLE, membership_id)?
                    {
                        hash_person_edit_inventory_row(
                            inventory_digest,
                            TRUSTED_SEARCH_TABLE,
                            membership_id,
                            &search_row,
                        )?;
                        trusted_search = trusted_search
                            .checked_add(1)
                            .ok_or("trusted-search correction count overflow")?;
                    }
                    Ok(())
                },
            )?;
        }
        Ok((trusted_members, trusted_search))
    }

    fn suggestion_removal_delta_count_unlocked(
        &self,
        face_ids: &[String],
        inventory_digest: &mut Sha256,
    ) -> Result<usize, String> {
        let mut suggestions = 0usize;
        for face_id in face_ids {
            for (suggestion_id, suggestion_row) in
                self.query_face_values_unlocked(SUGGESTION_TABLE, face_id)?
            {
                hash_person_edit_inventory_row(
                    inventory_digest,
                    SUGGESTION_TABLE,
                    &suggestion_id,
                    &suggestion_row,
                )?;
                suggestions = suggestions
                    .checked_add(1)
                    .ok_or("suggestion correction count overflow")?;
            }
        }
        Ok(suggestions)
    }

    fn append_person_suggestion_removal_deltas_unlocked(
        &self,
        deltas: &mut Vec<CorrectionRowDelta>,
        person_id: &str,
    ) -> Result<(), String> {
        self.visit_values_by_field_unlocked(
            SUGGESTION_TABLE,
            "suggestion_id",
            "candidate_person_id",
            person_id,
            |suggestion_id, row| {
                deltas.push(CorrectionRowDelta {
                    table: CorrectionTable::Suggestion,
                    stable_id: suggestion_id.to_string(),
                    before: Some(row.clone()),
                    after: None,
                });
                Ok(())
            },
        )?;
        Ok(())
    }

    fn calibration_invalidation_rows_unlocked(
        &self,
        reason: &str,
    ) -> Result<(Vec<(String, String, Value)>, usize), String> {
        validate_text("calibration invalidation reason", reason)?;
        let mut rows = self.list_unlocked::<CalibrationActivation>(CALIBRATION_TABLE)?;
        let count = rows.len();
        let timestamp = now();
        let mut upserts = Vec::with_capacity(count);
        for activation in &mut rows {
            activation.active = false;
            activation.wp084_runtime_ready = false;
            activation.wp087_release_ready = false;
            activation.invalidation_reason = Some(reason.to_string());
            activation.updated_at = timestamp.clone();
            activation.activation_integrity_digest =
                calibration_activation_integrity_digest(activation);
            upserts.push((
                CALIBRATION_TABLE.to_string(),
                activation.calibration_generation.clone(),
                serde_json::to_value(activation).map_err(|error| error.to_string())?,
            ));
        }
        Ok((upserts, count))
    }

    fn preview_merge_people_unlocked(
        &self,
        source_person_id: &str,
        target_person_id: &str,
    ) -> Result<PersonEditPreview, String> {
        let source: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let target: Person = self.require_unlocked(PERSON_TABLE, target_person_id, "Person")?;
        let (assignments, assignment_count, assignment_digest) =
            self.bounded_person_assignment_inventory_unlocked(source_person_id)?;
        let mut affected_face_ids = assignments
            .iter()
            .map(|assignment| assignment.face_id.clone())
            .collect::<BTreeSet<_>>();
        let mut affected_media_keys = assignments
            .iter()
            .map(|assignment| assignment.media_key.clone())
            .collect::<BTreeSet<_>>();
        if assignment_count <= CORRECTION_ROW_LIMIT {
            for assignment in &assignments {
                match assignment.state.as_str() {
                    state if state == AssignmentState::OperatorConfirmed.as_str() => {
                        if self
                            .get_one_unlocked::<CannotLinkConstraint>(
                                CONSTRAINT_TABLE,
                                &cannot_link_id(&assignment.face_id, target_person_id),
                            )?
                            .is_some()
                        {
                            return Err(format!(
                                "face {} is cannot-linked to the merge target",
                                assignment.face_id
                            ));
                        }
                    }
                    state if state == AssignmentState::CommittedStrictAutomatic.as_str() => {}
                    state => {
                        return Err(format!(
                            "merge source Assignment {} has unsupported state {state}",
                            assignment.assignment_id
                        ));
                    }
                }
            }
        }
        let mut inventory_digest = person_edit_inventory_digest("merge", &assignment_digest);
        let target_assignment_count =
            self.visit_merge_target_assignments_unlocked(target_person_id, |assignment| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    "match_merge_target_assignment",
                    &assignment.assignment_id,
                    &serde_json::to_value(assignment).map_err(|error| error.to_string())?,
                )?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(assignment.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    affected_media_keys.insert(assignment.media_key.clone());
                }
                Ok(())
            })?;
        let mut look_ids = Vec::new();
        let look_count = self.visit_values_by_field_unlocked(
            LOOK_TABLE,
            "look_id",
            "person_id",
            source_person_id,
            |stable_id, row| {
                hash_person_edit_inventory_row(&mut inventory_digest, LOOK_TABLE, stable_id, row)?;
                if look_ids.len() < CORRECTION_ROW_LIMIT {
                    look_ids.push(stable_id.to_string());
                }
                Ok(())
            },
        )?;
        let trusted_search_count = self.visit_values_by_field_unlocked(
            TRUSTED_SEARCH_TABLE,
            "membership_id",
            "person_id",
            source_person_id,
            |stable_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    TRUSTED_SEARCH_TABLE,
                    stable_id,
                    row,
                )?;
                let search: TrustedSearchEmbedding = serde_json::from_value(row.clone())
                    .map_err(|error| format!("decode merge trusted search: {error}"))?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(search.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    let face: FaceObservation =
                        self.require_unlocked(FACE_TABLE, &search.face_id, "FaceObservation")?;
                    affected_media_keys.insert(face.media_key);
                }
                Ok(())
            },
        )?;
        let mut source_constraint_count = 0usize;
        let mut missing_target_constraints = 0usize;
        self.visit_values_by_field_unlocked(
            CONSTRAINT_TABLE,
            "constraint_id",
            "person_id",
            source_person_id,
            |stable_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    CONSTRAINT_TABLE,
                    stable_id,
                    row,
                )?;
                source_constraint_count = source_constraint_count
                    .checked_add(1)
                    .ok_or("merge source-constraint count overflow")?;
                let constraint: CannotLinkConstraint = serde_json::from_value(row.clone())
                    .map_err(|error| format!("decode merge source constraint: {error}"))?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(constraint.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    let face: FaceObservation = self.require_unlocked(
                        FACE_TABLE,
                        &constraint.face_id,
                        "FaceObservation",
                    )?;
                    affected_media_keys.insert(face.media_key);
                }
                if self
                    .get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, &constraint.face_id)?
                    .is_some_and(|assignment| {
                        assignment.person_id == target_person_id
                            && assignment.state == AssignmentState::OperatorConfirmed.as_str()
                    })
                {
                    return Err(format!(
                        "source cannot-link for face {} conflicts with its current assignment to the merge target",
                        constraint.face_id
                    ));
                }
                let target_id = cannot_link_id(&constraint.face_id, target_person_id);
                match self.get_value_unlocked(CONSTRAINT_TABLE, &target_id)? {
                    Some(target_row) => hash_person_edit_inventory_row(
                        &mut inventory_digest,
                        "match_target_constraint",
                        &target_id,
                        &target_row,
                    )?,
                    None => {
                        inventory_digest.update(b"missing_target_constraint\0");
                        inventory_digest.update((target_id.len() as u64).to_be_bytes());
                        inventory_digest.update(target_id.as_bytes());
                        missing_target_constraints = missing_target_constraints
                            .checked_add(1)
                            .ok_or("merge target-constraint count overflow")?;
                    }
                }
                Ok(())
            },
        )?;
        let constraint_rows = source_constraint_count
            .checked_add(missing_target_constraints)
            .ok_or("merge constraint-row count overflow")?;
        let source_suggestion_count = self.append_person_suggestion_inventory_unlocked(
            source_person_id,
            &mut inventory_digest,
            &mut affected_face_ids,
            &mut affected_media_keys,
        )?;
        let target_suggestion_count = self.append_person_suggestion_inventory_unlocked(
            target_person_id,
            &mut inventory_digest,
            &mut affected_face_ids,
            &mut affected_media_keys,
        )?;
        let suggestion_count = source_suggestion_count
            .checked_add(target_suggestion_count)
            .ok_or("merge suggestion count overflow")?;
        let merge_assignment_delta_count = assignment_count
            .checked_add(target_assignment_count)
            .ok_or("merge assignment-delta count overflow")?;
        let delta_counts = PersonEditDeltaCounts {
            persons: 2,
            assignments: merge_assignment_delta_count,
            looks: look_count,
            trusted_search: trusted_search_count,
            constraints: constraint_rows,
            suggestions: suggestion_count,
            ..PersonEditDeltaCounts::default()
        };
        let required_reversible_rows = delta_counts.checked_total()?;
        let inventory_complete = required_reversible_rows <= CORRECTION_ROW_LIMIT;
        let affected_reference_count = assignment_count
            .checked_add(target_assignment_count)
            .and_then(|count| count.checked_add(trusted_search_count))
            .and_then(|count| count.checked_add(source_constraint_count))
            .and_then(|count| count.checked_add(suggestion_count))
            .ok_or("merge affected-reference count overflow")?;
        let bounded_affected_counts = (affected_face_ids.len(), affected_media_keys.len());
        let (face_ids, media_keys, look_ids) = if inventory_complete {
            look_ids.sort();
            look_ids.dedup();
            (
                affected_face_ids.into_iter().collect(),
                affected_media_keys.into_iter().collect(),
                look_ids,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let (affected_faces, affected_media) = if affected_reference_count <= CORRECTION_ROW_LIMIT {
            bounded_affected_counts
        } else {
            self.merge_person_edit_affected_face_media_counts_unlocked(
                source_person_id,
                target_person_id,
            )?
        };
        let affected_counts = PersonEditAffectedCounts {
            persons: 2,
            faces: affected_faces,
            media: affected_media,
            looks: look_count,
        };
        let inventory_digest = finish_person_edit_inventory_digest(inventory_digest, &delta_counts);
        let execution = self.execution_state_unlocked()?;
        let mut person_revisions = BTreeMap::new();
        person_revisions.insert(source.person_id.clone(), source.revision);
        person_revisions.insert(target.person_id.clone(), target.revision);
        let mut preview = PersonEditPreview {
            preview_id: String::new(),
            kind: PersonEditKind::Merge {
                source_person_id: source.person_id.clone(),
                target_person_id: target.person_id.clone(),
            },
            person_ids: vec![source.person_id, target.person_id],
            face_ids,
            media_keys,
            look_ids,
            assignment_count,
            delta_counts,
            required_reversible_rows,
            affected_counts,
            inventory_complete,
            inventory_digest,
            correction_delta_row_limit: CORRECTION_ROW_LIMIT,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            person_revisions,
        };
        preview.preview_id = preview_digest(&preview)?;
        Ok(preview)
    }

    fn preview_remove_person_unlocked(&self, person_id: &str) -> Result<PersonEditPreview, String> {
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let (assignments, assignment_count, assignment_digest) =
            self.bounded_person_assignment_inventory_unlocked(person_id)?;
        let mut affected_face_ids = assignments
            .iter()
            .map(|assignment| assignment.face_id.clone())
            .collect::<BTreeSet<_>>();
        let mut affected_media_keys = assignments
            .iter()
            .map(|assignment| assignment.media_key.clone())
            .collect::<BTreeSet<_>>();
        let mut inventory_digest = person_edit_inventory_digest("remove", &assignment_digest);
        let mut look_ids = Vec::new();
        let mut delta_counts = PersonEditDeltaCounts {
            persons: 1,
            assignments: assignment_count,
            ..PersonEditDeltaCounts::default()
        };
        let look_count = self.visit_values_by_field_unlocked(
            LOOK_TABLE,
            "look_id",
            "person_id",
            person_id,
            |look_id, look_row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    LOOK_TABLE,
                    look_id,
                    look_row,
                )?;
                if look_ids.len() < CORRECTION_ROW_LIMIT {
                    look_ids.push(look_id.to_string());
                }
                Ok(())
            },
        )?;
        delta_counts.looks = look_count;
        delta_counts.template_sets = self.visit_remove_person_descendants_unlocked(
            TEMPLATE_SET_TABLE,
            "set_id",
            person_id,
            |set_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    TEMPLATE_SET_TABLE,
                    set_id,
                    row,
                )
            },
        )?;
        delta_counts.trusted_members = self.visit_remove_person_descendants_unlocked(
            TRUSTED_MEMBER_TABLE,
            "membership_id",
            person_id,
            |membership_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    TRUSTED_MEMBER_TABLE,
                    membership_id,
                    row,
                )?;
                let member: TrustedTemplateMembership = serde_json::from_value(row.clone())
                    .map_err(|error| format!("decode remove-Person trusted member: {error}"))?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(member.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    let face: FaceObservation =
                        self.require_unlocked(FACE_TABLE, &member.face_id, "FaceObservation")?;
                    affected_media_keys.insert(face.media_key);
                }
                Ok(())
            },
        )?;
        delta_counts.trusted_search = self.visit_remove_person_descendants_unlocked(
            TRUSTED_SEARCH_TABLE,
            "membership_id",
            person_id,
            |membership_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    TRUSTED_SEARCH_TABLE,
                    membership_id,
                    row,
                )?;
                let search: TrustedSearchEmbedding = serde_json::from_value(row.clone())
                    .map_err(|error| format!("decode remove-Person trusted search: {error}"))?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(search.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    let face: FaceObservation =
                        self.require_unlocked(FACE_TABLE, &search.face_id, "FaceObservation")?;
                    affected_media_keys.insert(face.media_key);
                }
                Ok(())
            },
        )?;
        delta_counts.constraints = self.visit_values_by_field_unlocked(
            CONSTRAINT_TABLE,
            "constraint_id",
            "person_id",
            person_id,
            |constraint_id, row| {
                hash_person_edit_inventory_row(
                    &mut inventory_digest,
                    CONSTRAINT_TABLE,
                    constraint_id,
                    row,
                )?;
                let constraint: CannotLinkConstraint = serde_json::from_value(row.clone())
                    .map_err(|error| format!("decode remove-Person constraint: {error}"))?;
                if affected_face_ids.len() < CORRECTION_ROW_LIMIT {
                    affected_face_ids.insert(constraint.face_id.clone());
                }
                if affected_media_keys.len() < CORRECTION_ROW_LIMIT {
                    let face: FaceObservation =
                        self.require_unlocked(FACE_TABLE, &constraint.face_id, "FaceObservation")?;
                    affected_media_keys.insert(face.media_key);
                }
                Ok(())
            },
        )?;
        delta_counts.suggestions = self.append_person_suggestion_inventory_unlocked(
            person_id,
            &mut inventory_digest,
            &mut affected_face_ids,
            &mut affected_media_keys,
        )?;
        let required_reversible_rows = delta_counts.checked_total()?;
        let inventory_complete = required_reversible_rows <= CORRECTION_ROW_LIMIT;
        let affected_reference_count = assignment_count
            .checked_add(delta_counts.trusted_members)
            .and_then(|count| count.checked_add(delta_counts.trusted_search))
            .and_then(|count| count.checked_add(delta_counts.constraints))
            .and_then(|count| count.checked_add(delta_counts.suggestions))
            .ok_or("remove-Person affected-reference count overflow")?;
        let bounded_affected_counts = (affected_face_ids.len(), affected_media_keys.len());
        let (face_ids, media_keys, look_ids) = if inventory_complete {
            look_ids.sort();
            look_ids.dedup();
            (
                affected_face_ids.into_iter().collect(),
                affected_media_keys.into_iter().collect(),
                look_ids,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let (affected_faces, affected_media) = if affected_reference_count <= CORRECTION_ROW_LIMIT {
            bounded_affected_counts
        } else {
            self.person_edit_affected_face_media_counts_unlocked(person_id)?
        };
        let affected_counts = PersonEditAffectedCounts {
            persons: 1,
            faces: affected_faces,
            media: affected_media,
            looks: delta_counts.looks,
        };
        let inventory_digest = finish_person_edit_inventory_digest(inventory_digest, &delta_counts);
        let execution = self.execution_state_unlocked()?;
        let mut person_revisions = BTreeMap::new();
        person_revisions.insert(person.person_id.clone(), person.revision);
        let mut preview = PersonEditPreview {
            preview_id: String::new(),
            kind: PersonEditKind::Remove {
                person_id: person.person_id.clone(),
            },
            person_ids: vec![person.person_id],
            face_ids,
            media_keys,
            look_ids,
            assignment_count,
            delta_counts,
            required_reversible_rows,
            affected_counts,
            inventory_complete,
            inventory_digest,
            correction_delta_row_limit: CORRECTION_ROW_LIMIT,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            person_revisions,
        };
        preview.preview_id = preview_digest(&preview)?;
        Ok(preview)
    }

    fn preview_split_person_unlocked(
        &self,
        source_person_id: &str,
        mut face_ids: Vec<String>,
        destination_name: &str,
    ) -> Result<PersonEditPreview, String> {
        if face_ids.is_empty() || face_ids.len() > CORRECTION_ROW_LIMIT {
            return Err("split must select between 1 and 4096 faces".to_string());
        }
        face_ids.sort();
        face_ids.dedup();
        let source: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let mut media_keys = Vec::new();
        let mut inventory_digest = person_edit_inventory_digest("split_person", "selected");
        for face_id in &face_ids {
            let assignment: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            if assignment.person_id != source_person_id {
                return Err("split selection contains a face outside the source Person".to_string());
            }
            hash_person_edit_inventory_row(
                &mut inventory_digest,
                ASSIGNMENT_TABLE,
                &assignment.assignment_id,
                &serde_json::to_value(&assignment).map_err(|error| error.to_string())?,
            )?;
            media_keys.push(assignment.media_key);
        }
        media_keys.sort();
        media_keys.dedup();
        let execution = self.execution_state_unlocked()?;
        let destination_person_id = split_person_id(
            source_person_id,
            &face_ids,
            destination_name,
            execution.identity_revision,
            execution.catalog_revision,
        )?;
        if self
            .get_one_unlocked::<Person>(PERSON_TABLE, &destination_person_id)?
            .is_some()
        {
            return Err("split destination Person already exists".to_string());
        }
        let mut person_revisions = BTreeMap::new();
        person_revisions.insert(source.person_id.clone(), source.revision);
        let suggestions =
            self.suggestion_removal_delta_count_unlocked(&face_ids, &mut inventory_digest)?;
        let (trusted_members, trusted_search) =
            self.trust_removal_delta_counts_unlocked(&face_ids, &mut inventory_digest)?;
        let delta_counts = PersonEditDeltaCounts {
            persons: 1,
            assignments: face_ids.len(),
            trusted_members,
            trusted_search,
            suggestions,
            ..PersonEditDeltaCounts::default()
        };
        let required_reversible_rows = delta_counts.checked_total()?;
        let affected_counts = PersonEditAffectedCounts {
            persons: 2,
            faces: face_ids.len(),
            media: media_keys.len(),
            looks: 0,
        };
        let inventory_digest = finish_person_edit_inventory_digest(inventory_digest, &delta_counts);
        let mut preview = PersonEditPreview {
            preview_id: String::new(),
            kind: PersonEditKind::Split {
                source_person_id: source.person_id.clone(),
                destination_person_id: destination_person_id.clone(),
                destination_name: destination_name.trim().to_string(),
            },
            person_ids: vec![source.person_id, destination_person_id],
            face_ids: face_ids.clone(),
            media_keys,
            look_ids: Vec::new(),
            assignment_count: face_ids.len(),
            delta_counts,
            required_reversible_rows,
            affected_counts,
            inventory_complete: true,
            inventory_digest,
            correction_delta_row_limit: CORRECTION_ROW_LIMIT,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            person_revisions,
        };
        preview.preview_id = preview_digest(&preview)?;
        Ok(preview)
    }

    fn preview_split_to_person_unlocked(
        &self,
        source_person_id: &str,
        target_person_id: &str,
        mut face_ids: Vec<String>,
    ) -> Result<PersonEditPreview, String> {
        if face_ids.is_empty() || face_ids.len() > BATCH_CORRECTION_FACE_LIMIT {
            return Err(format!(
                "split must select between 1 and {BATCH_CORRECTION_FACE_LIMIT} faces"
            ));
        }
        face_ids.sort();
        if face_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("split selection FaceIds must be unique".to_string());
        }
        let source: Person = self.require_unlocked(PERSON_TABLE, source_person_id, "Person")?;
        let target: Person = self.require_unlocked(PERSON_TABLE, target_person_id, "Person")?;
        let mut media_keys = Vec::new();
        let source_inventory_digest =
            self.person_assignment_inventory_preview_token_unlocked(source_person_id)?;
        let mut inventory_digest =
            person_edit_inventory_digest("split_to_person", &source_inventory_digest);
        for face_id in &face_ids {
            let assignment: Assignment =
                self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
            if assignment.person_id != source_person_id {
                return Err("split selection contains a face outside the source Person".to_string());
            }
            if self
                .get_one_unlocked::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &cannot_link_id(face_id, target_person_id),
                )?
                .is_some()
            {
                return Err(format!(
                    "face {face_id} is cannot-linked to the split target"
                ));
            }
            hash_person_edit_inventory_row(
                &mut inventory_digest,
                ASSIGNMENT_TABLE,
                &assignment.assignment_id,
                &serde_json::to_value(&assignment).map_err(|error| error.to_string())?,
            )?;
            media_keys.push(assignment.media_key);
        }
        media_keys.sort();
        media_keys.dedup();
        let execution = self.execution_state_unlocked()?;
        let mut person_revisions = BTreeMap::new();
        person_revisions.insert(source.person_id.clone(), source.revision);
        person_revisions.insert(target.person_id.clone(), target.revision);
        let suggestions =
            self.suggestion_removal_delta_count_unlocked(&face_ids, &mut inventory_digest)?;
        let (trusted_members, trusted_search) =
            self.trust_removal_delta_counts_unlocked(&face_ids, &mut inventory_digest)?;
        let delta_counts = PersonEditDeltaCounts {
            assignments: face_ids.len(),
            trusted_members,
            trusted_search,
            suggestions,
            ..PersonEditDeltaCounts::default()
        };
        let required_reversible_rows = delta_counts.checked_total()?;
        let affected_counts = PersonEditAffectedCounts {
            persons: 2,
            faces: face_ids.len(),
            media: media_keys.len(),
            looks: 0,
        };
        let inventory_digest = finish_person_edit_inventory_digest(inventory_digest, &delta_counts);
        let mut preview = PersonEditPreview {
            preview_id: String::new(),
            kind: PersonEditKind::SplitToPerson {
                source_person_id: source.person_id.clone(),
                target_person_id: target.person_id.clone(),
            },
            person_ids: vec![source.person_id, target.person_id],
            face_ids: face_ids.clone(),
            media_keys,
            look_ids: Vec::new(),
            assignment_count: face_ids.len(),
            delta_counts,
            required_reversible_rows,
            affected_counts,
            inventory_complete: true,
            inventory_digest,
            correction_delta_row_limit: CORRECTION_ROW_LIMIT,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            person_revisions,
        };
        preview.preview_id = preview_digest(&preview)?;
        Ok(preview)
    }

    fn materialize_undo_value_unlocked(
        &self,
        row: &CorrectionRowDelta,
        undo_id: &str,
        execution: &PersistedExecutionState,
        face_revisions: &BTreeMap<String, u64>,
        person_revisions: &BTreeMap<String, u64>,
    ) -> Result<Option<Value>, String> {
        let Some(mut before) = row.before.clone() else {
            return Ok(None);
        };
        match row.table {
            CorrectionTable::Person => {
                let mut person: Person = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo Person: {error}"))?;
                person.revision = person_revisions
                    .get(&person.person_id)
                    .copied()
                    .unwrap_or_else(|| person.revision.saturating_add(1));
                person.catalog_revision = execution.catalog_revision;
                person.updated_at = now();
                before = serde_json::to_value(person).map_err(|error| error.to_string())?;
            }
            CorrectionTable::Look => {
                let mut look: Look = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo Look: {error}"))?;
                look.revision = look
                    .revision
                    .checked_add(1)
                    .ok_or("Look revision overflow")?;
                look.updated_at = now();
                before = serde_json::to_value(look).map_err(|error| error.to_string())?;
            }
            CorrectionTable::TemplateSet => {}
            CorrectionTable::VideoObservation => {
                let mut observation: StoredVideoObservation =
                    serde_json::from_value(before).map_err(|e| e.to_string())?;
                let current = self.require_unlocked::<StoredVideoObservation>(
                    super::video::VIDEO_OBSERVATION_TABLE,
                    &row.stable_id,
                    "video undo observation",
                )?;
                observation.revision = current
                    .revision
                    .checked_add(1)
                    .ok_or("video undo revision overflow")?;
                before = serde_json::to_value(observation).map_err(|e| e.to_string())?;
            }
            CorrectionTable::Face => {
                let mut face: FaceObservation = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo FaceObservation: {error}"))?;
                face.face_revision = face_revisions
                    .get(&face.face_id)
                    .copied()
                    .unwrap_or_else(|| face.face_revision.saturating_add(1));
                face.updated_at = now();
                before = serde_json::to_value(face).map_err(|error| error.to_string())?;
            }
            CorrectionTable::Assignment => {
                let mut assignment: Assignment = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo Assignment: {error}"))?;
                // Undo may restore model-derived evidence, but it must not
                // rewrite that evidence against current Face/Person revisions
                // or pretend the undo operation produced it. Preserving the
                // exact generation-bound row leaves it stale until the model
                // legitimately recomputes it. Operator evidence, by contrast,
                // remains durable and follows the restored canonical rows.
                if assignment.state != AssignmentState::CommittedStrictAutomatic.as_str() {
                    assignment.operation_id = undo_id.to_string();
                    assignment.updated_at = now();
                    assignment.face_revision = if let Some(revision) =
                        face_revisions.get(&assignment.face_id).copied()
                    {
                        revision
                    } else {
                        self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &assignment.face_id)?
                            .map(|face| face.face_revision)
                            .unwrap_or(assignment.face_revision)
                    };
                    assignment.person_revision = if let Some(revision) =
                        person_revisions.get(&assignment.person_id).copied()
                    {
                        revision
                    } else {
                        self.get_one_unlocked::<Person>(PERSON_TABLE, &assignment.person_id)?
                            .map(|person| person.revision)
                            .unwrap_or(assignment.person_revision)
                    };
                }
                before = serde_json::to_value(assignment).map_err(|error| error.to_string())?;
            }
            CorrectionTable::Constraint => {
                let mut constraint: CannotLinkConstraint = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo cannot-link: {error}"))?;
                constraint.operation_id = undo_id.to_string();
                before = serde_json::to_value(constraint).map_err(|error| error.to_string())?;
            }
            CorrectionTable::Disposition => {
                let mut disposition: FaceDisposition = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo face disposition: {error}"))?;
                disposition.operation_id = undo_id.to_string();
                disposition.face_revision = face_revisions
                    .get(&disposition.face_id)
                    .copied()
                    .unwrap_or(disposition.face_revision);
                disposition.updated_at = now();
                before = serde_json::to_value(disposition).map_err(|error| error.to_string())?;
            }
            CorrectionTable::Embedding => {
                if value_omits_portable_vector(&before) {
                    return Ok(None);
                }
                let mut embedding: FaceEmbedding = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo FaceEmbedding: {error}"))?;
                embedding.face_revision = face_revisions
                    .get(&embedding.face_id)
                    .copied()
                    .unwrap_or(embedding.face_revision);
                before = serde_json::to_value(embedding).map_err(|error| error.to_string())?;
            }
            CorrectionTable::TrustedMember => {
                let mut membership: TrustedTemplateMembership = serde_json::from_value(before)
                    .map_err(|error| format!("decode undo trusted membership: {error}"))?;
                membership.face_revision = face_revisions
                    .get(&membership.face_id)
                    .copied()
                    .unwrap_or(membership.face_revision);
                membership.operation_id = undo_id.to_string();
                before = serde_json::to_value(membership).map_err(|error| error.to_string())?;
            }
            CorrectionTable::TrustedSearch => {
                if value_omits_portable_vector(&before) {
                    return Ok(None);
                }
            }
            CorrectionTable::Suggestion => {
                // Suggestions are immutable model-derived provenance. Restoring
                // their exact pre-correction row intentionally leaves any
                // changed Face/Person revision stale instead of reviving it.
            }
        }
        Ok(Some(before))
    }
}

fn value_omits_portable_vector(value: &Value) -> bool {
    value.get("vector").is_none()
        && value.get("vector_omitted").and_then(Value::as_bool) == Some(true)
}

fn correction_row_omits_portable_vector(row: &CorrectionRowDelta) -> bool {
    matches!(
        row.table,
        CorrectionTable::Embedding | CorrectionTable::TrustedSearch
    ) && row
        .before
        .as_ref()
        .or(row.after.as_ref())
        .is_some_and(value_omits_portable_vector)
}

fn portable_derived_row_value(table: &CorrectionTable, value: &Value) -> Result<Value, String> {
    if !matches!(
        table,
        CorrectionTable::Embedding | CorrectionTable::TrustedSearch
    ) {
        return Ok(value.clone());
    }
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or("portable derived correction row is not an object")?;
    object.remove("vector");
    object.insert("vector_omitted".to_string(), Value::Bool(true));
    Ok(value)
}

fn display_point_to_source(x: f32, y: f32, orientation: ExifOrientation) -> (f32, f32) {
    match orientation {
        ExifOrientation::Normal => (x, y),
        ExifOrientation::MirrorHorizontal => (1.0 - x, y),
        ExifOrientation::Rotate180 => (1.0 - x, 1.0 - y),
        ExifOrientation::MirrorVertical => (x, 1.0 - y),
        ExifOrientation::Transpose => (y, x),
        ExifOrientation::Rotate90Clockwise => (y, 1.0 - x),
        ExifOrientation::Transverse => (1.0 - y, 1.0 - x),
        ExifOrientation::Rotate90CounterClockwise => (1.0 - y, x),
    }
}

fn prepare_manual_observation(input: &ManualFaceInput) -> Result<FaceObservation, String> {
    validate_media_key(&input.media_key)?;
    validate_text("media fingerprint", &input.media_fingerprint)?;
    validate_text("pose bucket", &input.pose_bucket)?;
    if !input.quality.is_finite() {
        return Err("manual face quality must be finite".to_string());
    }
    if input.source_width == 0 || input.source_height == 0 {
        return Err("manual face source dimensions must be non-zero".to_string());
    }
    if !input.alignment_valid && input.embedding.is_some() {
        return Err("invalid manual alignment must not produce an embedding".to_string());
    }
    let source_region = input
        .display_region
        .display_to_source(input.exif_orientation)?;
    let source_index =
        manual_source_index(&input.media_key, &input.media_fingerprint, source_region)?;
    let landmarks = input
        .display_landmarks
        .iter()
        .map(|point| {
            if point.len() != 2
                || point
                    .iter()
                    .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            {
                return Err("manual landmarks must contain finite normalized x/y pairs".to_string());
            }
            let (x, y) = display_point_to_source(point[0], point[1], input.exif_orientation);
            Ok(vec![x, y])
        })
        .collect::<Result<Vec<_>, String>>()?;
    let face_id = manual_face_id(
        &input.media_key,
        &input.media_fingerprint,
        source_index,
        source_region,
    )?;
    let timestamp = now();
    let face = FaceObservation {
        face_id,
        media_key: input.media_key.clone(),
        media_fingerprint: input.media_fingerprint.clone(),
        source_index,
        source_width: Some(input.source_width),
        source_height: Some(input.source_height),
        exif_orientation: Some(input.exif_orientation.code()),
        bounds_normalized: vec![
            source_region.x,
            source_region.y,
            source_region.width,
            source_region.height,
        ],
        landmarks_normalized: landmarks,
        alignment_valid: input.alignment_valid,
        quality: input.quality,
        pose_bucket: input.pose_bucket.clone(),
        operator_owned: true,
        schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
        face_revision: 1,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    };
    validate_face(&face)?;
    Ok(face)
}

fn serialize_optional<T: Serialize>(value: &Option<T>) -> Result<Option<Value>, String> {
    value
        .as_ref()
        .map(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
        .transpose()
}

fn batch_delta_counts(rows: &[CorrectionRowDelta]) -> Result<BatchCorrectionDeltaCounts, String> {
    let mut counts = BatchCorrectionDeltaCounts::default();
    for row in rows {
        let count = match &row.table {
            CorrectionTable::Person => &mut counts.persons,
            CorrectionTable::Look => &mut counts.looks,
            CorrectionTable::TemplateSet => &mut counts.template_sets,
            CorrectionTable::Face => &mut counts.faces,
            CorrectionTable::Embedding => &mut counts.embeddings,
            CorrectionTable::Assignment => &mut counts.assignments,
            CorrectionTable::Constraint => &mut counts.constraints,
            CorrectionTable::TrustedMember => &mut counts.trusted_members,
            CorrectionTable::TrustedSearch => &mut counts.trusted_search,
            CorrectionTable::Disposition => &mut counts.dispositions,
            CorrectionTable::Suggestion => &mut counts.suggestions,
            CorrectionTable::VideoObservation => &mut counts.faces,
        };
        *count = count
            .checked_add(1)
            .ok_or("batch correction per-table row count overflow")?;
    }
    Ok(counts)
}

fn batch_affected_identity_ids(
    source_person_id: Option<&str>,
    target_person_id: Option<&str>,
    rows: &[CorrectionRowDelta],
) -> (Vec<String>, Vec<String>) {
    let mut people = BTreeSet::new();
    let mut looks = BTreeSet::new();
    people.extend(source_person_id.map(str::to_string));
    people.extend(target_person_id.map(str::to_string));
    for row in rows {
        for value in [row.before.as_ref(), row.after.as_ref()]
            .into_iter()
            .flatten()
        {
            for field in ["person_id", "candidate_person_id"] {
                if let Some(id) = value.get(field).and_then(Value::as_str) {
                    people.insert(id.to_string());
                }
            }
            if let Some(id) = value.get("look_id").and_then(Value::as_str) {
                looks.insert(id.to_string());
            }
        }
    }
    (people.into_iter().collect(), looks.into_iter().collect())
}

fn batch_delta_digest(rows: &[CorrectionRowDelta]) -> Result<String, String> {
    let payload = serde_json::to_vec(&("wp084-batch-delta-v1", rows))
        .map_err(|error| format!("encode batch correction delta digest: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}

#[allow(clippy::too_many_arguments)]
fn batch_provenance_digest(
    action: BatchCorrectionAction,
    source_person_id: Option<&str>,
    target_person_id: Option<&str>,
    face_ids: &[String],
    fences: &[CorrectionFence],
    candidate_provenance: &[(String, Option<Assignment>, Option<Suggestion>)],
    delta_digest: &str,
    identity_revision: u64,
    catalog_revision: u64,
    model_generation: &str,
) -> Result<String, String> {
    let payload = serde_json::to_vec(&(
        "wp084-batch-provenance-v1",
        action,
        source_person_id,
        target_person_id,
        face_ids,
        fences,
        candidate_provenance,
        delta_digest,
        MATCH_SCHEMA_GENERATION,
        model_generation,
        identity_revision,
        catalog_revision,
    ))
    .map_err(|error| format!("encode batch correction provenance digest: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}

fn batch_preview_digest(preview: &BatchCorrectionPreview) -> Result<String, String> {
    let mut clone = preview.clone();
    clone.preview_id.clear();
    let payload = serde_json::to_vec(&("wp084-batch-preview-v1", clone))
        .map_err(|error| format!("encode batch correction preview: {error}"))?;
    Ok(format!(
        "batch-{}-{:x}",
        preview.action.as_str(),
        Sha256::digest(payload)
    ))
}

fn validate_batch_selection(face_ids: &[String], fences: &[CorrectionFence]) -> Result<(), String> {
    if face_ids.is_empty() {
        return Err("batch correction requires at least one FaceId".to_string());
    }
    if face_ids.len() > BATCH_CORRECTION_FACE_LIMIT {
        return Err(format!(
            "batch correction exceeds the bounded {BATCH_CORRECTION_FACE_LIMIT}-face limit"
        ));
    }
    if face_ids.len() != fences.len() {
        return Err("batch correction requires exactly one fence per FaceId".to_string());
    }
    if face_ids
        .iter()
        .any(|face_id| validate_text("FaceId", face_id).is_err())
    {
        return Err("batch correction contains an invalid FaceId".to_string());
    }
    if face_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("batch FaceIds must be strictly sorted and unique".to_string());
    }
    if face_ids
        .iter()
        .zip(fences)
        .any(|(face_id, fence)| face_id != &fence.face_id)
    {
        return Err("batch correction fences must match canonical FaceId order".to_string());
    }
    Ok(())
}

fn ensure_within_delta_limit(required_reversible_rows: usize) -> Result<(), String> {
    if required_reversible_rows > CORRECTION_ROW_LIMIT {
        Err(format!(
            "correction requires {} reversible rows, exceeding the global {CORRECTION_ROW_LIMIT}-row correction-delta limit; use bounded face selection/split actions",
            required_reversible_rows
        ))
    } else {
        Ok(())
    }
}

fn validate_delta_bound(rows: &[CorrectionRowDelta]) -> Result<(), String> {
    ensure_within_delta_limit(rows.len())
}

fn require_exact_preview_delta_count(
    preview: &PersonEditPreview,
    materialized_rows: usize,
) -> Result<(), String> {
    if materialized_rows != preview.required_reversible_rows {
        return Err(format!(
            "stale Person edit reversible-row count: preview required {}, materialized {materialized_rows}",
            preview.required_reversible_rows
        ));
    }
    Ok(())
}

fn materialize_rows(
    rows: &[CorrectionRowDelta],
    upserts: &mut Vec<(String, String, Value)>,
    deletes: &mut Vec<(String, String)>,
) {
    for row in rows {
        if let Some(value) = &row.after {
            upserts.push((
                row.table.name().to_string(),
                row.stable_id.clone(),
                value.clone(),
            ));
        } else {
            deletes.push((row.table.name().to_string(), row.stable_id.clone()));
        }
    }
}

fn dedup_owned_deletes(rows: &mut Vec<(String, String)>) {
    rows.sort();
    rows.dedup();
}

fn preview_digest(preview: &PersonEditPreview) -> Result<String, String> {
    let mut clone = preview.clone();
    clone.preview_id.clear();
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&clone).map_err(|error| error.to_string())?)
    ))
}

fn person_inventory_digest(source_person_id: &str) -> Sha256 {
    let mut digest = Sha256::new();
    digest.update(b"wp084-person-assignment-inventory-v3\0");
    digest.update((source_person_id.len() as u64).to_be_bytes());
    digest.update(source_person_id.as_bytes());
    digest
}

fn hash_person_inventory_assignment(
    digest: &mut Sha256,
    assignment: &Assignment,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(assignment)
        .map_err(|error| format!("encode Person assignment inventory row: {error}"))?;
    digest.update((encoded.len() as u64).to_be_bytes());
    digest.update(encoded);
    Ok(())
}

fn finish_person_inventory_digest(mut digest: Sha256, row_count: u64) -> String {
    digest.update(row_count.to_be_bytes());
    format!("{:x}", digest.finalize())
}

fn person_edit_inventory_digest(action: &str, assignment_digest: &str) -> Sha256 {
    let mut digest = Sha256::new();
    digest.update(b"wp084-person-edit-inventory-v1\0");
    digest.update((action.len() as u64).to_be_bytes());
    digest.update(action.as_bytes());
    digest.update((assignment_digest.len() as u64).to_be_bytes());
    digest.update(assignment_digest.as_bytes());
    digest
}

fn hash_person_edit_inventory_row(
    digest: &mut Sha256,
    table: &str,
    stable_id: &str,
    row: &Value,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(row)
        .map_err(|error| format!("encode Person-edit inventory row: {error}"))?;
    for value in [table.as_bytes(), stable_id.as_bytes(), encoded.as_slice()] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    Ok(())
}

fn finish_person_edit_inventory_digest(
    mut digest: Sha256,
    counts: &PersonEditDeltaCounts,
) -> String {
    let encoded = serde_json::to_vec(counts).expect("closed Person-edit counts serialize");
    digest.update((encoded.len() as u64).to_be_bytes());
    digest.update(encoded);
    format!("{:x}", digest.finalize())
}

pub(super) fn correction_media_mapping_id(operation_id: &str, media_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(operation_id.as_bytes());
    hash.update([0]);
    hash.update(media_key.as_bytes());
    format!("correction-media-{:x}", hash.finalize())
}

pub(super) fn suggestion_source_provenance_id(row: &SuggestionSourceProvenance) -> String {
    let mut hash = Sha256::new();
    hash.update(b"wp085-suggestion-source-provenance-v1");
    hash.update([0]);
    for value in [
        row.operation_id.as_str(),
        row.operation_kind.as_str(),
        row.suggestion_id.as_str(),
        row.face_id.as_str(),
        row.candidate_person_id.as_str(),
        row.media_fingerprint.as_str(),
        row.model_generation.as_str(),
        row.job_id.as_str(),
        row.suggestion_created_at.as_str(),
        row.embedding_id.as_str(),
        row.embedding_created_at.as_str(),
        row.schema_generation.as_str(),
        row.operation_created_at.as_str(),
    ] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    hash.update(row.face_revision.to_le_bytes());
    hash.update(row.person_revision.to_le_bytes());
    hash.update(row.similarity_bits.to_le_bytes());
    for value in [
        row.calibration_generation.as_deref(),
        row.envelope_hash.as_deref(),
    ] {
        match value {
            Some(value) => {
                hash.update([1]);
                hash.update(value.as_bytes());
            }
            None => hash.update([0]),
        }
        hash.update([0]);
    }
    format!("suggestion-source-{:x}", hash.finalize())
}

pub(super) fn suggestion_is_provenance_source(
    operation_kind: &str,
    rows: &[CorrectionRowDelta],
    suggestion: &Suggestion,
) -> bool {
    let assignment_row = rows.iter().find(|candidate| {
        candidate.table == CorrectionTable::Assignment && candidate.stable_id == suggestion.face_id
    });
    match operation_kind {
        "same" | "batch_same" => assignment_row.is_some_and(|assignment| {
            assignment.before.is_none()
                && assignment.after.as_ref().is_some_and(|after| {
                    serde_json::from_value::<Assignment>(after.clone()).is_ok_and(|result| {
                        result.face_id == suggestion.face_id
                            && result.person_id == suggestion.candidate_person_id
                    })
                })
        }),
        "different" | "batch_different" => {
            assignment_row.is_none()
                && rows.iter().any(|candidate| {
                    candidate.table == CorrectionTable::Constraint
                        && candidate.after.as_ref().is_some_and(|after| {
                            serde_json::from_value::<CannotLinkConstraint>(after.clone()).is_ok_and(
                                |constraint| {
                                    constraint.face_id == suggestion.face_id
                                        && constraint.person_id == suggestion.candidate_person_id
                                },
                            )
                        })
                })
        }
        "change_person" => assignment_row.is_some_and(|assignment| {
            assignment.before.is_none()
                && rows.iter().any(|candidate| {
                    candidate.table == CorrectionTable::Constraint
                        && candidate.after.as_ref().is_some_and(|after| {
                            serde_json::from_value::<CannotLinkConstraint>(after.clone()).is_ok_and(
                                |constraint| {
                                    constraint.face_id == suggestion.face_id
                                        && constraint.person_id == suggestion.candidate_person_id
                                },
                            )
                        })
                })
        }),
        "batch_change_person" => false,
        _ => false,
    }
}

fn serialize_correction_operation_json<T: Serialize>(
    value: &T,
    field: &str,
) -> Result<String, String> {
    let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
    if encoded.len() > CORRECTION_OPERATION_JSON_MAX_BYTES {
        return Err(format!(
            "correction {field} requires {} bytes, exceeding the portable identity-bundle nested-string limit of {CORRECTION_OPERATION_JSON_MAX_BYTES} bytes; use a smaller bounded selection",
            encoded.len()
        ));
    }
    Ok(encoded)
}

fn serialize_identity_bundle_nested_json<T: Serialize>(
    value: &T,
    field: &str,
) -> Result<String, String> {
    let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
    if encoded.len() > super::exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES {
        return Err(format!(
            "correction {field} requires {} bytes, exceeding the portable identity-bundle nested-string limit of {} bytes",
            encoded.len(),
            super::exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES
        ));
    }
    Ok(encoded)
}

fn manual_face_id(
    media_key: &str,
    media_fingerprint: &str,
    source_index: u32,
    region: NormalizedRegion,
) -> Result<String, String> {
    let payload = serde_json::to_vec(&(
        media_key,
        media_fingerprint,
        source_index,
        region,
        MATCH_SCHEMA_GENERATION,
    ))
    .map_err(|error| error.to_string())?;
    Ok(format!("manual-face-{:x}", Sha256::digest(payload)))
}

fn manual_source_index(
    media_key: &str,
    media_fingerprint: &str,
    region: NormalizedRegion,
) -> Result<u32, String> {
    let payload = serde_json::to_vec(&(media_key, media_fingerprint, region))
        .map_err(|error| error.to_string())?;
    let digest = Sha256::digest(payload);
    Ok(u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) | 0x8000_0000)
}

fn split_person_id(
    source_person_id: &str,
    face_ids: &[String],
    destination_name: &str,
    identity_revision: u64,
    catalog_revision: u64,
) -> Result<String, String> {
    let payload = serde_json::to_vec(&(
        source_person_id,
        face_ids,
        destination_name.trim(),
        identity_revision,
        catalog_revision,
    ))
    .map_err(|error| error.to_string())?;
    Ok(format!("split-person-{:x}", Sha256::digest(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial-wp084-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn close(root: &Path, store: MatchStore) {
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_media_fingerprint(label: &str) -> String {
        format!("{:x}", Sha256::digest(label.as_bytes()))
    }

    fn manual_face(id: &str, media_key: &str) -> FaceObservation {
        FaceObservation {
            face_id: id.to_string(),
            media_key: media_key.to_string(),
            media_fingerprint: test_media_fingerprint(&format!("test-media:{media_key}")),
            source_index: 0,
            source_width: None,
            source_height: None,
            exif_orientation: None,
            bounds_normalized: vec![0.1, 0.2, 0.3, 0.4],
            landmarks_normalized: vec![vec![0.2, 0.3], vec![0.4, 0.3]],
            alignment_valid: true,
            quality: 0.9,
            pose_bucket: "frontal".to_string(),
            operator_owned: true,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: now(),
            updated_at: now(),
        }
    }

    fn portable_assignment_fixture(
        person: &Person,
        media_key: &str,
        index: usize,
    ) -> (
        FaceObservation,
        Assignment,
        MatchOperation,
        CorrectionMediaOperation,
    ) {
        let face_id = format!("portable-boundary-face-{index:04}");
        let operation_id = format!("portable-boundary-direct-{index:04}");
        let timestamp = "2026-08-25T00:00:00.000Z".to_string();
        let mut face = manual_face(&face_id, media_key);
        // Every FaceObservation for one canonical media file must carry the
        // same media fingerprint even when the fixture varies FaceId.
        face.media_fingerprint = format!("{:x}", Sha256::digest(b"portable-boundary-media"));
        face.source_index = u32::try_from(index).unwrap();
        let assignment = Assignment {
            assignment_id: face_id.clone(),
            face_id: face_id.clone(),
            person_id: person.person_id.clone(),
            media_key: media_key.to_string(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "operator_review".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: operation_id.clone(),
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: "assign_operator_confirmed".to_string(),
            face_id: Some(face_id),
            person_id: Some(person.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&assignment).unwrap(),
            reversible: true,
            created_at: timestamp.clone(),
        };
        let mapping = CorrectionMediaOperation {
            mapping_id: correction_media_mapping_id(&operation_id, media_key),
            media_key: media_key.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            operation_id,
            kind: "assign_operator_confirmed".to_string(),
            created_at: timestamp,
        };
        (face, assignment, operation, mapping)
    }

    fn seed_portable_assignments(
        store: &MatchStore,
        fixtures: &[(
            FaceObservation,
            Assignment,
            MatchOperation,
            CorrectionMediaOperation,
        )],
    ) {
        // Keep fixture setup bounded while leaving the correction under test
        // as one atomic transaction.
        for fixture_page in fixtures.chunks(16) {
            let mut owned = Vec::<(String, String, Value)>::new();
            for (face, assignment, operation, mapping) in fixture_page {
                owned.push((
                    FACE_TABLE.to_string(),
                    face.face_id.clone(),
                    serde_json::to_value(face).unwrap(),
                ));
                owned.push((
                    ASSIGNMENT_TABLE.to_string(),
                    assignment.assignment_id.clone(),
                    serde_json::to_value(assignment).unwrap(),
                ));
                owned.push((
                    OPERATION_TABLE.to_string(),
                    operation.operation_id.clone(),
                    serde_json::to_value(operation).unwrap(),
                ));
                owned.push((
                    CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                    mapping.mapping_id.clone(),
                    serde_json::to_value(mapping).unwrap(),
                ));
            }
            let borrowed = owned
                .iter()
                .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
                .collect::<Vec<_>>();
            store
                .transactional_upserts_deletes_unlocked(&borrowed, &[])
                .unwrap();
        }
    }

    fn remove_person_boundary_rows(
        person: &Person,
        fixtures: &[(
            FaceObservation,
            Assignment,
            MatchOperation,
            CorrectionMediaOperation,
        )],
    ) -> Vec<CorrectionRowDelta> {
        let mut rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Person,
            stable_id: person.person_id.clone(),
            before: Some(serde_json::to_value(person).unwrap()),
            after: None,
        }];
        rows.extend(
            fixtures
                .iter()
                .map(|(_, assignment, _, _)| CorrectionRowDelta {
                    table: CorrectionTable::Assignment,
                    stable_id: assignment.assignment_id.clone(),
                    before: Some(serde_json::to_value(assignment).unwrap()),
                    after: None,
                }),
        );
        rows
    }

    fn remove_person_boundary_json(
        person: &Person,
        fixtures: &[(
            FaceObservation,
            Assignment,
            MatchOperation,
            CorrectionMediaOperation,
        )],
        media_key: &str,
    ) -> Result<(String, String), String> {
        let rows = remove_person_boundary_rows(person, fixtures);
        let before_json = serialize_correction_operation_json(
            &rows
                .iter()
                .map(|row| (&row.table, &row.stable_id, &row.before))
                .collect::<Vec<_>>(),
            "before_json",
        )?;
        let envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: "remove_person".to_string(),
            rows,
            face_ids: fixtures
                .iter()
                .map(|(face, _, _, _)| face.face_id.clone())
                .collect(),
            media_keys: vec![media_key.to_string()],
            identity_changed: true,
            catalog_changed: true,
        };
        let after_json = serialize_correction_operation_json(&envelope, "after_json")?;
        Ok((before_json, after_json))
    }

    fn seed_manual_media_authority(
        store: &MatchStore,
        root: &Path,
        media_key: &str,
        media_fingerprint: &str,
    ) -> ManualMediaAuthority {
        let media_root = root.join(format!("manual-media-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&media_root).unwrap();
        let source_path = media_root.join("source.jpg");
        std::fs::write(&source_path, b"manual-source").unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let execution = store.execution_state().unwrap();
        let timestamp = now();
        let job = IndexJob {
            job_id: new_id("manual-test-job"),
            root_key: configured.root_id,
            lifecycle: JobLifecycle::Running.as_str().to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            discovered: 1,
            completed: 0,
            failed: 0,
            skipped: 0,
            failure_code: None,
            failure_message: None,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let asset = JobAsset {
            asset_id: new_id("manual-test-asset"),
            job_id: job.job_id.clone(),
            media_key: media_key.to_string(),
            source_path: Some(source_path.to_string_lossy().to_string()),
            media_fingerprint: media_fingerprint.to_string(),
            next_stage: JobStage::Detect.as_str().to_string(),
            completed_stages: vec![JobStage::Discover.as_str().to_string()],
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: timestamp,
        };
        store.upsert_json(JOB_TABLE, &job.job_id, &job).unwrap();
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        store
            .manual_media_authority(media_key, media_fingerprint)
            .unwrap()
    }

    fn confirm(store: &MatchStore, face_id: &str, person: &Person) -> Assignment {
        let face: FaceObservation = store.require(FACE_TABLE, face_id, "Face").unwrap();
        let execution = store.execution_state().unwrap();
        let prior = store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
            .unwrap();
        store
            .review_same(
                face_id,
                &person.person_id,
                &OperatorMutationFence {
                    face_id: face_id.to_string(),
                    person_id: person.person_id.clone(),
                    face_revision: face.face_revision,
                    person_revision: person.revision,
                    assignment_operation_id: prior.map(|value| value.operation_id),
                    identity_revision: execution.identity_revision,
                    catalog_revision: execution.catalog_revision,
                },
            )
            .unwrap()
    }

    fn correction_for_target(
        store: &MatchStore,
        face_id: &str,
        target: &Person,
    ) -> CorrectionFence {
        store
            .correction_fence_for(face_id, vec![target.person_id.clone()])
            .unwrap()
    }

    fn ensure_candidate_provenance(store: &MatchStore, face: &FaceObservation) -> String {
        let timestamp = now();
        let generation = ModelGeneration {
            generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            state: "active".to_string(),
            validated: true,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        store
            .upsert_json(GENERATION_TABLE, UNCONFIGURED_MODEL_GENERATION, &generation)
            .unwrap();
        let execution = store.execution_state().unwrap();
        let job_id = format!("wp084-candidate-job-{}", face.face_id);
        let asset = JobAsset {
            asset_id: format!("wp084-candidate-asset-{}", face.face_id),
            job_id: job_id.clone(),
            media_key: face.media_key.clone(),
            source_path: None,
            media_fingerprint: face.media_fingerprint.clone(),
            next_stage: JobStage::Suggest.as_str().to_string(),
            completed_stages: vec![
                JobStage::Discover.as_str().to_string(),
                JobStage::Detect.as_str().to_string(),
                JobStage::Align.as_str().to_string(),
                JobStage::Embed.as_str().to_string(),
            ],
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: timestamp.clone(),
        };
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, UNCONFIGURED_MODEL_GENERATION),
            face_id: face.face_id.clone(),
            vector: vec![0.0; EMBEDDING_DIM],
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: job_id.clone(),
            active: true,
            created_at: timestamp,
        };
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        job_id
    }

    fn suggest_for_review(store: &MatchStore, face_id: &str, person: &Person) -> Suggestion {
        let face: FaceObservation = store.require(FACE_TABLE, face_id, "Face").unwrap();
        let job_id = ensure_candidate_provenance(store, &face);
        let suggestion = Suggestion {
            suggestion_id: suggestion_id(face_id, &person.person_id),
            face_id: face_id.to_string(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.91,
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: face.media_fingerprint,
            face_revision: face.face_revision,
            person_revision: person.revision,
            job_id,
            created_at: now(),
        };
        store
            .upsert_json(SUGGESTION_TABLE, &suggestion.suggestion_id, &suggestion)
            .unwrap();
        suggestion
    }

    struct CanonicalTrustedFixture {
        set: TrustedTemplateSet,
        embedding: FaceEmbedding,
        membership: TrustedTemplateMembership,
        search: TrustedSearchEmbedding,
    }

    fn seed_canonical_trusted_reference(
        store: &MatchStore,
        person: &Person,
        look: &Look,
        face_id: &str,
        suffix: &str,
    ) -> CanonicalTrustedFixture {
        let face: FaceObservation = store
            .require(FACE_TABLE, face_id, "trusted fixture FaceObservation")
            .unwrap();
        let assignment: Assignment = store
            .require(ASSIGNMENT_TABLE, face_id, "trusted fixture Assignment")
            .unwrap();
        assert_eq!(assignment.person_id, person.person_id);
        assert_eq!(assignment.look_id.as_deref(), Some(look.look_id.as_str()));
        let model_generation = format!("model-{suffix}");
        store
            .register_model_generation(&model_generation, true)
            .unwrap();
        let timestamp = now();
        let set = TrustedTemplateSet {
            set_id: format!("set-{suffix}"),
            look_id: look.look_id.clone(),
            name: format!("Trusted {suffix}"),
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let embedding = FaceEmbedding {
            embedding_id: format!("embedding-{suffix}"),
            face_id: face.face_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: model_generation.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: format!("embedding-job-{suffix}"),
            active: true,
            created_at: timestamp.clone(),
        };
        let membership = TrustedTemplateMembership {
            membership_id: format!("membership-{suffix}"),
            set_id: set.set_id.clone(),
            look_id: look.look_id.clone(),
            face_id: face.face_id.clone(),
            authorized: true,
            alignment_valid: true,
            quality_passed: true,
            pose_passed: true,
            diversity_passed: true,
            provenance: "operator_trusted_reference".to_string(),
            model_generation: model_generation.clone(),
            embedding_id: embedding.embedding_id.clone(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            quality_score: face.quality,
            quality_threshold: 0.8,
            pose_bucket: face.pose_bucket.clone(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
            operation_id: format!("trusted-seed-{suffix}"),
            created_at: timestamp.clone(),
        };
        let search = TrustedSearchEmbedding {
            membership_id: membership.membership_id.clone(),
            person_id: person.person_id.clone(),
            look_id: look.look_id.clone(),
            face_id: face.face_id,
            embedding_id: embedding.embedding_id.clone(),
            vector: embedding.vector.clone(),
            model_generation,
            created_at: timestamp,
        };
        store
            .transactional_upserts_deletes(
                &[
                    (
                        TEMPLATE_SET_TABLE,
                        set.set_id.as_str(),
                        serde_json::to_value(&set).unwrap(),
                    ),
                    (
                        EMBEDDING_TABLE,
                        embedding.embedding_id.as_str(),
                        serde_json::to_value(&embedding).unwrap(),
                    ),
                    (
                        TRUSTED_MEMBER_TABLE,
                        membership.membership_id.as_str(),
                        serde_json::to_value(&membership).unwrap(),
                    ),
                    (
                        TRUSTED_SEARCH_TABLE,
                        search.membership_id.as_str(),
                        serde_json::to_value(&search).unwrap(),
                    ),
                ],
                &[],
            )
            .unwrap();
        CanonicalTrustedFixture {
            set,
            embedding,
            membership,
            search,
        }
    }

    fn strict_assignment_without_revision_bump(
        store: &MatchStore,
        face_id: &str,
        person: &Person,
    ) -> Assignment {
        let face: FaceObservation = store.require(FACE_TABLE, face_id, "Face").unwrap();
        ensure_candidate_provenance(store, &face);
        let timestamp = now();
        let calibration_generation = "wp084-test-calibration";
        let envelope_hash = "e".repeat(64);
        let calibration_created_at = store
            .get_one::<CalibrationActivation>(CALIBRATION_TABLE, calibration_generation)
            .unwrap()
            .map(|activation| activation.created_at)
            .unwrap_or_else(|| timestamp.clone());
        let mut activation = CalibrationActivation {
            calibration_generation: calibration_generation.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            envelope_hash: envelope_hash.clone(),
            runtime_configuration_digest: strict_runtime_configuration_digest(100, 50),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: "a".repeat(64),
            gallery_members_digest: "b".repeat(64),
            verifier_artifact_id: "VAL-WP-084-TEST".to_string(),
            contract_sha256: "c".repeat(64),
            raw_records_sha256: "d".repeat(64),
            evidence_digest: "f".repeat(64),
            review_digest: "1".repeat(64),
            automatic_threshold: 0.90,
            suggestion_threshold: 0.80,
            runner_up_margin: 0.10,
            minimum_quality: 0.70,
            candidate_k: 100,
            rerank_k: 50,
            people_max: 10_000,
            looks_per_person_max: 8,
            templates_per_look_max: 16,
            total_templates_max: 100_000,
            verifier_verdict: "pass".to_string(),
            independent_review_verdict: "pass".to_string(),
            wp084_runtime_ready: true,
            wp087_release_ready: true,
            active: true,
            invalidation_reason: None,
            created_at: calibration_created_at,
            updated_at: timestamp.clone(),
        };
        activation.activation_integrity_digest =
            calibration_activation_integrity_digest(&activation);
        store
            .upsert_json(CALIBRATION_TABLE, calibration_generation, &activation)
            .unwrap();
        let assignment = Assignment {
            assignment_id: face_id.to_string(),
            face_id: face_id.to_string(),
            person_id: person.person_id.clone(),
            media_key: face.media_key,
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::CommittedStrictAutomatic
                .as_str()
                .to_string(),
            provenance: "strict_recognition_v1".to_string(),
            locked: false,
            model_generation: Some(UNCONFIGURED_MODEL_GENERATION.to_string()),
            calibration_generation: Some(calibration_generation.to_string()),
            envelope_hash: Some(envelope_hash),
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: new_id("operation"),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        store
            .upsert_json(ASSIGNMENT_TABLE, face_id, &assignment)
            .unwrap();
        assignment
    }

    #[test]
    fn exif_mapping_is_orientation_safe_and_bounded() {
        let region = NormalizedRegion {
            x: 0.1,
            y: 0.2,
            width: 0.3,
            height: 0.4,
        };
        let mapped = region
            .display_to_source(ExifOrientation::Rotate90Clockwise)
            .unwrap();
        assert!((mapped.x - 0.2).abs() < 1.0e-6);
        assert!((mapped.y - 0.6).abs() < 1.0e-6);
        assert!((mapped.width - 0.4).abs() < 1.0e-6);
        assert!((mapped.height - 0.3).abs() < 1.0e-6);
        assert!(NormalizedRegion {
            x: 0.9,
            y: 0.9,
            width: 0.2,
            height: 0.2,
        }
        .display_to_source(ExifOrientation::Normal)
        .is_err());
    }

    #[test]
    fn invalid_manual_alignment_cannot_persist_an_embedding() {
        let root = workspace("invalid-alignment");
        let store = MatchStore::open(&root).unwrap();
        let error = store
            .create_manual_face(ManualFaceInput {
                expected_schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                expected_model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
                expected_catalog_revision: store.execution_state().unwrap().catalog_revision,
                media_key: "media/manual-invalid.jpg".to_string(),
                media_fingerprint: "manual-invalid-fingerprint".to_string(),
                authority: ManualMediaAuthorityFence {
                    asset_id: "unused-invalid-asset".to_string(),
                    job_id: "unused-invalid-job".to_string(),
                    source_path: "unused-invalid-source".to_string(),
                },
                source_width: 640,
                source_height: 480,
                display_region: NormalizedRegion {
                    x: 0.1,
                    y: 0.1,
                    width: 0.4,
                    height: 0.4,
                },
                exif_orientation: ExifOrientation::Normal,
                display_landmarks: Vec::new(),
                alignment_valid: false,
                quality: 0.0,
                pose_bucket: "invalid".to_string(),
                embedding: Some(ManualEmbeddingInput {
                    vector: vec![1.0; EMBEDDING_DIM],
                    model_generation: "untrusted".to_string(),
                }),
            })
            .unwrap_err();
        assert!(error.contains("must not produce an embedding"));
        close(&root, store);
    }

    #[test]
    fn invalid_alignment_manual_assignment_is_atomic_unsorted_and_reversible() {
        let root = workspace("invalid-alignment-assigned");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Manual", Vec::new()).unwrap();
        let media_fingerprint = test_media_fingerprint("manual-assigned-fingerprint");
        let authority = seed_manual_media_authority(
            &store,
            &root,
            "media/manual-assigned.jpg",
            &media_fingerprint,
        );
        let execution = store.execution_state().unwrap();
        let created = store
            .create_manual_face_and_assign(
                ManualFaceInput {
                    expected_schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    expected_model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
                    expected_catalog_revision: execution.catalog_revision,
                    media_key: "media/manual-assigned.jpg".to_string(),
                    media_fingerprint,
                    authority: authority.fence,
                    source_width: 480,
                    source_height: 640,
                    display_region: NormalizedRegion {
                        x: 0.1,
                        y: 0.2,
                        width: 0.3,
                        height: 0.4,
                    },
                    exif_orientation: ExifOrientation::Rotate90Clockwise,
                    display_landmarks: Vec::new(),
                    alignment_valid: false,
                    quality: 0.0,
                    pose_bucket: "invalid".to_string(),
                    embedding: None,
                },
                &person.person_id,
                person.revision,
            )
            .unwrap();
        assert!(created.embedding_id.is_none());
        let assignment: Assignment = store
            .require(ASSIGNMENT_TABLE, &created.face.face_id, "Assignment")
            .unwrap();
        assert_eq!(assignment.placement, "unsorted");
        assert_eq!(assignment.state, "operator_confirmed");
        store
            .undo_correction(&created.operation.operation_id)
            .unwrap();
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, &created.face.face_id)
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn undo_manual_face_rejects_later_face_dependency_then_succeeds_after_supported_cleanup() {
        let root = workspace("undo-manual-face-dependent-assignment");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Manual Dependency", Vec::new())
            .unwrap();
        let media_fingerprint = test_media_fingerprint("manual-dependent-fingerprint");
        let authority = seed_manual_media_authority(
            &store,
            &root,
            "media/manual-dependent.jpg",
            &media_fingerprint,
        );
        let execution = store.execution_state().unwrap();
        let created = store
            .create_manual_face(ManualFaceInput {
                expected_schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                expected_model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
                expected_catalog_revision: execution.catalog_revision,
                media_key: "media/manual-dependent.jpg".to_string(),
                media_fingerprint,
                authority: authority.fence,
                source_width: 640,
                source_height: 480,
                display_region: NormalizedRegion {
                    x: 0.1,
                    y: 0.1,
                    width: 0.4,
                    height: 0.4,
                },
                exif_orientation: ExifOrientation::Normal,
                display_landmarks: Vec::new(),
                alignment_valid: false,
                quality: 0.0,
                pose_bucket: "invalid".to_string(),
                embedding: None,
            })
            .unwrap();
        suggest_for_review(&store, &created.face.face_id, &person);
        confirm(&store, &created.face.face_id, &person);

        let face_before: FaceObservation = store
            .require(FACE_TABLE, &created.face.face_id, "manual Face")
            .unwrap();
        let assignment_before: Assignment = store
            .require(ASSIGNMENT_TABLE, &created.face.face_id, "later assignment")
            .unwrap();
        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();
        let error = store
            .undo_correction(&created.operation.operation_id)
            .unwrap_err();
        assert!(error.contains("undo dependency conflict"));
        assert!(error.contains(ASSIGNMENT_TABLE));
        assert_eq!(
            store
                .require::<FaceObservation>(FACE_TABLE, &created.face.face_id, "manual Face")
                .unwrap(),
            face_before
        );
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, &created.face.face_id, "later assignment")
                .unwrap(),
            assignment_before
        );
        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);

        let cleanup_fence = store
            .correction_fence_for(&created.face.face_id, vec![person.person_id.clone()])
            .unwrap();
        store
            .remove_assignment_correction(&created.face.face_id, &person.person_id, &cleanup_fence)
            .unwrap();
        let embedding_key = embedding_id(&created.face.face_id, UNCONFIGURED_MODEL_GENERATION);
        store
            .transactional_upserts_deletes(&[], &[(EMBEDDING_TABLE, embedding_key.as_str())])
            .unwrap();
        store
            .undo_correction(&created.operation.operation_id)
            .unwrap();
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, &created.face.face_id)
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn viewer_snapshot_is_exact_joined_and_embedding_free() {
        let root = workspace("viewer");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_face(manual_face("face-viewer", "media/viewer.jpg"))
            .unwrap();
        let snapshot = store.media_faces("media/viewer.jpg").unwrap();
        assert_eq!(snapshot.total_faces, 1);
        assert_eq!(snapshot.rows[0].face.face_id, "face-viewer");
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("vector"));
        assert!(!encoded.contains("crop"));
        assert!(!encoded.contains("database"));
        close(&root, store);
    }

    #[test]
    fn strict_viewer_truth_requires_current_calibration_binding_and_nonfuture_time() {
        let root = workspace("viewer-strict-calibration-binding");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Strict Viewer", Vec::new()).unwrap();

        store
            .create_face(manual_face(
                "face-strict-envelope",
                "media/strict-envelope.jpg",
            ))
            .unwrap();
        let mut envelope_assignment =
            strict_assignment_without_revision_bump(&store, "face-strict-envelope", &person);
        assert!(
            store.media_faces("media/strict-envelope.jpg").unwrap().rows[0]
                .assignment
                .is_some()
        );
        envelope_assignment.envelope_hash = Some("0".repeat(64));
        store
            .upsert_json(
                ASSIGNMENT_TABLE,
                &envelope_assignment.assignment_id,
                &envelope_assignment,
            )
            .unwrap();
        assert!(
            store.media_faces("media/strict-envelope.jpg").unwrap().rows[0]
                .assignment
                .is_none()
        );

        store
            .create_face(manual_face("face-strict-future", "media/strict-future.jpg"))
            .unwrap();
        let mut future_assignment =
            strict_assignment_without_revision_bump(&store, "face-strict-future", &person);
        future_assignment.created_at = "2999-01-01T00:00:00Z".to_string();
        store
            .upsert_json(
                ASSIGNMENT_TABLE,
                &future_assignment.assignment_id,
                &future_assignment,
            )
            .unwrap();
        assert!(
            store.media_faces("media/strict-future.jpg").unwrap().rows[0]
                .assignment
                .is_none()
        );
        assert!(store
            .not_sure_correction(
                "face-strict-future",
                &person.person_id,
                &correction_for_target(&store, "face-strict-future", &person),
            )
            .unwrap_err()
            .contains("stale committed_strict_automatic provenance"));
        close(&root, store);
    }

    #[test]
    fn viewer_undo_candidates_survive_restart_and_disappear_after_undo() {
        let root = workspace("viewer-undo-restart");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Undo", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-undo", "media/undo.jpg"))
            .unwrap();
        suggest_for_review(&store, "face-undo", &person);
        let fence = store
            .correction_fence_for("face-undo", vec![person.person_id.clone()])
            .unwrap();
        let correction = store
            .same_correction("face-undo", &person.person_id, &fence)
            .unwrap();
        let mapping: CorrectionMediaOperation = store
            .require(
                CORRECTION_MEDIA_OPERATION_TABLE,
                &correction_media_mapping_id(&correction.operation_id, "media/undo.jpg"),
                "correction media mapping",
            )
            .unwrap();
        assert_eq!(
            mapping.media_fingerprint,
            test_media_fingerprint("test-media:media/undo.jpg")
        );
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let snapshot = reopened.media_faces("media/undo.jpg").unwrap();
        assert_eq!(snapshot.undo_candidates.len(), 1);
        assert_eq!(
            snapshot.undo_candidates[0].operation_id,
            correction.operation_id
        );
        reopened.undo_correction(&correction.operation_id).unwrap();
        assert!(reopened
            .media_faces("media/undo.jpg")
            .unwrap()
            .undo_candidates
            .is_empty());
        close(&root, reopened);
    }

    #[test]
    fn batch_mapping_rejects_ambiguous_fingerprints_for_one_media_key() {
        let root = workspace("batch-mapping-ambiguous-fingerprint");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Ambiguous media evidence", Vec::new())
            .unwrap();
        let media_key = "media/ambiguous.jpg";
        let first = manual_face("face-ambiguous-a", media_key);
        let mut second = manual_face("face-ambiguous-b", media_key);
        second.media_fingerprint = test_media_fingerprint("different-canonical-media-bytes");
        store.create_face(first.clone()).unwrap();
        store.create_face(second.clone()).unwrap();
        let face_ids = vec![first.face_id.clone(), second.face_id.clone()];
        let fences = face_ids
            .iter()
            .map(|face_id| correction_for_target(&store, face_id, &person))
            .collect::<Vec<_>>();
        let preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::IgnoreFace,
                None,
                None,
                face_ids,
                fences,
            )
            .unwrap();

        let error = store.apply_batch_correction(&preview).unwrap_err();
        assert!(error.contains("ambiguous fingerprint evidence"), "{error}");
        assert!(store
            .get_one::<MatchOperation>(OPERATION_TABLE, &preview.planned_operation_id)
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn correction_history_follows_proven_media_rekey_through_viewer_and_exchange() {
        let root = workspace("rekey-correction-history");
        let store = MatchStore::open(&root).unwrap();
        let media_root = root.join("portable-media-root");
        std::fs::create_dir_all(media_root.join("media")).unwrap();
        let configured_root = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let person = store.create_person("Rekey history", Vec::new()).unwrap();
        let media_bytes = b"portable-media";
        let mut face = manual_face("face-rekey-history", "media/history-old.jpg");
        face.media_fingerprint = format!("{:x}", Sha256::digest(media_bytes));
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        confirm(&store, "face-rekey-history", &person);
        let fence = correction_for_target(&store, "face-rekey-history", &person);
        let correction = store
            .same_person_new_look_correction(
                "face-rekey-history",
                &person.person_id,
                "Rekeyed look",
                &fence,
            )
            .unwrap();

        store
            .rekey_media(
                "media/history-old.jpg",
                "media/history-new.jpg",
                &fingerprint,
            )
            .unwrap();
        std::fs::write(media_root.join("media/history-new.jpg"), media_bytes).unwrap();
        let moved = store.media_faces("media/history-new.jpg").unwrap();
        assert_eq!(moved.undo_candidates.len(), 1);
        assert_eq!(
            moved.undo_candidates[0].operation_id,
            correction.operation_id
        );

        let export_path = root.join("rekey-history-bundle.json");
        store.export_identity_bundle(&export_path).unwrap();
        store.undo_correction(&correction.operation_id).unwrap();
        assert!(store
            .media_faces("media/history-new.jpg")
            .unwrap()
            .undo_candidates
            .is_empty());

        let imported_root = workspace("rekey-correction-history-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured_root.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = imported
            .preview_identity_bundle_import(&export_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(plan.unresolved_root_ids.is_empty());
        assert!(plan.unresolved_media_keys.is_empty());
        imported.apply_identity_bundle_import(plan).unwrap();
        let imported_viewer = imported.media_faces("media/history-new.jpg").unwrap();
        assert_eq!(imported_viewer.undo_candidates.len(), 1);
        assert_eq!(
            imported_viewer.undo_candidates[0].operation_id,
            correction.operation_id
        );
        imported.undo_correction(&correction.operation_id).unwrap();
        assert!(imported
            .media_faces("media/history-new.jpg")
            .unwrap()
            .undo_candidates
            .is_empty());

        close(&imported_root, imported);
        close(&root, store);
    }

    #[test]
    fn imported_projected_manual_embedding_regenerates_rekeys_and_reexports() {
        let source_root = workspace("portable-manual-embedding-rekey-source");
        let source = MatchStore::open(&source_root).unwrap();
        let old_media_key = "source.jpg";
        let new_media_key = "manual-rekeyed.jpg";
        let media_fingerprint = format!("{:x}", Sha256::digest(b"manual-source"));
        let model_generation = "portable-manual-embedding-model";
        let authority =
            seed_manual_media_authority(&source, &source_root, old_media_key, &media_fingerprint);
        let relocation_root = std::fs::canonicalize(&authority.root_path)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let configured_root = source
            .list::<MatchIndexRoot>(ROOT_CONFIG_TABLE)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        source
            .register_model_generation(model_generation, true)
            .unwrap();
        source.activate_model_generation(model_generation).unwrap();
        let execution = source.execution_state().unwrap();
        let created = source
            .create_manual_face(ManualFaceInput {
                expected_schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                expected_model_generation: model_generation.to_string(),
                expected_catalog_revision: execution.catalog_revision,
                media_key: old_media_key.to_string(),
                media_fingerprint: media_fingerprint.clone(),
                authority: authority.fence,
                source_width: 640,
                source_height: 480,
                display_region: NormalizedRegion {
                    x: 0.1,
                    y: 0.1,
                    width: 0.4,
                    height: 0.4,
                },
                exif_orientation: ExifOrientation::Normal,
                display_landmarks: Vec::new(),
                alignment_valid: true,
                quality: 0.9,
                pose_bucket: "frontal".to_string(),
                embedding: Some(ManualEmbeddingInput {
                    vector: vec![1.0; EMBEDDING_DIM],
                    model_generation: model_generation.to_string(),
                }),
            })
            .unwrap();
        let embedding_id = created.embedding_id.unwrap();
        let regenerated_embedding: FaceEmbedding = source
            .require(EMBEDDING_TABLE, &embedding_id, "manual FaceEmbedding")
            .unwrap();
        let bundle_path = source_root.join("portable-manual-embedding.json");
        source.export_identity_bundle(&bundle_path).unwrap();

        let imported_root = workspace("portable-manual-embedding-rekey-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(configured_root.root_id, relocation_root)]);
        let plan = imported
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        imported.apply_identity_bundle_import(plan).unwrap();
        assert!(imported
            .get_one::<FaceEmbedding>(EMBEDDING_TABLE, &embedding_id)
            .unwrap()
            .is_none());
        imported
            .upsert_json(EMBEDDING_TABLE, &embedding_id, &regenerated_embedding)
            .unwrap();
        imported
            .rekey_media(old_media_key, new_media_key, &media_fingerprint)
            .unwrap();
        let reexport_path = imported_root.join("portable-manual-embedding-rekeyed.json");
        imported.export_identity_bundle(&reexport_path).unwrap();
        let reexported = std::fs::read_to_string(reexport_path).unwrap();
        assert!(reexported.contains(new_media_key));
        assert!(reexported.contains("vector_omitted"));

        close(&imported_root, imported);
        close(&source_root, source);
    }

    #[test]
    fn imported_projected_trusted_search_regenerates_rekeys_and_reexports() {
        let source_root = workspace("portable-trusted-search-rekey-source");
        let source = MatchStore::open(&source_root).unwrap();
        let media_root = source_root.join("portable-trusted-media");
        std::fs::create_dir_all(&media_root).unwrap();
        let old_media_key = "trusted-old.jpg";
        let new_media_key = "trusted-rekeyed.jpg";
        std::fs::write(media_root.join(old_media_key), b"portable-trusted-media").unwrap();
        let configured_root = source
            .configure_index_root(&media_root, Vec::new())
            .unwrap();
        let person = source
            .create_person("Portable trusted rekey", Vec::new())
            .unwrap();
        let look = source.create_look(&person.person_id, "Reference").unwrap();
        let face = manual_face("face-portable-trusted-rekey", old_media_key);
        let media_fingerprint = format!("{:x}", Sha256::digest(b"portable-trusted-media"));
        let mut face = face;
        face.media_fingerprint.clone_from(&media_fingerprint);
        source.create_face(face.clone()).unwrap();
        suggest_for_review(&source, &face.face_id, &person);
        confirm(&source, &face.face_id, &person);
        source
            .move_to_look_correction(
                &face.face_id,
                &look.look_id,
                &correction_for_target(&source, &face.face_id, &person),
            )
            .unwrap();
        let model_generation = "portable-trusted-search-model";
        source
            .register_model_generation(model_generation, true)
            .unwrap();
        source.activate_model_generation(model_generation).unwrap();
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, model_generation),
            face_id: face.face_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: model_generation.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: "portable-trusted-search-embedding-job".to_string(),
            active: true,
            created_at: now(),
        };
        source
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        let set = source
            .create_template_set(&look.look_id, "Portable trusted set")
            .unwrap();
        source
            .authorize_trusted_reference(
                &set.set_id,
                &face.face_id,
                true,
                TrustedEligibility {
                    provenance: "operator_trusted_reference".to_string(),
                    model_generation: model_generation.to_string(),
                    embedding_id: embedding.embedding_id.clone(),
                    policy_version: TRUSTED_POLICY_VERSION.to_string(),
                },
                &source
                    .operator_mutation_fence(&face.face_id, &person.person_id)
                    .unwrap(),
            )
            .unwrap();
        let trusted_search: TrustedSearchEmbedding = source
            .require(
                TRUSTED_SEARCH_TABLE,
                &trusted_member_id(&set.set_id, &face.face_id),
                "TrustedSearchEmbedding",
            )
            .unwrap();
        let removal = source
            .remove_assignment_correction(
                &face.face_id,
                &person.person_id,
                &correction_for_target(&source, &face.face_id, &person),
            )
            .unwrap();
        source.undo_correction(&removal.operation_id).unwrap();
        assert_eq!(
            source
                .require::<TrustedSearchEmbedding>(
                    TRUSTED_SEARCH_TABLE,
                    &trusted_search.membership_id,
                    "restored TrustedSearchEmbedding",
                )
                .unwrap(),
            trusted_search
        );
        let bundle_path = source_root.join("portable-trusted-search.json");
        source.export_identity_bundle(&bundle_path).unwrap();

        let imported_root = workspace("portable-trusted-search-rekey-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured_root.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = imported
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        imported.apply_identity_bundle_import(plan).unwrap();
        assert_eq!(imported.count(TRUSTED_SEARCH_TABLE).unwrap(), 0);
        imported
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert_eq!(imported.reconcile_trusted_search().unwrap(), 1);
        imported
            .rekey_media(old_media_key, new_media_key, &media_fingerprint)
            .unwrap();
        let reexport_path = imported_root.join("portable-trusted-search-rekeyed.json");
        imported.export_identity_bundle(&reexport_path).unwrap();
        let reexported = std::fs::read_to_string(reexport_path).unwrap();
        assert!(reexported.contains(new_media_key));
        assert!(reexported.contains("vector_omitted"));

        close(&imported_root, imported);
        close(&source_root, source);
    }

    #[test]
    fn not_sure_histories_are_mapped_rekeyed_and_portable() {
        let root = workspace("rekey-not-sure-history");
        let store = MatchStore::open(&root).unwrap();
        let media_root = root.join("portable-media-root");
        std::fs::create_dir_all(media_root.join("media")).unwrap();
        let configured_root = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let person = store.create_person("Not sure history", Vec::new()).unwrap();
        let mut face = manual_face("face-rekey-not-sure", "media/not-sure-old.jpg");
        face.media_fingerprint = format!("{:x}", Sha256::digest(b"portable-media"));
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        suggest_for_review(&store, "face-rekey-not-sure", &person);
        let fence = correction_for_target(&store, "face-rekey-not-sure", &person);
        let single = store
            .not_sure_correction("face-rekey-not-sure", &person.person_id, &fence)
            .unwrap();
        let batch_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::NotSure,
                Some(&person.person_id),
                None,
                vec!["face-rekey-not-sure".to_string()],
                vec![fence],
            )
            .unwrap();
        let batch = store.apply_batch_correction(&batch_preview).unwrap();
        assert_eq!(store.count(CORRECTION_MEDIA_OPERATION_TABLE).unwrap(), 2);
        for operation_id in [&single.operation_id, &batch.operation_id] {
            let legacy_mapping_id =
                correction_media_mapping_id(operation_id, "media/not-sure-old.jpg");
            store
                .transactional_upserts_deletes_unlocked(
                    &[],
                    &[(CORRECTION_MEDIA_OPERATION_TABLE, legacy_mapping_id.as_str())],
                )
                .unwrap();
        }
        assert_eq!(store.count(CORRECTION_MEDIA_OPERATION_TABLE).unwrap(), 0);

        store
            .rekey_media(
                "media/not-sure-old.jpg",
                "media/not-sure-new.jpg",
                &fingerprint,
            )
            .unwrap();
        std::fs::write(media_root.join("media/not-sure-new.jpg"), b"portable-media").unwrap();
        for operation_id in [&single.operation_id, &batch.operation_id] {
            let operation: MatchOperation = store
                .require(OPERATION_TABLE, operation_id, "Not-sure operation")
                .unwrap();
            let envelope: CorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            assert_eq!(envelope.media_keys, vec!["media/not-sure-new.jpg"]);
            assert!(!operation.reversible);
            assert!(store
                .get_one::<CorrectionMediaOperation>(
                    CORRECTION_MEDIA_OPERATION_TABLE,
                    &correction_media_mapping_id(operation_id, "media/not-sure-new.jpg"),
                )
                .unwrap()
                .is_some());
        }

        let export_path = root.join("not-sure-rekey.json");
        store.export_identity_bundle(&export_path).unwrap();
        let imported_root = workspace("rekey-not-sure-history-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured_root.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = imported
            .preview_identity_bundle_import(&export_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        imported.apply_identity_bundle_import(plan).unwrap();
        assert_eq!(imported.count(CORRECTION_MEDIA_OPERATION_TABLE).unwrap(), 2);
        close(&imported_root, imported);
        close(&root, store);
    }

    #[test]
    fn legacy_batch_rekey_pages_past_4096_unrelated_operations() {
        let root = workspace("rekey-paged-legacy-batch-history");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Paged legacy batch", Vec::new())
            .unwrap();
        let face = manual_face("face-paged-legacy-batch", "media/paged-old.jpg");
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        suggest_for_review(&store, "face-paged-legacy-batch", &person);
        let fence = correction_for_target(&store, "face-paged-legacy-batch", &person);
        let preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::NotSure,
                Some(&person.person_id),
                None,
                vec!["face-paged-legacy-batch".to_string()],
                vec![fence],
            )
            .unwrap();
        let batch = store.apply_batch_correction(&preview).unwrap();
        let legacy_mapping_id =
            correction_media_mapping_id(&batch.operation_id, "media/paged-old.jpg");
        store
            .transactional_upserts_deletes_unlocked(
                &[],
                &[(CORRECTION_MEDIA_OPERATION_TABLE, legacy_mapping_id.as_str())],
            )
            .unwrap();

        let unrelated_envelope = CorrectionDeltaEnvelope {
            version: CORRECTION_DELTA_VERSION,
            kind: "batch_not_sure".to_string(),
            rows: Vec::new(),
            face_ids: vec!["unrelated-face".to_string()],
            media_keys: vec!["media/unrelated.jpg".to_string()],
            identity_changed: false,
            catalog_changed: false,
        };
        let unrelated_after = serde_json::to_string(&unrelated_envelope).unwrap();
        for page_start in (0..4097).step_by(256) {
            let page_end = (page_start + 256).min(4097);
            let owned = (page_start..page_end)
                .map(|index| {
                    let operation_id = format!("0000-unrelated-batch-{index:05}");
                    let operation = MatchOperation {
                        operation_id: operation_id.clone(),
                        kind: "correction_batch_not_sure".to_string(),
                        face_id: None,
                        person_id: None,
                        before_json: "[]".to_string(),
                        after_json: unrelated_after.clone(),
                        reversible: false,
                        created_at: "2026-08-25T00:00:00.000Z".to_string(),
                    };
                    (
                        OPERATION_TABLE.to_string(),
                        operation_id,
                        serde_json::to_value(operation).unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            let borrowed = owned
                .iter()
                .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
                .collect::<Vec<_>>();
            store
                .transactional_upserts_deletes_unlocked(&borrowed, &[])
                .unwrap();
        }

        store
            .rekey_media("media/paged-old.jpg", "media/paged-new.jpg", &fingerprint)
            .unwrap();
        assert!(store
            .get_one::<CorrectionMediaOperation>(
                CORRECTION_MEDIA_OPERATION_TABLE,
                &correction_media_mapping_id(&batch.operation_id, "media/paged-new.jpg"),
            )
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn legacy_unmapped_direct_assignment_history_is_discovered_and_rekeyed() {
        let root = workspace("rekey-legacy-direct-history");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Legacy direct", Vec::new()).unwrap();
        let face = manual_face("face-legacy-direct", "media/direct-old.jpg");
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        suggest_for_review(&store, "face-legacy-direct", &person);
        confirm(&store, "face-legacy-direct", &person);
        let assignment: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-legacy-direct", "assignment")
            .unwrap();
        let old_mapping_id =
            correction_media_mapping_id(&assignment.operation_id, "media/direct-old.jpg");
        store
            .transactional_upserts_deletes_unlocked(
                &[],
                &[(CORRECTION_MEDIA_OPERATION_TABLE, old_mapping_id.as_str())],
            )
            .unwrap();

        store
            .rekey_media("media/direct-old.jpg", "media/direct-new.jpg", &fingerprint)
            .unwrap();
        let operation: MatchOperation = store
            .require(
                OPERATION_TABLE,
                &assignment.operation_id,
                "direct operation",
            )
            .unwrap();
        let recorded: Assignment = serde_json::from_str(&operation.after_json).unwrap();
        assert_eq!(recorded.media_key, "media/direct-new.jpg");
        assert!(store
            .get_one::<CorrectionMediaOperation>(
                CORRECTION_MEDIA_OPERATION_TABLE,
                &correction_media_mapping_id(&assignment.operation_id, "media/direct-new.jpg"),
            )
            .unwrap()
            .is_some());
        let export_path = root.join("legacy-direct-rekey.json");
        store.export_identity_bundle(&export_path).unwrap();
        close(&root, store);
    }

    #[test]
    fn undone_correction_history_rekeys_source_and_undo_locators_without_reactivation() {
        let root = workspace("rekey-undone-correction-history");
        let store = MatchStore::open(&root).unwrap();
        let media_root = root.join("portable-media-root");
        std::fs::create_dir_all(media_root.join("media")).unwrap();
        let configured_root = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let person = store.create_person("Rekey undone", Vec::new()).unwrap();
        let mut face = manual_face("face-rekey-undone", "media/undone-old.jpg");
        face.media_fingerprint = format!("{:x}", Sha256::digest(b"portable-media"));
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        suggest_for_review(&store, "face-rekey-undone", &person);
        let fence = correction_for_target(&store, "face-rekey-undone", &person);
        let correction = store
            .same_correction("face-rekey-undone", &person.person_id, &fence)
            .unwrap();
        store.undo_correction(&correction.operation_id).unwrap();

        store
            .rekey_media("media/undone-old.jpg", "media/undone-new.jpg", &fingerprint)
            .unwrap();
        std::fs::write(media_root.join("media/undone-new.jpg"), b"portable-media").unwrap();

        let source: MatchOperation = store
            .require(
                OPERATION_TABLE,
                &correction.operation_id,
                "source correction",
            )
            .unwrap();
        let source_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&source.after_json).unwrap();
        assert_eq!(source_envelope.media_keys, vec!["media/undone-new.jpg"]);
        assert!(!source.before_json.contains("media/undone-old.jpg"));
        assert!(!source.after_json.contains("media/undone-old.jpg"));
        let undo: MatchOperation = store
            .require(
                OPERATION_TABLE,
                &format!("undo-{}", correction.operation_id),
                "undo correction",
            )
            .unwrap();
        let undo_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&undo.after_json).unwrap();
        assert_eq!(undo_envelope.media_keys, vec!["media/undone-new.jpg"]);
        assert!(!undo.after_json.contains("media/undone-old.jpg"));
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-rekey-undone")
            .unwrap()
            .is_none());
        let export_path = root.join("undone-rekey.json");
        store.export_identity_bundle(&export_path).unwrap();

        let imported_root = workspace("rekey-undone-correction-history-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured_root.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = imported
            .preview_identity_bundle_import(&export_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(plan.unresolved_root_ids.is_empty());
        assert!(plan.unresolved_media_keys.is_empty());
        imported.apply_identity_bundle_import(plan).unwrap();
        assert!(imported
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-rekey-undone")
            .unwrap()
            .is_none());
        assert!(imported
            .media_faces("media/undone-new.jpg")
            .unwrap()
            .undo_candidates
            .is_empty());
        let imported_source: MatchOperation = imported
            .require(
                OPERATION_TABLE,
                &correction.operation_id,
                "imported source correction",
            )
            .unwrap();
        let imported_undo: MatchOperation = imported
            .require(
                OPERATION_TABLE,
                &format!("undo-{}", correction.operation_id),
                "imported undo correction",
            )
            .unwrap();
        assert!(!imported_source.after_json.contains("media/undone-old.jpg"));
        assert!(!imported_undo.after_json.contains("media/undone-old.jpg"));

        close(&imported_root, imported);
        close(&root, store);
    }

    #[test]
    fn superseded_correction_rekey_preserves_historical_effect_and_updates_active_owner() {
        let root = workspace("rekey-superseded-correction-history");
        let store = MatchStore::open(&root).unwrap();
        let media_root = root.join("portable-media-root");
        std::fs::create_dir_all(media_root.join("media")).unwrap();
        let configured_root = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let person = store.create_person("Rekey superseded", Vec::new()).unwrap();
        let mut face = manual_face("face-rekey-superseded", "media/superseded-old.jpg");
        face.media_fingerprint = format!("{:x}", Sha256::digest(b"portable-media"));
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        confirm(&store, "face-rekey-superseded", &person);
        let first_fence = correction_for_target(&store, "face-rekey-superseded", &person);
        let first = store
            .same_person_new_look_correction(
                "face-rekey-superseded",
                &person.person_id,
                "Historical look",
                &first_fence,
            )
            .unwrap();
        let second_fence = correction_for_target(&store, "face-rekey-superseded", &person);
        let second = store
            .same_person_new_look_correction(
                "face-rekey-superseded",
                &person.person_id,
                "Current look",
                &second_fence,
            )
            .unwrap();

        store
            .rekey_media(
                "media/superseded-old.jpg",
                "media/superseded-new.jpg",
                &fingerprint,
            )
            .unwrap();
        std::fs::write(
            media_root.join("media/superseded-new.jpg"),
            b"portable-media",
        )
        .unwrap();

        let first_operation: MatchOperation = store
            .require(OPERATION_TABLE, &first.operation_id, "first correction")
            .unwrap();
        let second_operation: MatchOperation = store
            .require(OPERATION_TABLE, &second.operation_id, "second correction")
            .unwrap();
        let first_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&first_operation.after_json).unwrap();
        let second_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&second_operation.after_json).unwrap();
        assert!(!first_operation
            .before_json
            .contains("media/superseded-old.jpg"));
        assert!(!second_operation
            .before_json
            .contains("media/superseded-old.jpg"));
        let first_assignment: Assignment = serde_json::from_value(
            first_envelope
                .rows
                .iter()
                .find(|row| row.table == CorrectionTable::Assignment)
                .and_then(|row| row.after.clone())
                .unwrap(),
        )
        .unwrap();
        let second_assignment: Assignment = serde_json::from_value(
            second_envelope
                .rows
                .iter()
                .find(|row| row.table == CorrectionTable::Assignment)
                .and_then(|row| row.after.clone())
                .unwrap(),
        )
        .unwrap();
        let live: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-rekey-superseded", "live assignment")
            .unwrap();
        assert_ne!(first_assignment.look_id, second_assignment.look_id);
        assert_eq!(first_assignment.media_key, "media/superseded-new.jpg");
        assert_eq!(second_assignment, live);
        assert_eq!(live.media_key, "media/superseded-new.jpg");
        let historical_look_id = first_assignment.look_id.clone();
        let export_path = root.join("superseded-rekey.json");
        store.export_identity_bundle(&export_path).unwrap();

        let imported_root = workspace("rekey-superseded-correction-history-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured_root.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = imported
            .preview_identity_bundle_import(&export_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(plan.unresolved_root_ids.is_empty());
        assert!(plan.unresolved_media_keys.is_empty());
        imported.apply_identity_bundle_import(plan).unwrap();
        let imported_first: MatchOperation = imported
            .require(
                OPERATION_TABLE,
                &first.operation_id,
                "imported first correction",
            )
            .unwrap();
        let imported_second: MatchOperation = imported
            .require(
                OPERATION_TABLE,
                &second.operation_id,
                "imported second correction",
            )
            .unwrap();
        let imported_first_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&imported_first.after_json).unwrap();
        let imported_second_envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&imported_second.after_json).unwrap();
        let imported_first_assignment: Assignment = serde_json::from_value(
            imported_first_envelope
                .rows
                .iter()
                .find(|row| row.table == CorrectionTable::Assignment)
                .and_then(|row| row.after.clone())
                .unwrap(),
        )
        .unwrap();
        let imported_second_assignment: Assignment = serde_json::from_value(
            imported_second_envelope
                .rows
                .iter()
                .find(|row| row.table == CorrectionTable::Assignment)
                .and_then(|row| row.after.clone())
                .unwrap(),
        )
        .unwrap();
        let imported_live: Assignment = imported
            .require(
                ASSIGNMENT_TABLE,
                "face-rekey-superseded",
                "imported live assignment",
            )
            .unwrap();
        assert_eq!(imported_first_assignment.look_id, historical_look_id);
        assert_eq!(
            imported_first_assignment.media_key,
            "media/superseded-new.jpg"
        );
        assert_eq!(imported_second_assignment, imported_live);
        assert_eq!(imported_live.media_key, "media/superseded-new.jpg");

        close(&imported_root, imported);
        close(&root, store);
    }

    #[test]
    fn face_disposition_rekeys_live_row_and_active_correction_atomically() {
        let root = workspace("rekey-face-disposition");
        let store = MatchStore::open(&root).unwrap();
        let face = manual_face("face-rekey-disposition", "media/disposition-old.jpg");
        let fingerprint = face.media_fingerprint.clone();
        store.create_face(face).unwrap();
        let fence = store.correction_fence("face-rekey-disposition").unwrap();
        let ignored = store.ignore_face("face-rekey-disposition", &fence).unwrap();

        store
            .rekey_media(
                "media/disposition-old.jpg",
                "media/disposition-new.jpg",
                &fingerprint,
            )
            .unwrap();

        let live: FaceDisposition = store
            .require(
                FACE_DISPOSITION_TABLE,
                "face-rekey-disposition",
                "face disposition",
            )
            .unwrap();
        assert_eq!(live.media_key, "media/disposition-new.jpg");
        assert_eq!(live.operation_id, ignored.operation_id);
        let operation: MatchOperation = store
            .require(OPERATION_TABLE, &ignored.operation_id, "ignore correction")
            .unwrap();
        let envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        let recorded: FaceDisposition = serde_json::from_value(
            envelope
                .rows
                .iter()
                .find(|row| row.table == CorrectionTable::Disposition)
                .and_then(|row| row.after.clone())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(recorded, live);
        store
            .export_identity_bundle(&root.join("disposition-rekey.json"))
            .unwrap();
        store.undo_correction(&ignored.operation_id).unwrap();
        assert!(store
            .get_one::<FaceDisposition>(FACE_DISPOSITION_TABLE, "face-rekey-disposition")
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn person_edit_digest_rejects_post_preview_strict_assignment() {
        let root = workspace("person-preview-token");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        let page = store.person_face_page(&source.person_id, 0, 256).unwrap();
        let preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(preview.preview_id, preview_digest(&preview).unwrap());

        store
            .create_face(manual_face(
                "face-post-preview-strict",
                "media/post-preview-strict.jpg",
            ))
            .unwrap();
        let revision_before = store.execution_state().unwrap().identity_revision;
        strict_assignment_without_revision_bump(&store, "face-post-preview-strict", &source);
        assert_eq!(
            store.execution_state().unwrap().identity_revision,
            revision_before,
            "regression must exercise the strict-automatic path which does not bump identity"
        );
        let refreshed = store.person_face_page(&source.person_id, 0, 256).unwrap();
        assert_ne!(page.preview_token, refreshed.preview_token);
        let refreshed_preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_ne!(preview.preview_id, refreshed_preview.preview_id);
        assert_eq!(
            refreshed_preview.preview_id,
            preview_digest(&refreshed_preview).unwrap()
        );
        let error = store.merge_people(&preview).unwrap_err();
        assert!(error.contains("stale merge preview"));
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &source.person_id)
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn merge_preview_and_recomputed_apply_reject_source_cannot_link_to_target_assignment_atomically(
    ) {
        let root = workspace("merge-source-constraint-target-assignment");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Merge Source", Vec::new()).unwrap();
        let target = store.create_person("Merge Target", Vec::new()).unwrap();
        let face_id = "merge-target-assigned-face";
        store
            .create_face(manual_face(face_id, "media/merge-target-assigned.jpg"))
            .unwrap();
        confirm(&store, face_id, &target);
        let valid_preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        let constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(face_id, &source.person_id),
            face_id: face_id.to_string(),
            person_id: source.person_id.clone(),
            operation_id: "merge-conflict-fixture".to_string(),
            operator_owned: true,
            created_at: now(),
        };
        store
            .upsert_json(CONSTRAINT_TABLE, &constraint.constraint_id, &constraint)
            .unwrap();
        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();
        let source_before: Person = store
            .require(PERSON_TABLE, &source.person_id, "source Person")
            .unwrap();
        let target_before: Person = store
            .require(PERSON_TABLE, &target.person_id, "target Person")
            .unwrap();
        let assignment_before: Assignment = store
            .require(ASSIGNMENT_TABLE, face_id, "target Assignment")
            .unwrap();

        let preview_error = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap_err();
        assert!(preview_error.contains("conflicts with its current assignment to the merge target"));
        let apply_error = store.merge_people(&valid_preview).unwrap_err();
        assert!(apply_error.contains("conflicts with its current assignment to the merge target"));
        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        assert_eq!(
            store
                .require::<Person>(PERSON_TABLE, &source.person_id, "source Person")
                .unwrap(),
            source_before
        );
        assert_eq!(
            store
                .require::<Person>(PERSON_TABLE, &target.person_id, "target Person")
                .unwrap(),
            target_before
        );
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "target Assignment")
                .unwrap(),
            assignment_before
        );
        assert_eq!(
            store
                .require::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &constraint.constraint_id,
                    "source constraint",
                )
                .unwrap(),
            constraint
        );
        close(&root, store);
    }

    #[test]
    fn split_to_person_action_digest_extends_inventory_fence_and_rejects_stale_inventory() {
        let root = workspace("split-reachable-preview");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        let other_target = store.create_person("Other Target", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-split-reachable", "media/split.jpg"))
            .unwrap();
        confirm(&store, "face-split-reachable", &source);
        store
            .create_face(manual_face("face-split-subset", "media/split-subset.jpg"))
            .unwrap();
        confirm(&store, "face-split-subset", &source);

        let page = store.person_face_page(&source.person_id, 0, 256).unwrap();
        let preview = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec!["face-split-reachable".to_string()],
            )
            .unwrap();
        let fences = vec![store
            .correction_fence_for(
                "face-split-reachable",
                vec![source.person_id.clone(), target.person_id.clone()],
            )
            .unwrap()];
        assert_ne!(preview.preview_id, page.preview_token);
        assert_eq!(preview.preview_id, preview_digest(&preview).unwrap());
        let other_target_preview = store
            .preview_split_to_person(
                &source.person_id,
                &other_target.person_id,
                vec!["face-split-reachable".to_string()],
            )
            .unwrap();
        assert_ne!(preview.preview_id, other_target_preview.preview_id);
        let other_subset_preview = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec!["face-split-subset".to_string()],
            )
            .unwrap();
        assert_ne!(preview.preview_id, other_subset_preview.preview_id);

        store
            .create_face(manual_face("face-split-late", "media/split-late.jpg"))
            .unwrap();
        strict_assignment_without_revision_bump(&store, "face-split-late", &source);
        let refreshed = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec!["face-split-reachable".to_string()],
            )
            .unwrap();
        assert_ne!(preview.preview_id, refreshed.preview_id);
        assert!(store
            .split_to_person(&preview, fences)
            .unwrap_err()
            .contains("stale split-to-Person preview"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "face-split-reachable", "Assignment")
                .unwrap()
                .person_id,
            source.person_id
        );
        close(&root, store);
    }

    #[test]
    fn split_to_person_revalidates_face_media_and_revision_fences_under_write_lock() {
        let root = workspace("split-atomic-face-fence");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        let face_id = "face-split-atomic-fence";
        store
            .create_face(manual_face(face_id, "media/split-before.jpg"))
            .unwrap();
        confirm(&store, face_id, &source);

        let preview = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec![face_id.to_string()],
            )
            .unwrap();
        let fences = vec![store
            .correction_fence_for(
                face_id,
                vec![source.person_id.clone(), target.person_id.clone()],
            )
            .unwrap()];
        assert!(store
            .split_to_person(&preview, Vec::new())
            .unwrap_err()
            .contains("exactly one fence per FaceId"));
        let mut duplicate_preview = preview.clone();
        duplicate_preview.face_ids.push(face_id.to_string());
        assert!(store
            .split_to_person(
                &duplicate_preview,
                vec![fences[0].clone(), fences[0].clone()]
            )
            .unwrap_err()
            .contains("strictly sorted and unique"));
        let over_limit = (0..=BATCH_CORRECTION_FACE_LIMIT)
            .map(|index| format!("split-over-limit-{index:04}"))
            .collect::<Vec<_>>();
        assert!(store
            .preview_split_to_person(&source.person_id, &target.person_id, over_limit)
            .unwrap_err()
            .contains(&format!("1 and {BATCH_CORRECTION_FACE_LIMIT} faces")));
        let assignment_before: Assignment = store
            .require(ASSIGNMENT_TABLE, face_id, "Assignment")
            .unwrap();
        let source_before: Person = store
            .require(PERSON_TABLE, &source.person_id, "Person")
            .unwrap();
        let target_before: Person = store
            .require(PERSON_TABLE, &target.person_id, "Person")
            .unwrap();
        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();

        // Simulate canonical FaceObservation drift after the service obtained
        // its exact fence but before the store acquired the correction lock.
        // This bypasses revision bump helpers deliberately so the per-Face
        // fence, rather than only a global revision, must reject the apply.
        let mut drifted_face: FaceObservation = store
            .require(FACE_TABLE, face_id, "FaceObservation")
            .unwrap();
        drifted_face.face_revision += 1;
        drifted_face.media_key = "media/split-after.jpg".to_string();
        drifted_face.media_fingerprint = "fingerprint-after-preview".to_string();
        store
            .upsert_json(FACE_TABLE, face_id, &drifted_face)
            .unwrap();

        let error = store.split_to_person(&preview, fences).unwrap_err();
        assert!(error.contains("stale correction revision fence"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "Assignment")
                .unwrap(),
            assignment_before
        );
        assert_eq!(
            store
                .require::<Person>(PERSON_TABLE, &source.person_id, "Person")
                .unwrap(),
            source_before
        );
        assert_eq!(
            store
                .require::<Person>(PERSON_TABLE, &target.person_id, "Person")
                .unwrap(),
            target_before
        );
        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        close(&root, store);
    }

    #[test]
    fn split_to_person_action_preflight_matches_exact_materialized_delta() {
        let root = workspace("split-exact-materialized-count");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Split Source", Vec::new()).unwrap();
        let target = store.create_person("Split Target", Vec::new()).unwrap();
        let face_id = "face-split-exact";
        store
            .create_face(manual_face(face_id, "media/split-exact.jpg"))
            .unwrap();
        confirm(&store, face_id, &source);
        let suggestion = suggest_for_review(&store, face_id, &source);
        let preview = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec![face_id.to_string()],
            )
            .unwrap();
        assert_eq!(preview.required_reversible_rows, 2);
        assert_eq!(preview.delta_counts.assignments, 1);
        assert_eq!(preview.delta_counts.suggestions, 1);
        assert_eq!(preview.preview_id, preview_digest(&preview).unwrap());
        let fence = store
            .correction_fence_for(
                face_id,
                vec![source.person_id.clone(), target.person_id.clone()],
            )
            .unwrap();
        let receipt = store.split_to_person(&preview, vec![fence]).unwrap();
        assert_eq!(receipt.changed_rows, preview.required_reversible_rows);
        assert!(store
            .get_one::<Suggestion>(SUGGESTION_TABLE, &suggestion.suggestion_id)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "Assignment")
                .unwrap()
                .person_id,
            target.person_id
        );
        close(&root, store);
    }

    #[test]
    fn split_previews_digest_bind_selected_face_suggestion_changes_for_both_modes() {
        let root = workspace("split-suggestion-preview-digest");
        let store = MatchStore::open(&root).unwrap();
        let source = store
            .create_person("Split Suggestion Source", Vec::new())
            .unwrap();
        let target = store
            .create_person("Split Suggestion Target", Vec::new())
            .unwrap();
        let new_person_face = "face-split-suggestion-new-person";
        let existing_person_face = "face-split-suggestion-existing-person";
        for (face_id, media_key) in [
            (new_person_face, "media/split-suggestion-new-person.jpg"),
            (
                existing_person_face,
                "media/split-suggestion-existing-person.jpg",
            ),
        ] {
            store.create_face(manual_face(face_id, media_key)).unwrap();
            confirm(&store, face_id, &source);
        }
        let split_person_preview = store
            .preview_split_person(
                &source.person_id,
                vec![new_person_face.to_string()],
                "Suggestion Digest Destination",
            )
            .unwrap();
        let destination_id = match &split_person_preview.kind {
            PersonEditKind::Split {
                destination_person_id,
                ..
            } => destination_person_id.clone(),
            _ => unreachable!(),
        };
        let split_to_person_preview = store
            .preview_split_to_person(
                &source.person_id,
                &target.person_id,
                vec![existing_person_face.to_string()],
            )
            .unwrap();
        let new_person_suggestion = suggest_for_review(&store, new_person_face, &source);
        let existing_person_suggestion = suggest_for_review(&store, existing_person_face, &source);
        let fence = store
            .correction_fence_for(
                existing_person_face,
                vec![source.person_id.clone(), target.person_id.clone()],
            )
            .unwrap();
        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();

        let split_person_error = store.split_person(&split_person_preview).unwrap_err();
        assert!(split_person_error.contains("stale split preview"));
        let split_to_person_error = store
            .split_to_person(&split_to_person_preview, vec![fence])
            .unwrap_err();
        assert!(split_to_person_error.contains("stale split-to-Person preview"));
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &destination_id)
            .unwrap()
            .is_none());
        for face_id in [new_person_face, existing_person_face] {
            assert_eq!(
                store
                    .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "unchanged Assignment")
                    .unwrap()
                    .person_id,
                source.person_id
            );
        }
        assert_eq!(
            store
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &new_person_suggestion.suggestion_id,
                    "new-Person split Suggestion",
                )
                .unwrap(),
            new_person_suggestion
        );
        assert_eq!(
            store
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &existing_person_suggestion.suggestion_id,
                    "existing-Person split Suggestion",
                )
                .unwrap(),
            existing_person_suggestion
        );
        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        close(&root, store);
    }

    #[test]
    fn committed_catalog_correction_survives_cache_refresh_failure_and_recovers_reads() {
        let root = workspace("post-commit-autocomplete-refresh");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        let preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();

        // Simulate the strongest local refresh failure: a prior cache writer
        // panicked. The durable Person transaction must still return success,
        // and the correction path must invalidate/recover the projection.
        let caches = Arc::clone(&store.caches);
        assert!(std::thread::spawn(move || {
            let _guard = caches.write().unwrap();
            panic!("poison autocomplete projection for regression coverage");
        })
        .join()
        .is_err());

        let receipt = store.merge_people(&preview).unwrap();
        assert_eq!(receipt.kind, "merge_people");
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &source.person_id)
            .unwrap()
            .is_none());
        let matches = store
            .correction_autocomplete("target", receipt.catalog_revision, 10)
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].person_id, target.person_id);
        close(&root, store);
    }

    #[test]
    fn bounded_person_assignment_inventory_forces_candidate_leading_index_on_fresh_schema() {
        let root = workspace("assignment-inventory-index-plan");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Assignment Inventory Plan", Vec::new())
            .unwrap();
        store
            .create_face(manual_face(
                "face-assignment-inventory-plan",
                "media/assignment-inventory-plan.jpg",
            ))
            .unwrap();
        confirm(&store, "face-assignment-inventory-plan", &person);

        let db = store.store.db();
        let person_id = person.person_id.clone();
        let plan: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query("SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT 512 EXPLAIN FULL;")
                .bind(("person_id", person_id))
                .bind(("after_assignment_id", String::new()))
                .await
                .map_err(|error| format!("explain bounded Person assignment inventory: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode bounded Person assignment inventory plan: {error}"))
        })
        .unwrap();
        assert!(serde_json::to_string(&plan)
            .unwrap()
            .contains("match_assignment_person_inventory"));
        let (assignments, count, _) = store
            .bounded_person_assignment_inventory_unlocked(&person.person_id)
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].person_id, person.person_id);
        close(&root, store);
    }

    #[test]
    fn person_affected_counts_bound_sparse_pages_and_partition_boundary_duplicates() {
        let root = workspace("person-count-sparse-partitions");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Count Source", Vec::new()).unwrap();
        let target = store.create_person("Count Target", Vec::new()).unwrap();
        let empty = store.create_person("Count Empty", Vec::new()).unwrap();
        let mut faces = (0..600)
            .map(|index| {
                manual_face(
                    &format!("a-unrelated-{index:06}"),
                    &format!("unrelated/{index:06}.jpg"),
                )
            })
            .collect::<Vec<_>>();
        let timestamp = now();
        let mut assignments = Vec::new();
        for index in 0..4101 {
            let media_key = match index {
                4097 | 4100 => "media/002048.jpg".to_string(),
                4098 => "media/002049.jpg".to_string(),
                4099 => "media/é-共享.jpg".to_string(),
                _ => format!("media/{index:06}.jpg"),
            };
            let face_id = format!("z-affected-{index:06}");
            let mut face = manual_face(&face_id, &media_key);
            face.source_index = index as u32;
            faces.push(face);
            let person = if index == 4100 { &target } else { &source };
            assignments.push(Assignment {
                assignment_id: format!("count-assignment-{index:06}"),
                face_id,
                person_id: person.person_id.clone(),
                media_key,
                look_id: None,
                placement: "unsorted".into(),
                state: AssignmentState::CommittedStrictAutomatic.as_str().into(),
                provenance: "bounded-count-regression".into(),
                locked: false,
                model_generation: Some(UNCONFIGURED_MODEL_GENERATION.into()),
                calibration_generation: Some("test-calibration".into()),
                envelope_hash: Some("test-envelope".into()),
                face_revision: 1,
                person_revision: person.revision,
                operation_id: format!("count-seed-{index:06}"),
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            });
        }
        let db = store.database();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; FOR $row IN $faces { UPSERT type::record('match_face_observation', $row.face_id) CONTENT $row; }; FOR $row IN $assignments { UPSERT type::record('match_assignment', $row.assignment_id) CONTENT $row; }; COMMIT TRANSACTION;")
                .bind(("faces", faces)).bind(("assignments", assignments)).await?.check()?;
            Ok(())
        }).unwrap();
        let db = store.database();
        let plan: Vec<Value> = surreal_store::run(async move {
            let mut response = db.query("SELECT face_id, media_key FROM match_face_observation WITH INDEX match_face_id WHERE face_id > $after ORDER BY face_id ASC LIMIT 512 EXPLAIN FULL;")
                .bind(("after", "a-unrelated-000000")).await?.check()?;
            response.take(0)
        }).unwrap();
        let scan = &plan[0]["children"][0];
        assert_eq!(scan["operator"], "IndexScan", "{plan:?}");
        assert_eq!(scan["attributes"]["index"], "match_face_id", "{plan:?}");
        assert_eq!(scan["attributes"]["limit"], "512", "{plan:?}");
        assert_eq!(scan["metrics"]["output_rows"], 512, "{plan:?}");
        let encoded_plan = serde_json::to_string(&plan).unwrap();
        assert!(!encoded_plan.contains("TableScan"), "{encoded_plan}");
        assert!(!encoded_plan.contains("Sort"), "{encoded_plan}");
        let started = std::time::Instant::now();
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&source.person_id)
                .unwrap(),
            (4100, 4098)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        let started = std::time::Instant::now();
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (4101, 4098)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&empty.person_id)
                .unwrap(),
            (0, 0)
        );
        close(&root, store);
    }

    #[test]
    fn whole_person_assignment_preview_streams_pages_and_surfaces_delta_bound() {
        let root = workspace("streamed-person-preview");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Streamed", Vec::new()).unwrap();
        // Seed every referenced Face so exact affected-entity counts exercise
        // the paged assignment inventory without relying on orphaned rows.
        let faces = (0..(CORRECTION_ROW_LIMIT + 1))
            .map(|index| {
                manual_face(
                    &format!("face-{index:06}"),
                    &format!("media/{index:06}.jpg"),
                )
            })
            .collect::<Vec<_>>();
        let timestamp = now();
        let assignments = (0..(CORRECTION_ROW_LIMIT + 1))
            .map(|index| Assignment {
                assignment_id: format!("assignment-{index:06}"),
                face_id: format!("face-{index:06}"),
                person_id: source.person_id.clone(),
                media_key: format!("media/{index:06}.jpg"),
                look_id: None,
                placement: "unsorted".to_string(),
                state: AssignmentState::CommittedStrictAutomatic
                    .as_str()
                    .to_string(),
                provenance: "wp084-streaming-regression".to_string(),
                locked: false,
                model_generation: Some(UNCONFIGURED_MODEL_GENERATION.to_string()),
                calibration_generation: Some("test-calibration".to_string()),
                envelope_hash: Some("test-envelope".to_string()),
                face_revision: 1,
                person_revision: source.revision,
                operation_id: format!("seed-{index:06}"),
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query(
                "BEGIN TRANSACTION;
                 FOR $row IN $faces { UPSERT type::record('match_face_observation', $row.face_id) CONTENT $row; };
                 FOR $row IN $assignments { UPSERT type::record('match_assignment', $row.assignment_id) CONTENT $row; };
                 COMMIT TRANSACTION;",
            )
            .bind(("faces", faces))
            .bind(("assignments", assignments))
            .await
            .map_err(|error| format!("bulk seed streamed Person preview: {error}"))?
            .check()
            .map_err(|error| format!("bulk seed streamed Person preview: {error}"))?;
            Ok::<(), String>(())
        })
        .unwrap();

        let preview = store.preview_remove_person(&source.person_id).unwrap();
        assert_eq!(preview.assignment_count, CORRECTION_ROW_LIMIT + 1);
        assert!(!preview.inventory_complete);
        assert!(preview.face_ids.is_empty());
        assert!(preview.media_keys.is_empty());
        assert_eq!(preview.affected_counts.faces, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(preview.affected_counts.media, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(preview.correction_delta_row_limit, CORRECTION_ROW_LIMIT);
        assert_eq!(preview.required_reversible_rows, CORRECTION_ROW_LIMIT + 2);
        assert_eq!(preview.preview_id, preview_digest(&preview).unwrap());

        let over_limit = (0..=CORRECTION_ROW_LIMIT)
            .map(|index| CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: format!("delta-{index}"),
                before: None,
                after: None,
            })
            .collect::<Vec<_>>();
        let error = validate_delta_bound(&over_limit).unwrap_err();
        assert!(error.contains(&format!("requires {} reversible rows", over_limit.len())));
        assert!(error.contains(&format!(
            "global {CORRECTION_ROW_LIMIT}-row correction-delta limit"
        )));
        close(&root, store);
    }

    #[test]
    fn over_cap_non_assignment_inventory_returns_exact_bounded_preflights() {
        let root = workspace("over-cap-look-preflight");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Look Heavy", Vec::new()).unwrap();
        let target = store.create_person("Merge Target", Vec::new()).unwrap();
        let timestamp = now();
        let looks = (0..(CORRECTION_ROW_LIMIT + 1))
            .map(|index| Look {
                look_id: format!("look-over-cap-{index:06}"),
                person_id: source.person_id.clone(),
                name: format!("Look {index}"),
                revision: 1,
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let values = looks
            .iter()
            .map(|look| serde_json::to_value(look).unwrap())
            .collect::<Vec<_>>();
        let upserts = looks
            .iter()
            .zip(values)
            .map(|(look, value)| (LOOK_TABLE, look.look_id.as_str(), value))
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&upserts, &[]).unwrap();

        let remove = store.preview_remove_person(&source.person_id).unwrap();
        assert_eq!(remove.delta_counts.looks, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(remove.required_reversible_rows, CORRECTION_ROW_LIMIT + 2);
        assert_eq!(remove.affected_counts.looks, CORRECTION_ROW_LIMIT + 1);
        assert!(!remove.inventory_complete);
        assert!(remove.look_ids.is_empty());

        let merge = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(merge.delta_counts.looks, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(merge.required_reversible_rows, CORRECTION_ROW_LIMIT + 3);
        assert_eq!(merge.affected_counts.looks, CORRECTION_ROW_LIMIT + 1);
        assert!(!merge.inventory_complete);
        assert!(merge.look_ids.is_empty());
        close(&root, store);
    }

    #[test]
    fn over_cap_suggestion_inventory_is_exact_bounded_and_rejected_by_preflight() {
        let root = workspace("over-cap-suggestion-preflight");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Suggestion Heavy", Vec::new()).unwrap();
        let target = store
            .create_person("Suggestion Merge Target", Vec::new())
            .unwrap();
        let timestamp = now();
        let mut faces = Vec::with_capacity(CORRECTION_ROW_LIMIT);
        let mut suggestions = Vec::with_capacity(CORRECTION_ROW_LIMIT);
        // A Person row plus exactly 4096 suggestions is already one row over
        // the global reversible-delta cap. Bind the typed arrays once and let
        // SurrealDB iterate them inside one transaction; generating one UPSERT
        // statement per row made this boundary fixture hour-scale.
        for index in 0..CORRECTION_ROW_LIMIT {
            let face_id = format!("face-suggestion-over-cap-{index:06}");
            let mut face = manual_face(&face_id, "media/shared-suggestion-over-cap.jpg");
            face.source_index = index as u32;
            let suggestion = Suggestion {
                suggestion_id: suggestion_id(&face_id, &source.person_id),
                face_id: face_id.clone(),
                candidate_person_id: source.person_id.clone(),
                similarity: 0.91,
                model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
                calibration_generation: None,
                envelope_hash: None,
                media_fingerprint: face.media_fingerprint.clone(),
                face_revision: face.face_revision,
                person_revision: source.revision,
                job_id: format!("seed-suggestion-over-cap-{index:06}"),
                created_at: timestamp.clone(),
            };
            faces.push(face);
            suggestions.push(suggestion);
        }
        let db = store.store.db();
        surreal_store::run(async move {
            db.query(
                "BEGIN TRANSACTION;
                 FOR $row IN $faces { UPSERT type::record('match_face_observation', $row.face_id) CONTENT $row; };
                 FOR $row IN $suggestions { UPSERT type::record('match_suggestion', $row.suggestion_id) CONTENT $row; };
                 COMMIT TRANSACTION;",
            )
            .bind(("faces", faces))
            .bind(("suggestions", suggestions))
            .await
            .map_err(|error| format!("bulk seed over-cap suggestion inventory: {error}"))?
            .check()
            .map_err(|error| format!("bulk seed over-cap suggestion inventory: {error}"))?;
            Ok::<(), String>(())
        })
        .unwrap();

        let remove = store.preview_remove_person(&source.person_id).unwrap();
        assert_eq!(remove.delta_counts.suggestions, CORRECTION_ROW_LIMIT);
        assert_eq!(remove.required_reversible_rows, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(remove.affected_counts.faces, CORRECTION_ROW_LIMIT);
        assert_eq!(remove.affected_counts.media, 1);
        assert!(!remove.inventory_complete);
        assert!(remove.face_ids.is_empty());
        assert!(remove.media_keys.is_empty());
        assert_eq!(remove.preview_id, preview_digest(&remove).unwrap());

        let merge = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(merge.delta_counts.suggestions, CORRECTION_ROW_LIMIT);
        assert_eq!(merge.required_reversible_rows, CORRECTION_ROW_LIMIT + 2);
        assert_eq!(merge.affected_counts.faces, CORRECTION_ROW_LIMIT);
        assert_eq!(merge.affected_counts.media, 1);
        assert!(!merge.inventory_complete);
        assert!(merge.face_ids.is_empty());
        assert!(merge.media_keys.is_empty());

        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();
        let error = store.remove_person(&remove).unwrap_err();
        assert!(error.contains("global 4096-row correction-delta limit"));
        let error = store.merge_people(&merge).unwrap_err();
        assert!(error.contains("global 4096-row correction-delta limit"));
        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        assert_eq!(
            store.count(SUGGESTION_TABLE).unwrap(),
            CORRECTION_ROW_LIMIT as u64
        );
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &source.person_id)
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn person_edit_previews_bind_exact_action_specific_delta_counts() {
        let root = workspace("person-edit-exact-delta-counts");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Count Source", Vec::new()).unwrap();
        let target = store.create_person("Count Target", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-count-source", "media/count-source.jpg"))
            .unwrap();
        confirm(&store, "face-count-source", &source);
        store
            .create_face(manual_face(
                "face-count-suggestion",
                "media/count-suggestion.jpg",
            ))
            .unwrap();
        let source_suggestion = suggest_for_review(&store, "face-count-suggestion", &source);
        store
            .create_face(manual_face(
                "face-count-constraint",
                "media/count-constraint.jpg",
            ))
            .unwrap();
        let look = store.create_look(&source.person_id, "Count Look").unwrap();
        store
            .move_to_look_correction(
                "face-count-source",
                &look.look_id,
                &correction_for_target(&store, "face-count-source", &source),
            )
            .unwrap();
        let trusted_face: FaceObservation = store
            .require(FACE_TABLE, "face-count-source", "FaceObservation")
            .unwrap();
        let model_generation = "model-count-source";
        store
            .register_model_generation(model_generation, true)
            .unwrap();
        let timestamp = now();
        let set = TrustedTemplateSet {
            set_id: "set-count-source".to_string(),
            look_id: look.look_id.clone(),
            name: "Count Set".to_string(),
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let membership = TrustedTemplateMembership {
            membership_id: "membership-count-source".to_string(),
            set_id: set.set_id.clone(),
            look_id: look.look_id.clone(),
            face_id: "face-count-source".to_string(),
            authorized: true,
            alignment_valid: true,
            quality_passed: true,
            pose_passed: true,
            diversity_passed: true,
            provenance: "operator_trusted_reference".to_string(),
            model_generation: model_generation.to_string(),
            embedding_id: "embedding-count-source".to_string(),
            media_fingerprint: trusted_face.media_fingerprint.clone(),
            face_revision: trusted_face.face_revision,
            quality_score: 0.99,
            quality_threshold: 0.8,
            pose_bucket: trusted_face.pose_bucket.clone(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
            operation_id: "seed-count-source".to_string(),
            created_at: timestamp.clone(),
        };
        let embedding = FaceEmbedding {
            embedding_id: membership.embedding_id.clone(),
            face_id: trusted_face.face_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: model_generation.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: trusted_face.media_fingerprint.clone(),
            face_revision: trusted_face.face_revision,
            job_id: "embedding-job-count-source".to_string(),
            active: true,
            created_at: timestamp.clone(),
        };
        let search = TrustedSearchEmbedding {
            membership_id: membership.membership_id.clone(),
            person_id: source.person_id.clone(),
            look_id: look.look_id.clone(),
            face_id: membership.face_id.clone(),
            embedding_id: membership.embedding_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: membership.model_generation.clone(),
            created_at: timestamp.clone(),
        };
        let source_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id("face-count-constraint", &source.person_id),
            face_id: "face-count-constraint".to_string(),
            person_id: source.person_id.clone(),
            operation_id: "seed-count-source".to_string(),
            operator_owned: true,
            created_at: timestamp.clone(),
        };
        store
            .transactional_upserts_deletes(
                &[
                    (
                        EMBEDDING_TABLE,
                        embedding.embedding_id.as_str(),
                        serde_json::to_value(&embedding).unwrap(),
                    ),
                    (
                        TEMPLATE_SET_TABLE,
                        set.set_id.as_str(),
                        serde_json::to_value(&set).unwrap(),
                    ),
                    (
                        TRUSTED_MEMBER_TABLE,
                        membership.membership_id.as_str(),
                        serde_json::to_value(&membership).unwrap(),
                    ),
                    (
                        TRUSTED_SEARCH_TABLE,
                        search.membership_id.as_str(),
                        serde_json::to_value(&search).unwrap(),
                    ),
                    (
                        CONSTRAINT_TABLE,
                        source_constraint.constraint_id.as_str(),
                        serde_json::to_value(&source_constraint).unwrap(),
                    ),
                ],
                &[],
            )
            .unwrap();

        let remove = store.preview_remove_person(&source.person_id).unwrap();
        assert_eq!(
            remove.delta_counts,
            PersonEditDeltaCounts {
                persons: 1,
                assignments: 1,
                looks: 1,
                template_sets: 1,
                trusted_members: 1,
                trusted_search: 1,
                constraints: 1,
                suggestions: 1,
            }
        );
        assert_eq!(remove.required_reversible_rows, 8);
        assert!(remove.inventory_complete);
        assert_eq!(remove.affected_counts.faces, 3);
        assert_eq!(remove.affected_counts.media, 3);
        assert_eq!(remove.affected_counts.looks, 1);
        assert_eq!(
            remove.face_ids,
            vec![
                "face-count-constraint".to_string(),
                "face-count-source".to_string(),
                "face-count-suggestion".to_string(),
            ]
        );
        assert_eq!(
            remove.media_keys,
            vec![
                "media/count-constraint.jpg".to_string(),
                "media/count-source.jpg".to_string(),
                "media/count-suggestion.jpg".to_string(),
            ]
        );

        let merge_missing_target = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(
            merge_missing_target.delta_counts,
            PersonEditDeltaCounts {
                persons: 2,
                assignments: 1,
                looks: 1,
                template_sets: 0,
                trusted_members: 0,
                trusted_search: 1,
                // Delete the source constraint and create its missing target
                // counterpart.
                constraints: 2,
                suggestions: 1,
            }
        );
        assert_eq!(merge_missing_target.required_reversible_rows, 8);
        assert!(merge_missing_target.inventory_complete);
        assert_eq!(merge_missing_target.face_ids, remove.face_ids);
        assert_eq!(merge_missing_target.media_keys, remove.media_keys);

        let target_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id("face-count-constraint", &target.person_id),
            face_id: "face-count-constraint".to_string(),
            person_id: target.person_id.clone(),
            operation_id: "seed-count-target".to_string(),
            operator_owned: true,
            created_at: timestamp,
        };
        store
            .upsert_json(
                CONSTRAINT_TABLE,
                &target_constraint.constraint_id,
                &target_constraint,
            )
            .unwrap();
        let merge_existing_target = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(merge_existing_target.delta_counts.constraints, 1);
        assert_eq!(merge_existing_target.delta_counts.suggestions, 1);
        assert_eq!(merge_existing_target.required_reversible_rows, 7);
        assert_ne!(
            merge_missing_target.preview_id, merge_existing_target.preview_id,
            "target-constraint topology must be digest-bound"
        );
        let stale = store.merge_people(&merge_missing_target).unwrap_err();
        assert!(stale.contains("stale merge preview"));
        let merged = store.merge_people(&merge_existing_target).unwrap();
        assert_eq!(
            merged.changed_rows,
            merge_existing_target.required_reversible_rows
        );
        assert!(store
            .get_one::<Suggestion>(SUGGESTION_TABLE, &source_suggestion.suggestion_id)
            .unwrap()
            .is_none());
        let moved_search = store
            .require::<TrustedSearchEmbedding>(
                TRUSTED_SEARCH_TABLE,
                &membership.membership_id,
                "TrustedSearchEmbedding",
            )
            .unwrap();
        assert_eq!(moved_search.person_id, target.person_id);
        store.undo_correction(&merged.operation_id).unwrap();
        assert!(store
            .get_one::<Suggestion>(SUGGESTION_TABLE, &source_suggestion.suggestion_id)
            .unwrap()
            .is_some());
        let restored_search = store
            .require::<TrustedSearchEmbedding>(
                TRUSTED_SEARCH_TABLE,
                &membership.membership_id,
                "TrustedSearchEmbedding",
            )
            .unwrap();
        assert_eq!(restored_search.person_id, source.person_id);
        // Exercise the over-cap count query on this small, overlapping graph
        // too: the ordinary preview above can use its bounded retained rows.
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&source.person_id)
                .unwrap(),
            (3, 3)
        );
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (3, 3)
        );
        let assigned: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-count-source", "Assignment")
            .unwrap();
        store
            .transactional_upserts_deletes_unlocked(
                &[],
                &[(ASSIGNMENT_TABLE, assigned.assignment_id.as_str())],
            )
            .unwrap();
        // Search plus membership share the same Face: count once.
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&source.person_id)
                .unwrap(),
            (3, 3)
        );
        store
            .transactional_upserts_deletes_unlocked(
                &[],
                &[(TRUSTED_SEARCH_TABLE, membership.membership_id.as_str())],
            )
            .unwrap();
        // Only the member -> set -> Look ownership chain reaches this Face.
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&source.person_id)
                .unwrap(),
            (3, 3)
        );
        let target_look = store
            .create_look(&target.person_id, "Unrelated owner")
            .unwrap();
        let mut unrelated_set = set.clone();
        unrelated_set.look_id = target_look.look_id.clone();
        store
            .upsert_json(TEMPLATE_SET_TABLE, &unrelated_set.set_id, &unrelated_set)
            .unwrap();
        assert_eq!(
            store
                .person_edit_affected_face_media_counts_unlocked(&source.person_id)
                .unwrap(),
            (2, 2)
        );
        // Merge must not include target-only trusted membership.
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (2, 2)
        );
        store
            .transactional_upserts_deletes_unlocked(
                &[],
                &[(CONSTRAINT_TABLE, source_constraint.constraint_id.as_str())],
            )
            .unwrap();
        // The remaining target-only constraint is outside both source scopes.
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (1, 1)
        );
        let mut target_assignment = assigned;
        target_assignment.person_id = target.person_id.clone();
        store
            .upsert_json(
                ASSIGNMENT_TABLE,
                &target_assignment.assignment_id,
                &target_assignment,
            )
            .unwrap();
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (2, 2)
        );
        let mut target_suggestion = source_suggestion.clone();
        target_suggestion.face_id = "face-count-constraint".to_string();
        target_suggestion.candidate_person_id = target.person_id.clone();
        target_suggestion.suggestion_id =
            suggestion_id(&target_suggestion.face_id, &target.person_id);
        store
            .upsert_json(
                SUGGESTION_TABLE,
                &target_suggestion.suggestion_id,
                &target_suggestion,
            )
            .unwrap();
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (3, 3)
        );
        // Dangling candidate references never manufacture affected Faces/media.
        store
            .transactional_upserts_deletes_unlocked(&[], &[(FACE_TABLE, "face-count-constraint")])
            .unwrap();
        assert_eq!(
            store
                .merge_person_edit_affected_face_media_counts_unlocked(
                    &source.person_id,
                    &target.person_id
                )
                .unwrap(),
            (2, 2)
        );
        close(&root, store);
    }

    #[test]
    fn merge_apply_restart_undo_restores_complete_typed_delta_and_preserves_later_rows() {
        let root = workspace("merge-restart-complete-delta");
        let store = MatchStore::open(&root).unwrap();
        let source = store
            .create_person("Restart Merge Source", Vec::new())
            .unwrap();
        let target = store
            .create_person("Restart Merge Target", Vec::new())
            .unwrap();
        let source_look = store
            .create_look(&source.person_id, "Restart Merge Look")
            .unwrap();
        let face_id = "face-restart-merge-owned";
        store
            .create_face(manual_face(face_id, "media/restart-merge-owned.jpg"))
            .unwrap();
        confirm(&store, face_id, &source);
        store
            .move_to_look_correction(
                face_id,
                &source_look.look_id,
                &correction_for_target(&store, face_id, &source),
            )
            .unwrap();
        let suggestion = suggest_for_review(&store, face_id, &source);
        let trusted = seed_canonical_trusted_reference(
            &store,
            &source,
            &source_look,
            face_id,
            "restart-merge",
        );
        store
            .create_face(manual_face(
                "constraint-face-restart-merge",
                "media/constraint-face-restart-merge.jpg",
            ))
            .unwrap();
        let source_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id("constraint-face-restart-merge", &source.person_id),
            face_id: "constraint-face-restart-merge".to_string(),
            person_id: source.person_id.clone(),
            operation_id: "constraint-seed-restart-merge".to_string(),
            operator_owned: true,
            created_at: now(),
        };
        store
            .upsert_json(
                CONSTRAINT_TABLE,
                &source_constraint.constraint_id,
                &source_constraint,
            )
            .unwrap();

        let source_before = store
            .require::<Person>(PERSON_TABLE, &source.person_id, "merge source")
            .unwrap();
        let target_before = store
            .require::<Person>(PERSON_TABLE, &target.person_id, "merge target")
            .unwrap();
        let look_before = store
            .require::<Look>(LOOK_TABLE, &source_look.look_id, "merge Look")
            .unwrap();
        let assignment_before = store
            .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "merge Assignment")
            .unwrap();
        let preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        let receipt = store.merge_people(&preview).unwrap();
        let operation = store
            .require::<MatchOperation>(OPERATION_TABLE, &receipt.operation_id, "merge operation")
            .unwrap();
        let envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        let mut owned_rows = envelope
            .rows
            .iter()
            .map(|row| format!("{}:{}", row.table.name(), row.stable_id))
            .collect::<Vec<_>>();
        owned_rows.sort();
        let target_constraint_id =
            cannot_link_id("constraint-face-restart-merge", &target.person_id);
        let mut expected_owned_rows = vec![
            format!("{ASSIGNMENT_TABLE}:{face_id}"),
            format!("{CONSTRAINT_TABLE}:{}", source_constraint.constraint_id),
            format!("{CONSTRAINT_TABLE}:{target_constraint_id}"),
            format!("{LOOK_TABLE}:{}", source_look.look_id),
            format!("{PERSON_TABLE}:{}", source.person_id),
            format!("{PERSON_TABLE}:{}", target.person_id),
            format!("{SUGGESTION_TABLE}:{}", suggestion.suggestion_id),
            format!("{TRUSTED_SEARCH_TABLE}:{}", trusted.search.membership_id),
        ];
        expected_owned_rows.sort();
        assert_eq!(owned_rows, expected_owned_rows);
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &source.person_id)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .require::<Look>(LOOK_TABLE, &source_look.look_id, "moved merge Look")
                .unwrap()
                .person_id,
            target.person_id
        );
        assert_eq!(
            store
                .require::<TrustedSearchEmbedding>(
                    TRUSTED_SEARCH_TABLE,
                    &trusted.search.membership_id,
                    "moved merge trusted search",
                )
                .unwrap()
                .person_id,
            target.person_id
        );

        let later_person = store
            .create_person("Later Unrelated Merge Person", Vec::new())
            .unwrap();
        let later_look = store
            .create_look(&later_person.person_id, "Later Unrelated Merge Look")
            .unwrap();
        let later_face_id = "face-restart-merge-later";
        store
            .create_face(manual_face(later_face_id, "media/restart-merge-later.jpg"))
            .unwrap();
        confirm(&store, later_face_id, &later_person);
        store
            .move_to_look_correction(
                later_face_id,
                &later_look.look_id,
                &correction_for_target(&store, later_face_id, &later_person),
            )
            .unwrap();
        let later_suggestion_face = "face-restart-merge-later-suggestion";
        store
            .create_face(manual_face(
                later_suggestion_face,
                "media/restart-merge-later-suggestion.jpg",
            ))
            .unwrap();
        let later_suggestion = suggest_for_review(&store, later_suggestion_face, &later_person);
        let later_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(later_face_id, &source.person_id),
            face_id: later_face_id.to_string(),
            person_id: source.person_id.clone(),
            operation_id: "later-unrelated-merge-constraint".to_string(),
            operator_owned: true,
            created_at: now(),
        };
        store
            .upsert_json(
                CONSTRAINT_TABLE,
                &later_constraint.constraint_id,
                &later_constraint,
            )
            .unwrap();
        let later_person_before = store
            .require::<Person>(PERSON_TABLE, &later_person.person_id, "later Person")
            .unwrap();
        let later_look_before = store
            .require::<Look>(LOOK_TABLE, &later_look.look_id, "later Look")
            .unwrap();
        let later_assignment_before = store
            .require::<Assignment>(ASSIGNMENT_TABLE, later_face_id, "later Assignment")
            .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        reopened.undo_correction(&receipt.operation_id).unwrap();
        let restored_source = reopened
            .require::<Person>(PERSON_TABLE, &source.person_id, "restored merge source")
            .unwrap();
        let mut expected_source = source_before.clone();
        expected_source.revision += 1;
        expected_source.catalog_revision = restored_source.catalog_revision;
        expected_source.updated_at = restored_source.updated_at.clone();
        assert_eq!(restored_source, expected_source);
        let restored_target = reopened
            .require::<Person>(PERSON_TABLE, &target.person_id, "restored merge target")
            .unwrap();
        let mut expected_target = target_before.clone();
        expected_target.revision += 2;
        expected_target.catalog_revision = restored_target.catalog_revision;
        expected_target.updated_at = restored_target.updated_at.clone();
        assert_eq!(restored_target, expected_target);
        let restored_look = reopened
            .require::<Look>(LOOK_TABLE, &source_look.look_id, "restored merge Look")
            .unwrap();
        let mut expected_look = look_before.clone();
        expected_look.revision += 1;
        expected_look.updated_at = restored_look.updated_at.clone();
        assert_eq!(restored_look, expected_look);
        let undo_id = format!("undo-{}", receipt.operation_id);
        let restored_assignment = reopened
            .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "restored merge Assignment")
            .unwrap();
        let mut expected_assignment = assignment_before.clone();
        expected_assignment.operation_id = undo_id.clone();
        expected_assignment.person_revision = restored_source.revision;
        expected_assignment.updated_at = restored_assignment.updated_at.clone();
        assert_eq!(restored_assignment, expected_assignment);
        let restored_constraint = reopened
            .require::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &source_constraint.constraint_id,
                "restored merge source constraint",
            )
            .unwrap();
        let mut expected_constraint = source_constraint.clone();
        expected_constraint.operation_id = undo_id;
        assert_eq!(restored_constraint, expected_constraint);
        assert!(reopened
            .get_one::<CannotLinkConstraint>(CONSTRAINT_TABLE, &target_constraint_id)
            .unwrap()
            .is_none());
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &suggestion.suggestion_id,
                    "restored merge Suggestion",
                )
                .unwrap(),
            suggestion
        );
        assert_eq!(
            reopened
                .require::<TrustedSearchEmbedding>(
                    TRUSTED_SEARCH_TABLE,
                    &trusted.search.membership_id,
                    "restored merge trusted search",
                )
                .unwrap(),
            trusted.search
        );
        assert_eq!(
            reopened
                .require::<TrustedTemplateSet>(
                    TEMPLATE_SET_TABLE,
                    &trusted.set.set_id,
                    "unaffected merge template set",
                )
                .unwrap(),
            trusted.set
        );
        assert_eq!(
            reopened
                .require::<TrustedTemplateMembership>(
                    TRUSTED_MEMBER_TABLE,
                    &trusted.membership.membership_id,
                    "unaffected merge membership",
                )
                .unwrap(),
            trusted.membership
        );
        assert_eq!(
            reopened
                .require::<FaceEmbedding>(
                    EMBEDDING_TABLE,
                    &trusted.embedding.embedding_id,
                    "unaffected merge embedding",
                )
                .unwrap(),
            trusted.embedding
        );
        assert_eq!(
            reopened
                .require::<Person>(PERSON_TABLE, &later_person.person_id, "later Person")
                .unwrap(),
            later_person_before
        );
        assert_eq!(
            reopened
                .require::<Look>(LOOK_TABLE, &later_look.look_id, "later Look")
                .unwrap(),
            later_look_before
        );
        assert_eq!(
            reopened
                .require::<Assignment>(ASSIGNMENT_TABLE, later_face_id, "later Assignment")
                .unwrap(),
            later_assignment_before
        );
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &later_suggestion.suggestion_id,
                    "later Suggestion",
                )
                .unwrap(),
            later_suggestion
        );
        assert_eq!(
            reopened
                .require::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &later_constraint.constraint_id,
                    "later constraint",
                )
                .unwrap(),
            later_constraint
        );
        close(&root, reopened);
    }

    #[test]
    fn split_apply_restart_undo_restores_complete_typed_delta_and_preserves_later_rows() {
        let root = workspace("split-restart-complete-delta");
        let store = MatchStore::open(&root).unwrap();
        let source = store
            .create_person("Restart Split Source", Vec::new())
            .unwrap();
        let source_look = store
            .create_look(&source.person_id, "Restart Split Look")
            .unwrap();
        let face_id = "face-restart-split-owned";
        store
            .create_face(manual_face(face_id, "media/restart-split-owned.jpg"))
            .unwrap();
        confirm(&store, face_id, &source);
        store
            .move_to_look_correction(
                face_id,
                &source_look.look_id,
                &correction_for_target(&store, face_id, &source),
            )
            .unwrap();
        let suggestion = suggest_for_review(&store, face_id, &source);
        let trusted = seed_canonical_trusted_reference(
            &store,
            &source,
            &source_look,
            face_id,
            "restart-split",
        );
        let unaffected_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(face_id, "unrelated-split-person"),
            face_id: face_id.to_string(),
            person_id: "unrelated-split-person".to_string(),
            operation_id: "unaffected-split-constraint".to_string(),
            operator_owned: true,
            created_at: now(),
        };
        store
            .upsert_json(
                CONSTRAINT_TABLE,
                &unaffected_constraint.constraint_id,
                &unaffected_constraint,
            )
            .unwrap();
        let source_before = store
            .require::<Person>(PERSON_TABLE, &source.person_id, "split source")
            .unwrap();
        let look_before = store
            .require::<Look>(LOOK_TABLE, &source_look.look_id, "split Look")
            .unwrap();
        let assignment_before = store
            .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "split Assignment")
            .unwrap();
        let preview = store
            .preview_split_person(
                &source.person_id,
                vec![face_id.to_string()],
                "Restart Split Destination",
            )
            .unwrap();
        assert_eq!(preview.delta_counts.suggestions, 1);
        assert_eq!(preview.required_reversible_rows, 5);
        let destination_id = match &preview.kind {
            PersonEditKind::Split {
                destination_person_id,
                ..
            } => destination_person_id.clone(),
            _ => unreachable!(),
        };
        let receipt = store.split_person(&preview).unwrap();
        let operation = store
            .require::<MatchOperation>(OPERATION_TABLE, &receipt.operation_id, "split operation")
            .unwrap();
        let envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        let mut owned_rows = envelope
            .rows
            .iter()
            .map(|row| format!("{}:{}", row.table.name(), row.stable_id))
            .collect::<Vec<_>>();
        owned_rows.sort();
        let mut expected_owned_rows = vec![
            format!("{ASSIGNMENT_TABLE}:{face_id}"),
            format!("{PERSON_TABLE}:{destination_id}"),
            format!("{SUGGESTION_TABLE}:{}", suggestion.suggestion_id),
            format!(
                "{TRUSTED_MEMBER_TABLE}:{}",
                trusted.membership.membership_id
            ),
            format!("{TRUSTED_SEARCH_TABLE}:{}", trusted.search.membership_id),
        ];
        expected_owned_rows.sort();
        assert_eq!(owned_rows, expected_owned_rows);
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "split moved Assignment")
                .unwrap()
                .person_id,
            destination_id
        );
        assert!(store
            .get_one::<Suggestion>(SUGGESTION_TABLE, &suggestion.suggestion_id)
            .unwrap()
            .is_none());
        assert!(store
            .get_one::<TrustedTemplateMembership>(
                TRUSTED_MEMBER_TABLE,
                &trusted.membership.membership_id,
            )
            .unwrap()
            .is_none());
        assert!(store
            .get_one::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE, &trusted.search.membership_id,)
            .unwrap()
            .is_none());

        let later_person = store
            .create_person("Later Unrelated Split Person", Vec::new())
            .unwrap();
        let later_look = store
            .create_look(&later_person.person_id, "Later Unrelated Split Look")
            .unwrap();
        let later_face_id = "face-restart-split-later";
        store
            .create_face(manual_face(later_face_id, "media/restart-split-later.jpg"))
            .unwrap();
        confirm(&store, later_face_id, &later_person);
        store
            .move_to_look_correction(
                later_face_id,
                &later_look.look_id,
                &correction_for_target(&store, later_face_id, &later_person),
            )
            .unwrap();
        let later_suggestion_face = "face-restart-split-later-suggestion";
        store
            .create_face(manual_face(
                later_suggestion_face,
                "media/restart-split-later-suggestion.jpg",
            ))
            .unwrap();
        let later_suggestion = suggest_for_review(&store, later_suggestion_face, &later_person);
        let later_person_before = store
            .require::<Person>(PERSON_TABLE, &later_person.person_id, "later split Person")
            .unwrap();
        let later_look_before = store
            .require::<Look>(LOOK_TABLE, &later_look.look_id, "later split Look")
            .unwrap();
        let later_assignment_before = store
            .require::<Assignment>(ASSIGNMENT_TABLE, later_face_id, "later split Assignment")
            .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        reopened.undo_correction(&receipt.operation_id).unwrap();
        assert!(reopened
            .get_one::<Person>(PERSON_TABLE, &destination_id)
            .unwrap()
            .is_none());
        assert_eq!(
            reopened
                .require::<Person>(PERSON_TABLE, &source.person_id, "unaffected split source")
                .unwrap(),
            source_before
        );
        assert_eq!(
            reopened
                .require::<Look>(LOOK_TABLE, &source_look.look_id, "unaffected split Look")
                .unwrap(),
            look_before
        );
        let restored_assignment = reopened
            .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "restored split Assignment")
            .unwrap();
        let mut expected_assignment = assignment_before.clone();
        expected_assignment.operation_id = format!("undo-{}", receipt.operation_id);
        expected_assignment.updated_at = restored_assignment.updated_at.clone();
        assert_eq!(restored_assignment, expected_assignment);
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &suggestion.suggestion_id,
                    "restored split Suggestion",
                )
                .unwrap(),
            suggestion
        );
        let restored_membership = reopened
            .require::<TrustedTemplateMembership>(
                TRUSTED_MEMBER_TABLE,
                &trusted.membership.membership_id,
                "restored split membership",
            )
            .unwrap();
        let mut expected_membership = trusted.membership.clone();
        expected_membership.operation_id = format!("undo-{}", receipt.operation_id);
        assert_eq!(restored_membership, expected_membership);
        assert_eq!(
            reopened
                .require::<TrustedSearchEmbedding>(
                    TRUSTED_SEARCH_TABLE,
                    &trusted.search.membership_id,
                    "restored split trusted search",
                )
                .unwrap(),
            trusted.search
        );
        assert_eq!(
            reopened
                .require::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &unaffected_constraint.constraint_id,
                    "unaffected split constraint",
                )
                .unwrap(),
            unaffected_constraint
        );
        assert_eq!(
            reopened
                .require::<TrustedTemplateSet>(
                    TEMPLATE_SET_TABLE,
                    &trusted.set.set_id,
                    "unaffected split template set",
                )
                .unwrap(),
            trusted.set
        );
        assert_eq!(
            reopened
                .require::<FaceEmbedding>(
                    EMBEDDING_TABLE,
                    &trusted.embedding.embedding_id,
                    "unaffected split embedding",
                )
                .unwrap(),
            trusted.embedding
        );
        assert_eq!(
            reopened
                .require::<Person>(PERSON_TABLE, &later_person.person_id, "later split Person")
                .unwrap(),
            later_person_before
        );
        assert_eq!(
            reopened
                .require::<Look>(LOOK_TABLE, &later_look.look_id, "later split Look")
                .unwrap(),
            later_look_before
        );
        assert_eq!(
            reopened
                .require::<Assignment>(ASSIGNMENT_TABLE, later_face_id, "later split Assignment",)
                .unwrap(),
            later_assignment_before
        );
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &later_suggestion.suggestion_id,
                    "later split Suggestion",
                )
                .unwrap(),
            later_suggestion
        );
        close(&root, reopened);
    }

    #[test]
    fn person_edit_delta_limit_allows_4096_and_blocks_4097_before_row_materialization() {
        assert!(ensure_within_delta_limit(CORRECTION_ROW_LIMIT).is_ok());
        let error = ensure_within_delta_limit(CORRECTION_ROW_LIMIT + 1).unwrap_err();
        assert!(error.contains("requires 4097 reversible rows"));
        // Both whole-Person mutation paths call this scalar gate before
        // constructing their first CorrectionRowDelta, so this exact boundary
        // stays independent of a 4097-row allocation.
    }

    #[test]
    fn correction_operation_json_preflight_accepts_exact_portable_boundary() {
        let exact = Value::String("x".repeat(CORRECTION_OPERATION_JSON_MAX_BYTES - 2));
        let encoded = serialize_correction_operation_json(&exact, "after_json").unwrap();
        assert_eq!(encoded.len(), CORRECTION_OPERATION_JSON_MAX_BYTES);
        let over = Value::String("x".repeat(CORRECTION_OPERATION_JSON_MAX_BYTES - 1));
        assert!(serialize_correction_operation_json(&over, "after_json")
            .unwrap_err()
            .contains("nested-string limit"));
    }

    #[test]
    fn near_maximum_persisted_correction_round_trips_rebuilds_and_clears() {
        const MEDIA_KEY: &str = "media/portable-boundary.jpg";
        let root = workspace("portable-correction-boundary");
        let store = MatchStore::open(&root).unwrap();
        let media_root = root.join("portable-boundary-media");
        std::fs::create_dir_all(media_root.join("media")).unwrap();
        std::fs::write(media_root.join(MEDIA_KEY), b"portable-boundary-media").unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let person = store
            .create_person("Portable boundary", Vec::new())
            .unwrap();
        let all_fixtures = (0..256)
            .map(|index| portable_assignment_fixture(&person, MEDIA_KEY, index))
            .collect::<Vec<_>>();
        let accepted_count = (1..all_fixtures.len())
            .take_while(|count| {
                remove_person_boundary_json(&person, &all_fixtures[..*count], MEDIA_KEY).is_ok()
            })
            .last()
            .expect("at least one portable correction row must fit");
        assert!(accepted_count + 1 < all_fixtures.len());
        let (expected_before, expected_after) =
            remove_person_boundary_json(&person, &all_fixtures[..accepted_count], MEDIA_KEY)
                .unwrap();
        assert!(
            expected_before.len().max(expected_after.len())
                > CORRECTION_OPERATION_JSON_MAX_BYTES - 2048,
            "accepted correction must exercise the real nested-string ceiling"
        );
        assert!(remove_person_boundary_json(
            &person,
            &all_fixtures[..accepted_count + 1],
            MEDIA_KEY,
        )
        .unwrap_err()
        .contains("nested-string limit"));

        seed_portable_assignments(&store, &all_fixtures[..accepted_count]);
        let preview = store.preview_remove_person(&person.person_id).unwrap();
        let receipt = store.remove_person(&preview).unwrap();
        let persisted: MatchOperation = store
            .require(
                OPERATION_TABLE,
                &receipt.operation_id,
                "boundary correction",
            )
            .unwrap();
        assert!(persisted.before_json.len() <= CORRECTION_OPERATION_JSON_MAX_BYTES);
        assert!(persisted.after_json.len() <= CORRECTION_OPERATION_JSON_MAX_BYTES);
        assert!(
            persisted.before_json.len().max(persisted.after_json.len())
                > CORRECTION_OPERATION_JSON_MAX_BYTES - 2048
        );

        let export_path = root.join("portable-boundary-bundle.json");
        store.export_identity_bundle(&export_path).unwrap();
        let imported_root = workspace("portable-correction-boundary-import");
        let imported = MatchStore::open(&imported_root).unwrap();
        let relocations = BTreeMap::from([(
            configured.root_id,
            std::fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let import = imported
            .preview_identity_bundle_import(&export_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        imported.apply_identity_bundle_import(import).unwrap();
        imported.preview_rebuild_match_analysis().unwrap();
        imported
            .preview_clear_all_match_data(&imported_root.join("boundary-recovery.json"))
            .unwrap();

        let over_root = workspace("portable-correction-boundary-over");
        let over = MatchStore::open(&over_root).unwrap();
        let over_person = over.create_person("Portable boundary", Vec::new()).unwrap();
        let over_fixtures = (0..=accepted_count)
            .map(|index| portable_assignment_fixture(&over_person, MEDIA_KEY, index))
            .collect::<Vec<_>>();
        seed_portable_assignments(&over, &over_fixtures);
        let over_preview = over.preview_remove_person(&over_person.person_id).unwrap();
        let operation_count_before = over.count(OPERATION_TABLE).unwrap();
        let error = over.remove_person(&over_preview).unwrap_err();
        assert!(error.contains("nested-string limit"));
        assert_eq!(over.count(OPERATION_TABLE).unwrap(), operation_count_before);
        assert!(over
            .get_one::<Person>(PERSON_TABLE, &over_person.person_id)
            .unwrap()
            .is_some());
        assert_eq!(
            over.count(ASSIGNMENT_TABLE).unwrap(),
            (accepted_count + 1) as u64
        );

        close(&over_root, over);
        close(&imported_root, imported);
        close(&root, store);
    }

    #[test]
    fn manual_media_authority_rejects_fabricated_and_stale_pairs() {
        let root = workspace("manual-media-authority");
        let media_root = root.join("indexed-media");
        std::fs::create_dir_all(&media_root).unwrap();
        let source = media_root.join("portrait.jpg");
        std::fs::write(&source, b"canonical-source").unwrap();
        let alternate_source = media_root.join("portrait-alternate.jpg");
        std::fs::write(&alternate_source, b"alternate-source").unwrap();
        let canonical_fingerprint = format!("{:x}", Sha256::digest(b"canonical-source"));
        let alternate_fingerprint = format!("{:x}", Sha256::digest(b"alternate-source"));
        let stale_fingerprint = format!("{:x}", Sha256::digest(b"stale-source"));
        let store = MatchStore::open(&root).unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let execution = store.execution_state().unwrap();
        let timestamp = now();
        let job = IndexJob {
            job_id: "job-manual-authority-a".to_string(),
            root_key: configured.root_id,
            lifecycle: JobLifecycle::Running.as_str().to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            discovered: 1,
            completed: 0,
            failed: 0,
            skipped: 0,
            failure_code: None,
            failure_message: None,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let asset = JobAsset {
            asset_id: "asset-manual-authority-a".to_string(),
            job_id: job.job_id.clone(),
            media_key: "portrait.jpg".to_string(),
            source_path: Some(source.to_string_lossy().to_string()),
            media_fingerprint: canonical_fingerprint.clone(),
            next_stage: JobStage::Detect.as_str().to_string(),
            completed_stages: vec![JobStage::Discover.as_str().to_string()],
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: timestamp.clone(),
        };
        let alternate_job = IndexJob {
            job_id: "job-manual-authority-z".to_string(),
            ..job.clone()
        };
        let alternate_asset = JobAsset {
            asset_id: "asset-manual-authority-z".to_string(),
            job_id: alternate_job.job_id.clone(),
            source_path: Some(alternate_source.to_string_lossy().to_string()),
            media_fingerprint: alternate_fingerprint,
            ..asset.clone()
        };
        {
            let _guard = store.store.transaction_lock().write().unwrap();
            store
                .transactional_upserts_deletes_unlocked(
                    &[
                        (JOB_TABLE, &job.job_id, serde_json::to_value(&job).unwrap()),
                        (
                            JOB_TABLE,
                            &alternate_job.job_id,
                            serde_json::to_value(&alternate_job).unwrap(),
                        ),
                        (
                            JOB_ASSET_TABLE,
                            &asset.asset_id,
                            serde_json::to_value(&asset).unwrap(),
                        ),
                        (
                            JOB_ASSET_TABLE,
                            &alternate_asset.asset_id,
                            serde_json::to_value(&alternate_asset).unwrap(),
                        ),
                    ],
                    &[],
                )
                .unwrap();
        }
        let authority = store
            .manual_media_authority("portrait.jpg", &canonical_fingerprint)
            .unwrap();
        assert_eq!(authority.source_path, source);
        assert_eq!(
            store
                .media_faces("portrait.jpg")
                .unwrap()
                .media_fingerprint
                .as_deref(),
            Some(canonical_fingerprint.as_str()),
            "Viewer fallback must use the same equal-timestamp asset_id ASC authority"
        );
        let person = store.create_person("Authority", Vec::new()).unwrap();
        let mut face = manual_face("authority-face", "portrait.jpg");
        face.media_fingerprint.clone_from(&canonical_fingerprint);
        store.create_face(face).unwrap();
        confirm(&store, "authority-face", &person);
        let current_person = store
            .require::<Person>(PERSON_TABLE, &person.person_id, "Person")
            .unwrap();
        store
            .update_person_preferences(
                &person.person_id,
                current_person.revision,
                Some("portrait.jpg".to_string()),
                false,
                false,
            )
            .unwrap();
        assert_eq!(
            store
                .person_gallery(&person.person_id, 0, 10)
                .unwrap()
                .media_paths,
            vec![source.to_string_lossy().to_string()]
        );
        let catalog = store.catalog_snapshot(0, 10, true).unwrap();
        assert_eq!(
            catalog.rows[0].cover_source_path.as_deref(),
            Some(source.to_string_lossy().as_ref())
        );
        assert!(store
            .manual_media_authority("portrait.jpg", &stale_fingerprint)
            .unwrap_err()
            .contains("stale"));
        assert!(store
            .manual_media_authority("fabricated.jpg", &canonical_fingerprint)
            .unwrap_err()
            .contains("no canonical JobAsset"));
        close(&root, store);
    }

    #[test]
    fn manual_face_commit_rejects_a_changed_canonical_asset_under_the_write_lock() {
        let root = workspace("manual-media-authority-fence");
        let store = MatchStore::open(&root).unwrap();
        let media_key = "media/manual-fenced.jpg";
        let fingerprint = "manual-fenced-fingerprint";
        let person = store.create_person("Manual Fence", Vec::new()).unwrap();
        let authority = seed_manual_media_authority(&store, &root, media_key, fingerprint);
        let execution = store.execution_state().unwrap();
        let original_asset: JobAsset = store
            .require(JOB_ASSET_TABLE, &authority.fence.asset_id, "JobAsset")
            .unwrap();
        let replacement_asset = JobAsset {
            asset_id: new_id("manual-test-replacement-asset"),
            media_fingerprint: "manual-replacement-fingerprint".to_string(),
            source_path: Some(
                authority
                    .root_path
                    .join("replacement.jpg")
                    .to_string_lossy()
                    .to_string(),
            ),
            updated_at: "9999-12-31T23:59:59.999999999Z".to_string(),
            ..original_asset
        };
        store
            .upsert_json(
                JOB_ASSET_TABLE,
                &replacement_asset.asset_id,
                &replacement_asset,
            )
            .unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();
        let input = ManualFaceInput {
            expected_schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            expected_model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            expected_catalog_revision: execution.catalog_revision,
            media_key: media_key.to_string(),
            media_fingerprint: fingerprint.to_string(),
            authority: authority.fence,
            source_width: 640,
            source_height: 480,
            display_region: NormalizedRegion {
                x: 0.1,
                y: 0.1,
                width: 0.4,
                height: 0.4,
            },
            exif_orientation: ExifOrientation::Normal,
            display_landmarks: Vec::new(),
            alignment_valid: false,
            quality: 0.0,
            pose_bucket: "manual_unaligned".to_string(),
            embedding: None,
        };
        let error = store.create_manual_face(input.clone()).unwrap_err();
        assert_eq!(error, "stale manual-face canonical media authority");
        let assigned_error = store
            .create_manual_face_and_assign(input, &person.person_id, person.revision)
            .unwrap_err();
        assert_eq!(
            assigned_error,
            "stale manual-face canonical media authority"
        );
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        assert_eq!(store.execution_state().unwrap(), execution);
        assert_eq!(store.count(FACE_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn viewer_snapshot_keeps_more_than_one_thousand_faces_operable_and_ordered() {
        let root = workspace("viewer-dense");
        let store = MatchStore::open(&root).unwrap();
        let media_key = "media/dense.jpg";
        let owned = (0..1001_u32)
            .rev()
            .map(|source_index| {
                let mut face = manual_face(&format!("dense-{source_index:04}"), media_key);
                face.source_index = source_index;
                (
                    FACE_TABLE.to_string(),
                    face.face_id.clone(),
                    serde_json::to_value(face).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let borrowed = owned
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        {
            let _guard = store.store.transaction_lock().write().unwrap();
            store
                .transactional_upserts_deletes_unlocked(&borrowed, &[])
                .unwrap();
        }
        let snapshot = store.media_faces(media_key).unwrap();
        assert_eq!(snapshot.total_faces, 1001);
        assert_eq!(snapshot.rows.first().unwrap().face.source_index, 0);
        assert_eq!(snapshot.rows.last().unwrap().face.source_index, 1000);
        assert!(snapshot.rows.windows(2).all(|rows| {
            (rows[0].face.source_index, rows[0].face.face_id.as_str())
                < (rows[1].face.source_index, rows[1].face.face_id.as_str())
        }));
        close(&root, store);
    }

    #[test]
    fn person_face_page_uses_canonical_assignments_and_exact_total_beyond_page() {
        let root = workspace("person-face-page");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Paged", Vec::new()).unwrap();
        store.create_look(&person.person_id, "Zulu").unwrap();
        store.create_look(&person.person_id, "Alpha").unwrap();
        for index in (0..7).rev() {
            let face_id = format!("page-face-{index}");
            store
                .create_face(manual_face(&face_id, &format!("media/page-{index}.jpg")))
                .unwrap();
            confirm(&store, &face_id, &person);
        }
        let first = store.person_face_page(&person.person_id, 0, 3).unwrap();
        let second = store.person_face_page(&person.person_id, 3, 3).unwrap();
        assert_eq!(first.total_faces, 7);
        assert_eq!(first.total_media, 7);
        assert_eq!(first.total_looks, 2);
        assert_eq!(
            first
                .looks
                .iter()
                .map(|look| look.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "Zulu"]
        );
        assert_eq!(first.rows.len(), 3);
        assert_eq!(second.rows.len(), 3);
        assert_eq!(
            first
                .rows
                .iter()
                .chain(&second.rows)
                .map(|row| row.face_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "page-face-0",
                "page-face-1",
                "page-face-2",
                "page-face-3",
                "page-face-4",
                "page-face-5",
            ]
        );
        assert_eq!(
            store
                .media_faces("media/page-0.jpg")
                .unwrap()
                .looks
                .iter()
                .map(|look| look.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "Zulu"]
        );
        close(&root, store);
    }

    #[test]
    fn deleting_face_analysis_never_deletes_media() {
        let root = workspace("no-media-delete");
        let media = root.join("kept.jpg");
        std::fs::write(&media, b"not-an-image-but-owned-media").unwrap();
        let store = MatchStore::open(&root).unwrap();
        store
            .create_face(manual_face("face-delete-analysis", "media/kept.jpg"))
            .unwrap();
        let fence = store.correction_fence("face-delete-analysis").unwrap();
        store
            .delete_face_analysis("face-delete-analysis", &fence)
            .unwrap();
        assert!(media.exists());
        close(&root, store);
    }

    #[test]
    fn previewed_merge_and_split_use_stable_ids_and_exact_rows() {
        let root = workspace("merge-split");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        let receiver = store.create_person("Receiver", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-merge-a", "media/a.jpg"))
            .unwrap();
        store
            .create_face(manual_face("face-merge-b", "media/b.jpg"))
            .unwrap();
        confirm(&store, "face-merge-a", &source);
        confirm(&store, "face-merge-b", &source);

        let preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(
            preview,
            store
                .preview_merge_people(&source.person_id, &target.person_id)
                .unwrap()
        );
        assert_eq!(preview.face_ids, vec!["face-merge-a", "face-merge-b"]);
        store.merge_people(&preview).unwrap();
        let moved: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-merge-a", "Assignment")
            .unwrap();
        assert_eq!(moved.person_id, target.person_id);

        let split = store
            .preview_split_to_person(
                &target.person_id,
                &receiver.person_id,
                vec!["face-merge-a".to_string()],
            )
            .unwrap();
        let split_fences = vec![store
            .correction_fence_for(
                "face-merge-a",
                vec![target.person_id.clone(), receiver.person_id.clone()],
            )
            .unwrap()];
        let destination_id = receiver.person_id.clone();
        store.split_to_person(&split, split_fences).unwrap();
        let split_assignment: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-merge-a", "Assignment")
            .unwrap();
        assert_eq!(split_assignment.person_id, destination_id);
        assert_eq!(split_assignment.placement, "unsorted");
        close(&root, store);
    }

    #[test]
    fn repeated_merge_revisions_rebind_surviving_target_operator_assignments_and_undo() {
        let root = workspace("merge-target-assignment-revision");
        let store = MatchStore::open(&root).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        store
            .create_face(manual_face(
                "face-merge-target-existing",
                "media/merge-target-existing.jpg",
            ))
            .unwrap();
        confirm(&store, "face-merge-target-existing", &target);

        let first_source = store.create_person("First Source", Vec::new()).unwrap();
        let first_preview = store
            .preview_merge_people(&first_source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(first_preview.assignment_count, 0);
        assert_eq!(first_preview.delta_counts.assignments, 1);
        assert_eq!(
            first_preview.face_ids,
            vec!["face-merge-target-existing".to_string()]
        );
        store.merge_people(&first_preview).unwrap();

        let revised_target = store
            .require::<Person>(PERSON_TABLE, &target.person_id, "merge target")
            .unwrap();
        let revised_target = store
            .update_person_preferences(
                &revised_target.person_id,
                revised_target.revision,
                None,
                false,
                true,
            )
            .unwrap();
        let second_source = store.create_person("Second Source", Vec::new()).unwrap();
        let second_preview = store
            .preview_merge_people(&second_source.person_id, &revised_target.person_id)
            .unwrap();
        assert_eq!(second_preview.assignment_count, 0);
        assert_eq!(second_preview.delta_counts.assignments, 1);
        let second_receipt = store.merge_people(&second_preview).unwrap();

        let merged_target = store
            .require::<Person>(PERSON_TABLE, &target.person_id, "merged target")
            .unwrap();
        let merged_assignment = store
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                "face-merge-target-existing",
                "surviving target Assignment",
            )
            .unwrap();
        assert_eq!(
            merged_assignment.state,
            AssignmentState::OperatorConfirmed.as_str()
        );
        assert_eq!(merged_assignment.person_revision, merged_target.revision);
        assert_eq!(merged_assignment.operation_id, second_receipt.operation_id);

        store.undo_correction(&second_receipt.operation_id).unwrap();
        let restored_target = store
            .require::<Person>(PERSON_TABLE, &target.person_id, "restored target")
            .unwrap();
        let restored_assignment = store
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                "face-merge-target-existing",
                "restored target Assignment",
            )
            .unwrap();
        assert_eq!(
            restored_assignment.state,
            AssignmentState::OperatorConfirmed.as_str()
        );
        assert_eq!(
            restored_assignment.person_revision,
            restored_target.revision
        );
        assert_eq!(
            restored_assignment.operation_id,
            format!("undo-{}", second_receipt.operation_id)
        );
        close(&root, store);
    }

    #[test]
    fn merge_invalidates_both_people_derived_evidence_and_undo_restores_it_stale_exactly() {
        let root = workspace("merge-invalidates-derived-evidence");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Source", Vec::new()).unwrap();
        let target = store.create_person("Target", Vec::new()).unwrap();
        for (face_id, media_key) in [
            ("merge-source-operator", "media/merge-source-operator.jpg"),
            ("merge-target-operator", "media/merge-target-operator.jpg"),
            ("merge-source-strict", "media/merge-source-strict.jpg"),
            ("merge-target-strict", "media/merge-target-strict.jpg"),
            (
                "merge-source-suggestion",
                "media/merge-source-suggestion.jpg",
            ),
            (
                "merge-target-suggestion",
                "media/merge-target-suggestion.jpg",
            ),
        ] {
            store.create_face(manual_face(face_id, media_key)).unwrap();
        }
        let source_operator = confirm(&store, "merge-source-operator", &source);
        let target_operator = confirm(&store, "merge-target-operator", &target);
        let source_strict =
            strict_assignment_without_revision_bump(&store, "merge-source-strict", &source);
        let target_strict =
            strict_assignment_without_revision_bump(&store, "merge-target-strict", &target);
        let source_suggestion = suggest_for_review(&store, "merge-source-suggestion", &source);
        let target_suggestion = suggest_for_review(&store, "merge-target-suggestion", &target);

        let preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        assert_eq!(preview.assignment_count, 2);
        assert_eq!(preview.delta_counts.assignments, 4);
        assert_eq!(preview.delta_counts.suggestions, 2);
        assert_eq!(preview.required_reversible_rows, 8);
        assert_eq!(preview.affected_counts.faces, 6);
        assert_eq!(preview.affected_counts.media, 6);

        let receipt = store.merge_people(&preview).unwrap();
        let merged_target = store
            .require::<Person>(PERSON_TABLE, &target.person_id, "merged target")
            .unwrap();
        for face_id in ["merge-source-strict", "merge-target-strict"] {
            assert!(store
                .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
                .unwrap()
                .is_none());
        }
        for suggestion_id in [
            &source_suggestion.suggestion_id,
            &target_suggestion.suggestion_id,
        ] {
            assert!(store
                .get_one::<Suggestion>(SUGGESTION_TABLE, suggestion_id)
                .unwrap()
                .is_none());
        }
        for face_id in ["merge-source-operator", "merge-target-operator"] {
            let assignment = store
                .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "surviving operator Assignment")
                .unwrap();
            assert_eq!(assignment.person_id, merged_target.person_id);
            assert_eq!(assignment.person_revision, merged_target.revision);
            assert_eq!(assignment.operation_id, receipt.operation_id);
        }
        store
            .export_identity_bundle(&root.join("merge-derived-valid.json"))
            .unwrap();

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        reopened.undo_correction(&receipt.operation_id).unwrap();
        let restored_source = reopened
            .require::<Person>(PERSON_TABLE, &source.person_id, "restored source")
            .unwrap();
        let restored_target = reopened
            .require::<Person>(PERSON_TABLE, &target.person_id, "restored target")
            .unwrap();

        let restored_source_strict = reopened
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                &source_strict.assignment_id,
                "restored source strict Assignment",
            )
            .unwrap();
        let restored_target_strict = reopened
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                &target_strict.assignment_id,
                "restored target strict Assignment",
            )
            .unwrap();
        assert_eq!(restored_source_strict, source_strict);
        assert_eq!(restored_target_strict, target_strict);
        assert_ne!(
            restored_source_strict.person_revision,
            restored_source.revision
        );
        assert_ne!(
            restored_target_strict.person_revision,
            restored_target.revision
        );
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &source_suggestion.suggestion_id,
                    "restored source Suggestion",
                )
                .unwrap(),
            source_suggestion
        );
        assert_eq!(
            reopened
                .require::<Suggestion>(
                    SUGGESTION_TABLE,
                    &target_suggestion.suggestion_id,
                    "restored target Suggestion",
                )
                .unwrap(),
            target_suggestion
        );

        let restored_source_operator = reopened
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                &source_operator.assignment_id,
                "restored source operator Assignment",
            )
            .unwrap();
        let restored_target_operator = reopened
            .require::<Assignment>(
                ASSIGNMENT_TABLE,
                &target_operator.assignment_id,
                "restored target operator Assignment",
            )
            .unwrap();
        assert_eq!(
            restored_source_operator.person_id,
            restored_source.person_id
        );
        assert_eq!(
            restored_source_operator.person_revision,
            restored_source.revision
        );
        assert_eq!(
            restored_target_operator.person_id,
            restored_target.person_id
        );
        assert_eq!(
            restored_target_operator.person_revision,
            restored_target.revision
        );
        close(&root, reopened);
    }

    #[test]
    fn undo_conflict_is_atomic_and_retryable_after_exact_after_state_is_restored() {
        let root = workspace("conditional-undo");
        let store = MatchStore::open(&root).unwrap();
        let first = store.create_person("First", Vec::new()).unwrap();
        let second = store.create_person("Second", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-undo", "media/undo.jpg"))
            .unwrap();
        confirm(&store, "face-undo", &first);
        let change = store
            .change_person(
                "face-undo",
                &second.person_id,
                &correction_for_target(&store, "face-undo", &second),
            )
            .unwrap();
        let operation_after: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-undo", "Assignment")
            .unwrap();
        let constraint_id = cannot_link_id("face-undo", &first.person_id);
        let operation_constraint: CannotLinkConstraint = store
            .require(CONSTRAINT_TABLE, &constraint_id, "CannotLinkConstraint")
            .unwrap();
        let execution_after = store.execution_state().unwrap();

        let mut conflicting_assignment = operation_after.clone();
        conflicting_assignment.provenance = "independent-import".to_string();
        store
            .upsert_json(ASSIGNMENT_TABLE, "face-undo", &conflicting_assignment)
            .unwrap();
        let error = store.undo_correction(&change.operation_id).unwrap_err();
        assert!(error.contains("undo conflict"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "face-undo", "Assignment")
                .unwrap(),
            conflicting_assignment
        );
        assert_eq!(
            store
                .require::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &constraint_id,
                    "CannotLinkConstraint",
                )
                .unwrap(),
            operation_constraint
        );
        let execution_after_conflict = store.execution_state().unwrap();
        assert_eq!(
            execution_after_conflict.identity_revision,
            execution_after.identity_revision
        );
        assert_eq!(
            execution_after_conflict.catalog_revision,
            execution_after.catalog_revision
        );
        assert!(store
            .get_one::<MatchOperation>(OPERATION_TABLE, &format!("undo-{}", change.operation_id),)
            .unwrap()
            .is_none());

        store
            .upsert_json(ASSIGNMENT_TABLE, "face-undo", &operation_after)
            .unwrap();
        let undo = store.undo_correction(&change.operation_id).unwrap();
        assert_eq!(undo.conflict_rows, 0);
        let restored: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-undo", "Assignment")
            .unwrap();
        assert_eq!(restored.person_id, first.person_id);
        assert!(store
            .get_one::<CannotLinkConstraint>(CONSTRAINT_TABLE, &constraint_id)
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn undo_split_rejects_a_later_assignment_to_the_created_person() {
        let root = workspace("undo-split-dependent-assignment");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Split Source", Vec::new()).unwrap();
        store
            .create_face(manual_face("split-original", "media/split-original.jpg"))
            .unwrap();
        confirm(&store, "split-original", &source);
        let preview = store
            .preview_split_person(
                &source.person_id,
                vec!["split-original".to_string()],
                "Split Destination",
            )
            .unwrap();
        let destination_id = match &preview.kind {
            PersonEditKind::Split {
                destination_person_id,
                ..
            } => destination_person_id.clone(),
            _ => unreachable!(),
        };
        let split = store.split_person(&preview).unwrap();
        let destination: Person = store
            .require(PERSON_TABLE, &destination_id, "split destination")
            .unwrap();
        store
            .create_face(manual_face("split-later", "media/split-later.jpg"))
            .unwrap();
        confirm(&store, "split-later", &destination);

        let error = store.undo_correction(&split.operation_id).unwrap_err();
        assert!(error.contains("undo dependency conflict"));
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &destination_id)
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "split-later", "later assignment")
                .unwrap()
                .person_id,
            destination_id
        );

        let removal_fence = store
            .correction_fence_for("split-later", vec![destination_id.clone()])
            .unwrap();
        store
            .remove_assignment_correction("split-later", &destination_id, &removal_fence)
            .unwrap();
        store.undo_correction(&split.operation_id).unwrap();
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &destination_id)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "split-original", "restored assignment")
                .unwrap()
                .person_id,
            source.person_id
        );
        close(&root, store);
    }

    #[test]
    fn undo_merge_rejects_a_later_assignment_into_the_moved_look() {
        let root = workspace("undo-merge-dependent-look");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Merge Source", Vec::new()).unwrap();
        let target = store.create_person("Merge Target", Vec::new()).unwrap();
        let look = store.create_look(&source.person_id, "Source Look").unwrap();
        store
            .create_face(manual_face("merge-original", "media/merge-original.jpg"))
            .unwrap();
        confirm(&store, "merge-original", &source);
        store
            .move_to_look_correction(
                "merge-original",
                &look.look_id,
                &correction_for_target(&store, "merge-original", &source),
            )
            .unwrap();
        let merge_preview = store
            .preview_merge_people(&source.person_id, &target.person_id)
            .unwrap();
        let merge = store.merge_people(&merge_preview).unwrap();
        let current_target: Person = store
            .require(PERSON_TABLE, &target.person_id, "merge target")
            .unwrap();
        store
            .create_face(manual_face("merge-later", "media/merge-later.jpg"))
            .unwrap();
        confirm(&store, "merge-later", &current_target);
        store
            .move_to_look_correction(
                "merge-later",
                &look.look_id,
                &correction_for_target(&store, "merge-later", &current_target),
            )
            .unwrap();

        let error = store.undo_correction(&merge.operation_id).unwrap_err();
        assert!(error.contains("cross Person/Look ownership"));
        assert_eq!(
            store
                .require::<Look>(LOOK_TABLE, &look.look_id, "moved Look")
                .unwrap()
                .person_id,
            target.person_id
        );

        let removal_fence = store
            .correction_fence_for("merge-later", vec![target.person_id.clone()])
            .unwrap();
        store
            .remove_assignment_correction("merge-later", &target.person_id, &removal_fence)
            .unwrap();
        store.undo_correction(&merge.operation_id).unwrap();
        assert_eq!(
            store
                .require::<Look>(LOOK_TABLE, &look.look_id, "restored Look")
                .unwrap()
                .person_id,
            source.person_id
        );
        close(&root, store);
    }

    #[test]
    fn different_rejects_an_unrelated_person_without_mutation() {
        let root = workspace("different-unrelated");
        let store = MatchStore::open(&root).unwrap();
        let assigned = store.create_person("Assigned", Vec::new()).unwrap();
        let unrelated = store.create_person("Unrelated", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-unrelated", "media/unrelated.jpg"))
            .unwrap();
        let original = confirm(&store, "face-unrelated", &assigned);
        let execution = store.execution_state().unwrap();
        let error = store
            .different_correction(
                "face-unrelated",
                &unrelated.person_id,
                &store
                    .correction_fence_for(
                        "face-unrelated",
                        vec![assigned.person_id.clone(), unrelated.person_id.clone()],
                    )
                    .unwrap(),
            )
            .unwrap_err();
        assert_eq!(
            error,
            "Different requires a matching suggestion or same-Person committed_strict_automatic assignment"
        );
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "face-unrelated", "Assignment")
                .unwrap(),
            original
        );
        assert!(store
            .get_one::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &cannot_link_id("face-unrelated", &unrelated.person_id),
            )
            .unwrap()
            .is_none());
        let remove_error = store
            .remove_assignment_correction(
                "face-unrelated",
                &unrelated.person_id,
                &store
                    .correction_fence_for(
                        "face-unrelated",
                        vec![assigned.person_id.clone(), unrelated.person_id.clone()],
                    )
                    .unwrap(),
            )
            .unwrap_err();
        assert!(remove_error.contains("source Person does not own the assignment"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "face-unrelated", "Assignment")
                .unwrap(),
            original
        );
        let after = store.execution_state().unwrap();
        assert_eq!(after.identity_revision, execution.identity_revision);
        assert_eq!(after.catalog_revision, execution.catalog_revision);
        close(&root, store);
    }

    #[test]
    fn reversible_trust_cleanup_is_typed_and_restart_undo_restores_exact_rows() {
        let root = workspace("trusted-restart-undo");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Trusted", Vec::new()).unwrap();
        let look = store.create_look(&person.person_id, "Reference").unwrap();
        store
            .create_face(manual_face("face-trusted", "media/trusted.jpg"))
            .unwrap();
        confirm(&store, "face-trusted", &person);
        store
            .move_to_look_correction(
                "face-trusted",
                &look.look_id,
                &correction_for_target(&store, "face-trusted", &person),
            )
            .unwrap();
        let face: FaceObservation = store
            .require(FACE_TABLE, "face-trusted", "FaceObservation")
            .unwrap();
        let model_generation = "model-wp084-restart";
        store
            .register_model_generation(model_generation, true)
            .unwrap();
        let embedding = FaceEmbedding {
            embedding_id: "embedding-wp084-restart".to_string(),
            face_id: face.face_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: model_generation.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: "embedding-job-wp084-restart".to_string(),
            active: true,
            created_at: now(),
        };
        let membership_id = "membership-wp084-restart".to_string();
        let membership = TrustedTemplateMembership {
            membership_id: membership_id.clone(),
            set_id: "template-set-wp084-restart".to_string(),
            look_id: look.look_id.clone(),
            face_id: face.face_id.clone(),
            authorized: true,
            alignment_valid: true,
            quality_passed: true,
            pose_passed: true,
            diversity_passed: true,
            provenance: "operator_trusted_reference".to_string(),
            model_generation: model_generation.to_string(),
            embedding_id: embedding.embedding_id.clone(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            quality_score: 0.99,
            quality_threshold: 0.8,
            pose_bucket: face.pose_bucket.clone(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
            operation_id: "trusted-enrollment-wp084-restart".to_string(),
            created_at: now(),
        };
        let search = TrustedSearchEmbedding {
            membership_id: membership_id.clone(),
            person_id: person.person_id.clone(),
            look_id: look.look_id.clone(),
            face_id: face.face_id.clone(),
            embedding_id: membership.embedding_id.clone(),
            // Cosine HNSW fixtures must have a defined direction. A zero
            // vector has no cosine norm and can exercise engine-specific
            // failure behavior unrelated to correction rollback semantics.
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: membership.model_generation.clone(),
            created_at: membership.created_at.clone(),
        };
        store
            .transactional_upserts_deletes(
                &[
                    (
                        EMBEDDING_TABLE,
                        &embedding.embedding_id,
                        serde_json::to_value(&embedding).unwrap(),
                    ),
                    (
                        TRUSTED_MEMBER_TABLE,
                        &membership_id,
                        serde_json::to_value(&membership).unwrap(),
                    ),
                    (
                        TRUSTED_SEARCH_TABLE,
                        &membership_id,
                        serde_json::to_value(&search).unwrap(),
                    ),
                ],
                &[],
            )
            .unwrap();
        let removed = store
            .remove_assignment_correction(
                "face-trusted",
                &person.person_id,
                &correction_for_target(&store, "face-trusted", &person),
            )
            .unwrap();
        let operation: MatchOperation = store
            .require(OPERATION_TABLE, &removed.operation_id, "MatchOperation")
            .unwrap();
        let envelope: CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        assert!(envelope.rows.iter().any(|row| {
            row.table == CorrectionTable::TrustedMember && row.stable_id == membership_id
        }));
        assert!(envelope.rows.iter().any(|row| {
            row.table == CorrectionTable::TrustedSearch && row.stable_id == membership_id
        }));
        assert!(store
            .get_one::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE, &membership_id)
            .unwrap()
            .is_none());
        assert!(store
            .get_one::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE, &membership_id)
            .unwrap()
            .is_none());
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let undo = reopened.undo_correction(&removed.operation_id).unwrap();
        let mut expected_membership = membership;
        expected_membership.operation_id = undo.operation_id.clone();
        assert_eq!(
            reopened
                .require::<TrustedTemplateMembership>(
                    TRUSTED_MEMBER_TABLE,
                    &membership_id,
                    "TrustedTemplateMembership",
                )
                .unwrap(),
            expected_membership
        );
        assert_eq!(
            reopened
                .require::<TrustedSearchEmbedding>(
                    TRUSTED_SEARCH_TABLE,
                    &membership_id,
                    "TrustedSearchEmbedding",
                )
                .unwrap(),
            search
        );
        close(&root, reopened);
    }

    #[test]
    fn same_and_not_sure_require_unresolved_candidate_provenance() {
        let root = workspace("candidate-provenance");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Candidate", Vec::new()).unwrap();

        for face_id in ["unidentified-same", "unidentified-not-sure"] {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
        }
        let same_error = store
            .same_correction(
                "unidentified-same",
                &person.person_id,
                &correction_for_target(&store, "unidentified-same", &person),
            )
            .unwrap_err();
        assert!(same_error.contains("requires a matching suggestion"));
        let not_sure_error = store
            .not_sure_correction(
                "unidentified-not-sure",
                &person.person_id,
                &correction_for_target(&store, "unidentified-not-sure", &person),
            )
            .unwrap_err();
        assert!(not_sure_error.contains("requires a matching suggestion"));

        let unrelated = store.create_person("Unrelated", Vec::new()).unwrap();
        store
            .create_face(manual_face(
                "unrelated-review",
                "media/unrelated-review.jpg",
            ))
            .unwrap();
        let unrelated_before = confirm(&store, "unrelated-review", &unrelated);
        let unrelated_fence = store
            .correction_fence_for(
                "unrelated-review",
                vec![person.person_id.clone(), unrelated.person_id.clone()],
            )
            .unwrap();
        assert!(store
            .same_correction("unrelated-review", &person.person_id, &unrelated_fence,)
            .unwrap_err()
            .contains("different from the committed assignment"));
        assert!(store
            .not_sure_correction("unrelated-review", &person.person_id, &unrelated_fence,)
            .unwrap_err()
            .contains("different from the committed assignment"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "unrelated-review", "Assignment")
                .unwrap(),
            unrelated_before
        );

        store
            .create_face(manual_face("strict-same", "media/strict-same.jpg"))
            .unwrap();
        strict_assignment_without_revision_bump(&store, "strict-same", &person);
        let same = store
            .same_correction(
                "strict-same",
                &person.person_id,
                &correction_for_target(&store, "strict-same", &person),
            )
            .unwrap();
        assert_eq!(same.kind, "same");
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "strict-same", "Assignment")
                .unwrap()
                .state,
            AssignmentState::OperatorConfirmed.as_str()
        );

        store
            .create_face(manual_face("strict-not-sure", "media/strict-not-sure.jpg"))
            .unwrap();
        let strict_before =
            strict_assignment_without_revision_bump(&store, "strict-not-sure", &person);
        let not_sure = store
            .not_sure_correction(
                "strict-not-sure",
                &person.person_id,
                &correction_for_target(&store, "strict-not-sure", &person),
            )
            .unwrap();
        assert_eq!(not_sure.kind, "not_sure");
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "strict-not-sure", "Assignment")
                .unwrap(),
            strict_before
        );
        close(&root, store);
    }

    #[test]
    fn stale_candidate_generation_media_face_and_person_revisions_are_hidden_and_unconfirmable() {
        let root = workspace("stale-candidate-provenance");
        let store = MatchStore::open(&root).unwrap();

        for stale_axis in ["generation", "media", "face", "person", "embedding"] {
            let suggestion_face_id = format!("stale-suggestion-{stale_axis}");
            let strict_face_id = format!("stale-strict-{stale_axis}");
            let suggestion_media = format!("media/{suggestion_face_id}.jpg");
            let strict_media = format!("media/{strict_face_id}.jpg");
            let suggestion_person = store
                .create_person(&format!("Suggestion {stale_axis}"), Vec::new())
                .unwrap();
            let strict_person = store
                .create_person(&format!("Strict {stale_axis}"), Vec::new())
                .unwrap();
            let replacement_person = store
                .create_person(&format!("Replacement {stale_axis}"), Vec::new())
                .unwrap();
            store
                .create_face(manual_face(&suggestion_face_id, &suggestion_media))
                .unwrap();
            store
                .create_face(manual_face(&strict_face_id, &strict_media))
                .unwrap();
            suggest_for_review(&store, &suggestion_face_id, &suggestion_person);
            strict_assignment_without_revision_bump(&store, &strict_face_id, &strict_person);

            match stale_axis {
                "generation" => {
                    let mut generation: ModelGeneration = store
                        .require(
                            GENERATION_TABLE,
                            UNCONFIGURED_MODEL_GENERATION,
                            "ModelGeneration",
                        )
                        .unwrap();
                    generation.state = "usable".to_string();
                    generation.updated_at = now();
                    store
                        .upsert_json(GENERATION_TABLE, UNCONFIGURED_MODEL_GENERATION, &generation)
                        .unwrap();
                }
                "media" => {
                    for face_id in [&suggestion_face_id, &strict_face_id] {
                        let asset_id = format!("wp084-candidate-asset-{face_id}");
                        let mut asset: JobAsset = store
                            .require(JOB_ASSET_TABLE, &asset_id, "JobAsset")
                            .unwrap();
                        asset.media_fingerprint = format!("stale-{}", asset.media_fingerprint);
                        store
                            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
                            .unwrap();
                    }
                }
                "face" => {
                    for face_id in [&suggestion_face_id, &strict_face_id] {
                        let mut face: FaceObservation =
                            store.require(FACE_TABLE, face_id, "Face").unwrap();
                        face.face_revision += 1;
                        face.updated_at = now();
                        store.upsert_json(FACE_TABLE, face_id, &face).unwrap();
                    }
                }
                "person" => {
                    for person_id in [
                        suggestion_person.person_id.as_str(),
                        strict_person.person_id.as_str(),
                    ] {
                        let mut person: Person =
                            store.require(PERSON_TABLE, person_id, "Person").unwrap();
                        person.revision += 1;
                        person.updated_at = now();
                        store.upsert_json(PERSON_TABLE, person_id, &person).unwrap();
                    }
                }
                "embedding" => {
                    for face_id in [&suggestion_face_id, &strict_face_id] {
                        let embedding_id = embedding_id(face_id, UNCONFIGURED_MODEL_GENERATION);
                        let mut embedding: FaceEmbedding = store
                            .require(EMBEDDING_TABLE, &embedding_id, "FaceEmbedding")
                            .unwrap();
                        embedding.active = false;
                        store
                            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
                            .unwrap();
                    }
                }
                _ => unreachable!(),
            }

            let suggestion_snapshot = store.media_faces(&suggestion_media).unwrap();
            assert!(suggestion_snapshot.rows[0].suggestion.is_none());
            assert!(suggestion_snapshot.rows[0].candidate_person.is_none());
            let strict_snapshot = store.media_faces(&strict_media).unwrap();
            assert!(strict_snapshot.rows[0].assignment.is_none());
            assert!(strict_snapshot.rows[0].person.is_none());

            let current_suggestion_person: Person = store
                .require(
                    PERSON_TABLE,
                    &suggestion_person.person_id,
                    "suggestion Person",
                )
                .unwrap();
            let current_strict_person: Person = store
                .require(PERSON_TABLE, &strict_person.person_id, "strict Person")
                .unwrap();
            let suggestion_error = store
                .same_correction(
                    &suggestion_face_id,
                    &current_suggestion_person.person_id,
                    &correction_for_target(&store, &suggestion_face_id, &current_suggestion_person),
                )
                .unwrap_err();
            assert!(suggestion_error.contains("stale suggestion provenance"));
            let strict_error = store
                .not_sure_correction(
                    &strict_face_id,
                    &current_strict_person.person_id,
                    &correction_for_target(&store, &strict_face_id, &current_strict_person),
                )
                .unwrap_err();
            assert!(strict_error.contains("stale committed_strict_automatic provenance"));
            let stale_fence = store
                .correction_fence_for(
                    &suggestion_face_id,
                    vec![
                        current_suggestion_person.person_id.clone(),
                        replacement_person.person_id.clone(),
                    ],
                )
                .unwrap();
            assert!(store
                .different_correction(
                    &suggestion_face_id,
                    &current_suggestion_person.person_id,
                    &stale_fence,
                )
                .unwrap_err()
                .contains("stale suggestion provenance"));
            assert!(store
                .batch_correction_preflight(
                    BatchCorrectionAction::ThisIsNot,
                    Some(&current_suggestion_person.person_id),
                    None,
                    vec![suggestion_face_id.clone()],
                    vec![stale_fence.clone()],
                )
                .unwrap_err()
                .contains("requires a matching committed assignment"));
            assert!(store
                .change_person_correction(
                    &suggestion_face_id,
                    &current_suggestion_person.person_id,
                    &replacement_person.person_id,
                    &stale_fence,
                )
                .unwrap_err()
                .contains("stale suggestion provenance"));

            if stale_axis == "generation" {
                let mut generation: ModelGeneration = store
                    .require(
                        GENERATION_TABLE,
                        UNCONFIGURED_MODEL_GENERATION,
                        "ModelGeneration",
                    )
                    .unwrap();
                generation.state = "active".to_string();
                generation.updated_at = now();
                store
                    .upsert_json(GENERATION_TABLE, UNCONFIGURED_MODEL_GENERATION, &generation)
                    .unwrap();
            }
        }
        close(&root, store);
    }

    #[test]
    fn typed_same_not_sure_look_and_remove_person_round_trip() {
        let root = workspace("typed-single-face");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Typed", Vec::new()).unwrap();
        store
            .create_face(manual_face("face-typed", "media/typed.jpg"))
            .unwrap();
        suggest_for_review(&store, "face-typed", &person);
        let same = store
            .same_correction(
                "face-typed",
                &person.person_id,
                &correction_for_target(&store, "face-typed", &person),
            )
            .unwrap();
        assert_eq!(same.kind, "same");
        let already_confirmed_error = store
            .not_sure_correction(
                "face-typed",
                &person.person_id,
                &correction_for_target(&store, "face-typed", &person),
            )
            .unwrap_err();
        assert!(already_confirmed_error.contains("already operator-confirmed"));

        store
            .create_face(manual_face("face-not-sure", "media/not-sure.jpg"))
            .unwrap();
        suggest_for_review(&store, "face-not-sure", &person);
        let not_sure = store
            .not_sure_correction(
                "face-not-sure",
                &person.person_id,
                &correction_for_target(&store, "face-not-sure", &person),
            )
            .unwrap();
        assert_eq!(not_sure.changed_rows, 0);
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-not-sure")
            .unwrap()
            .is_none());

        let created_look = store
            .same_person_new_look_correction(
                "face-typed",
                &person.person_id,
                "Created Look",
                &correction_for_target(&store, "face-typed", &person),
            )
            .unwrap();
        let in_created_look: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-typed", "Assignment")
            .unwrap();
        let created_look_id = in_created_look.look_id.clone().unwrap();
        let snapshot = store.media_faces("media/typed.jpg").unwrap();
        assert_eq!(
            snapshot
                .looks
                .iter()
                .map(|look| look.look_id.as_str())
                .collect::<Vec<_>>(),
            vec![created_look_id.as_str()]
        );
        store.undo_correction(&created_look.operation_id).unwrap();
        assert!(store
            .get_one::<Look>(LOOK_TABLE, &created_look_id)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "face-typed", "Assignment")
                .unwrap()
                .placement,
            "unsorted"
        );

        let look = store.create_look(&person.person_id, "Look").unwrap();
        let moved = store
            .move_to_look_correction(
                "face-typed",
                &look.look_id,
                &correction_for_target(&store, "face-typed", &person),
            )
            .unwrap();
        store.undo_correction(&moved.operation_id).unwrap();
        let unsorted: Assignment = store
            .require(ASSIGNMENT_TABLE, "face-typed", "Assignment")
            .unwrap();
        assert_eq!(unsorted.placement, "unsorted");

        let preview = store.preview_remove_person(&person.person_id).unwrap();
        let removed = store.remove_person(&preview).unwrap();
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "face-typed")
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-typed")
            .unwrap()
            .is_none());
        store.undo_correction(&removed.operation_id).unwrap();
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-typed")
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn batch_preflight_is_action_specific_and_apply_matches_the_exact_delta() {
        let root = workspace("batch-action-specific-preview");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Previewed", Vec::new()).unwrap();
        let face_ids = vec!["preview-a".to_string(), "preview-b".to_string()];
        for face_id in &face_ids {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            suggest_for_review(&store, face_id, &person);
        }
        let fences = || {
            face_ids
                .iter()
                .map(|face_id| correction_for_target(&store, face_id, &person))
                .collect::<Vec<_>>()
        };
        let not_sure = store
            .batch_correction_preflight(
                BatchCorrectionAction::NotSure,
                Some(&person.person_id),
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap();
        let same = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap();
        assert_eq!(not_sure.required_reversible_rows, 0);
        assert_eq!(not_sure.delta_counts, BatchCorrectionDeltaCounts::default());
        assert_eq!(same.delta_counts.assignments, face_ids.len());
        assert_eq!(same.delta_counts.suggestions, face_ids.len());
        assert_eq!(same.required_reversible_rows, face_ids.len() * 2);
        assert_ne!(same.preview_id, not_sure.preview_id);
        assert_ne!(same.delta_digest, not_sure.delta_digest);
        assert_eq!(same.affected_counts.persons, 1);
        assert_eq!(same.affected_counts.faces, 2);
        assert_eq!(same.affected_counts.media, 2);

        let deferred = store.apply_batch_correction(&not_sure).unwrap();
        assert_eq!(deferred.changed_rows, 0);
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
            .unwrap()
            .is_none()));

        let refreshed_fences = fences();
        let same = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                face_ids.clone(),
                refreshed_fences,
            )
            .unwrap();
        let applied = store.apply_batch_correction(&same).unwrap();
        assert_eq!(applied.operation_id, same.planned_operation_id);
        assert_eq!(applied.changed_rows, same.required_reversible_rows);
        assert_eq!(applied.affected_face_ids, same.face_ids);
        assert_eq!(applied.affected_media_keys, same.media_keys);
        close(&root, store);
    }

    #[test]
    fn batch_suggestion_removals_are_counted_and_restore_exactly_after_restart() {
        let root = workspace("batch-suggestion-reversible-restart");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Suggestion Delta", Vec::new()).unwrap();
        let face_id = "suggestion-delta-face";
        store
            .create_face(manual_face(face_id, "media/suggestion-delta.jpg"))
            .unwrap();
        let suggestion = suggest_for_review(&store, face_id, &person);
        let preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                vec![face_id.to_string()],
                vec![correction_for_target(&store, face_id, &person)],
            )
            .unwrap();
        assert_eq!(preview.delta_counts.assignments, 1);
        assert_eq!(preview.delta_counts.suggestions, 1);
        assert_eq!(preview.required_reversible_rows, 2);
        let receipt = store.apply_batch_correction(&preview).unwrap();
        assert_eq!(receipt.changed_rows, 2);
        assert!(store
            .get_one::<Suggestion>(SUGGESTION_TABLE, &suggestion.suggestion_id)
            .unwrap()
            .is_none());
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        reopened.undo_correction(&receipt.operation_id).unwrap();
        assert_eq!(
            reopened
                .require::<Suggestion>(SUGGESTION_TABLE, &suggestion.suggestion_id, "Suggestion",)
                .unwrap(),
            suggestion
        );
        close(&root, reopened);
    }

    #[test]
    fn batch_apply_rejects_tampered_and_stale_previews_before_mutation() {
        let root = workspace("batch-stale-preview");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Stale Preview", Vec::new()).unwrap();
        store
            .create_face(manual_face("stale-preview-face", "media/stale-preview.jpg"))
            .unwrap();
        suggest_for_review(&store, "stale-preview-face", &person);
        let preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                vec!["stale-preview-face".to_string()],
                vec![correction_for_target(&store, "stale-preview-face", &person)],
            )
            .unwrap();
        let mut tampered = preview.clone();
        tampered.action = BatchCorrectionAction::DeleteFaceAnalysis;
        assert!(store
            .apply_batch_correction(&tampered)
            .unwrap_err()
            .contains("integrity mismatch"));

        let candidate_id = suggestion_id("stale-preview-face", &person.person_id);
        let mut candidate: Suggestion = store
            .require(SUGGESTION_TABLE, &candidate_id, "Suggestion")
            .unwrap();
        let prior_similarity = candidate.similarity;
        candidate.similarity = prior_similarity - 0.01;
        store
            .upsert_json(SUGGESTION_TABLE, &candidate_id, &candidate)
            .unwrap();
        assert!(store
            .apply_batch_correction(&preview)
            .unwrap_err()
            .contains("stale or mismatched"));
        candidate.similarity = prior_similarity;
        store
            .upsert_json(SUGGESTION_TABLE, &candidate_id, &candidate)
            .unwrap();

        let mut face: FaceObservation = store
            .require(FACE_TABLE, "stale-preview-face", "Face")
            .unwrap();
        face.face_revision += 1;
        face.updated_at = now();
        store
            .upsert_json(FACE_TABLE, "stale-preview-face", &face)
            .unwrap();
        assert!(store
            .apply_batch_correction(&preview)
            .unwrap_err()
            .contains("stale correction revision fence"));
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "stale-preview-face")
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn batch_delta_accounting_surfaces_exact_over_cap_without_truncation() {
        let rows = (0..=CORRECTION_ROW_LIMIT)
            .map(|index| CorrectionRowDelta {
                table: CorrectionTable::Disposition,
                stable_id: format!("over-cap-{index:05}"),
                before: None,
                after: Some(json!({"index": index})),
            })
            .collect::<Vec<_>>();
        let counts = batch_delta_counts(&rows).unwrap();
        assert_eq!(counts.dispositions, CORRECTION_ROW_LIMIT + 1);
        assert_eq!(counts.checked_total().unwrap(), CORRECTION_ROW_LIMIT + 1);
        assert!(ensure_within_delta_limit(counts.checked_total().unwrap())
            .unwrap_err()
            .contains("4096-row"));
    }

    #[test]
    fn batch_change_is_atomic_preserves_unaffected_rows_and_undoes_as_one_operation() {
        let root = workspace("batch-change");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Batch Source", Vec::new()).unwrap();
        let target = store.create_person("Batch Target", Vec::new()).unwrap();
        let other = store.create_person("Batch Other", Vec::new()).unwrap();
        for (face_id, media_key, person) in [
            ("batch-a", "media/batch-a.jpg", &source),
            ("batch-b", "media/batch-b.jpg", &source),
            ("batch-c", "media/batch-c.jpg", &other),
        ] {
            store.create_face(manual_face(face_id, media_key)).unwrap();
            confirm(&store, face_id, person);
        }
        let unaffected_before: Assignment = store
            .require(ASSIGNMENT_TABLE, "batch-c", "Assignment")
            .unwrap();
        let face_ids = vec!["batch-a".to_string(), "batch-b".to_string()];
        let fences = face_ids
            .iter()
            .map(|face_id| {
                store
                    .correction_fence_for(
                        face_id,
                        vec![source.person_id.clone(), target.person_id.clone()],
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::ChangePerson,
                Some(&source.person_id),
                Some(&target.person_id),
                face_ids.clone(),
                fences,
            )
            .unwrap();
        assert_eq!(
            preview.required_reversible_rows,
            preview.delta_counts.checked_total().unwrap()
        );
        let receipt = store.apply_batch_correction(&preview).unwrap();
        for face_id in &face_ids {
            let assignment: Assignment = store
                .require(ASSIGNMENT_TABLE, face_id, "Assignment")
                .unwrap();
            assert_eq!(assignment.person_id, target.person_id);
            assert_eq!(assignment.placement, "unsorted");
            assert_eq!(assignment.operation_id, receipt.operation_id);
        }
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "batch-c", "Assignment")
                .unwrap(),
            unaffected_before
        );
        store.undo_correction(&receipt.operation_id).unwrap();
        for face_id in &face_ids {
            assert_eq!(
                store
                    .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "Assignment")
                    .unwrap()
                    .person_id,
                source.person_id
            );
        }
        close(&root, store);
    }

    #[test]
    fn batch_member_mismatch_rolls_back_every_preceding_member() {
        let root = workspace("batch-rollback");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Rollback Source", Vec::new()).unwrap();
        let target = store.create_person("Rollback Target", Vec::new()).unwrap();
        let other = store.create_person("Rollback Other", Vec::new()).unwrap();
        store
            .create_face(manual_face("rollback-a", "media/rollback-a.jpg"))
            .unwrap();
        store
            .create_face(manual_face("rollback-b", "media/rollback-b.jpg"))
            .unwrap();
        confirm(&store, "rollback-a", &source);
        confirm(&store, "rollback-b", &other);
        let face_ids = vec!["rollback-a".to_string(), "rollback-b".to_string()];
        let fences = face_ids
            .iter()
            .map(|face_id| {
                store
                    .correction_fence_for(
                        face_id,
                        vec![source.person_id.clone(), target.person_id.clone()],
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(store
            .batch_correction_preflight(
                BatchCorrectionAction::ChangePerson,
                Some(&source.person_id),
                Some(&target.person_id),
                face_ids,
                fences,
            )
            .unwrap_err()
            .contains("no longer belongs"));
        assert_eq!(
            store
                .require::<Assignment>(ASSIGNMENT_TABLE, "rollback-a", "Assignment")
                .unwrap()
                .person_id,
            source.person_id
        );
        assert!(store
            .get_one::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &cannot_link_id("rollback-a", &source.person_id),
            )
            .unwrap()
            .is_none());
        close(&root, store);
    }

    #[test]
    fn batch_different_and_remove_assignments_are_single_undoable_operations() {
        let root = workspace("batch-other-actions");
        let store = MatchStore::open(&root).unwrap();
        let source = store.create_person("Batch Review", Vec::new()).unwrap();
        for face_id in ["review-a", "review-b"] {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            strict_assignment_without_revision_bump(&store, face_id, &source);
        }
        let face_ids = vec!["review-a".to_string(), "review-b".to_string()];
        let fences = face_ids
            .iter()
            .map(|face_id| {
                store
                    .correction_fence_for(face_id, vec![source.person_id.clone()])
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let different_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::Different,
                Some(&source.person_id),
                None,
                face_ids.clone(),
                fences,
            )
            .unwrap();
        let different = store.apply_batch_correction(&different_preview).unwrap();
        assert_eq!(different.affected_face_ids, face_ids);
        store.undo_correction(&different.operation_id).unwrap();

        let fences = face_ids
            .iter()
            .map(|face_id| store.correction_fence(face_id).unwrap())
            .collect::<Vec<_>>();
        let removed_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::RemoveAssignment,
                Some(&source.person_id),
                None,
                face_ids.clone(),
                fences,
            )
            .unwrap();
        let removed = store.apply_batch_correction(&removed_preview).unwrap();
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
            .unwrap()
            .is_none()));
        store.undo_correction(&removed.operation_id).unwrap();
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
            .unwrap()
            .is_some()));
        close(&root, store);
    }

    #[test]
    fn remaining_batch_verbs_validate_once_and_round_trip_as_single_operations() {
        let root = workspace("batch-remaining-verbs");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Batch Verbs", Vec::new()).unwrap();
        let face_ids = vec!["verbs-a".to_string(), "verbs-b".to_string()];
        for face_id in &face_ids {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            confirm(&store, face_id, &person);
        }
        let fences = || {
            face_ids
                .iter()
                .map(|face_id| {
                    store
                        .correction_fence_for(face_id, vec![person.person_id.clone()])
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        let already_confirmed_not_sure = store
            .batch_correction_preflight(
                BatchCorrectionAction::NotSure,
                Some(&person.person_id),
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap_err();
        assert!(already_confirmed_not_sure.contains("already operator-confirmed"));

        let already_confirmed = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap_err();
        assert!(already_confirmed.contains("already operator-confirmed"));

        let same_face_ids = vec!["same-a".to_string(), "same-b".to_string()];
        for face_id in &same_face_ids {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            suggest_for_review(&store, face_id, &person);
        }
        let same_fences = same_face_ids
            .iter()
            .map(|face_id| {
                store
                    .correction_fence_for(face_id, vec![person.person_id.clone()])
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let same_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::Same,
                Some(&person.person_id),
                None,
                same_face_ids,
                same_fences,
            )
            .unwrap();
        let same = store.apply_batch_correction(&same_preview).unwrap();
        store.undo_correction(&same.operation_id).unwrap();

        let ignore_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::IgnoreFace,
                None,
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap();
        let ignored = store.apply_batch_correction(&ignore_preview).unwrap();
        store.undo_correction(&ignored.operation_id).unwrap();
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<FaceDisposition>(FACE_DISPOSITION_TABLE, face_id)
            .unwrap()
            .is_none()));

        let not_face_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::NotAFace,
                None,
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap();
        let not_faces = store.apply_batch_correction(&not_face_preview).unwrap();
        store.undo_correction(&not_faces.operation_id).unwrap();

        let delete_preview = store
            .batch_correction_preflight(
                BatchCorrectionAction::DeleteFaceAnalysis,
                None,
                None,
                face_ids.clone(),
                fences(),
            )
            .unwrap();
        let deleted = store.apply_batch_correction(&delete_preview).unwrap();
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<FaceObservation>(FACE_TABLE, face_id)
            .unwrap()
            .is_none()));
        store.undo_correction(&deleted.operation_id).unwrap();
        assert!(face_ids.iter().all(|face_id| store
            .get_one::<FaceObservation>(FACE_TABLE, face_id)
            .unwrap()
            .is_some()));
        close(&root, store);
    }

    #[test]
    fn different_and_this_is_not_share_exact_reversible_semantics() {
        let root = workspace("different-map");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Rejected", Vec::new()).unwrap();
        for (face_id, this_is_not) in [("face-different", false), ("face-this-is-not", true)] {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            if this_is_not {
                suggest_for_review(&store, face_id, &person);
                store
                    .same_correction(
                        face_id,
                        &person.person_id,
                        &correction_for_target(&store, face_id, &person),
                    )
                    .unwrap();
            } else {
                strict_assignment_without_revision_bump(&store, face_id, &person);
            }
            let fence = correction_for_target(&store, face_id, &person);
            let receipt = if this_is_not {
                store
                    .this_is_not_correction(face_id, &person.person_id, &fence)
                    .unwrap()
            } else {
                store
                    .different_correction(face_id, &person.person_id, &fence)
                    .unwrap()
            };
            assert!(store
                .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
                .unwrap()
                .is_none());
            assert!(store
                .get_one::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &cannot_link_id(face_id, &person.person_id),
                )
                .unwrap()
                .is_some());
            store.undo_correction(&receipt.operation_id).unwrap();
            assert!(store
                .get_one::<Assignment>(ASSIGNMENT_TABLE, face_id)
                .unwrap()
                .is_some());
            assert!(store
                .get_one::<CannotLinkConstraint>(
                    CONSTRAINT_TABLE,
                    &cannot_link_id(face_id, &person.person_id),
                )
                .unwrap()
                .is_none());
        }
        close(&root, store);
    }

    #[test]
    fn person_suggestion_face_pages_reject_a_missing_canonical_face_exactly() {
        let root = workspace("missing-canonical-suggestion-face");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Missing Face", Vec::new()).unwrap();
        let face = manual_face(
            "face-missing-from-suggestion-page",
            "media/missing-face.jpg",
        );
        store.create_face(face.clone()).unwrap();
        let suggestion = Suggestion {
            suggestion_id: suggestion_id(&face.face_id, &person.person_id),
            face_id: face.face_id.clone(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.91,
            model_generation: UNCONFIGURED_MODEL_GENERATION.to_string(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            person_revision: person.revision,
            job_id: "missing-face-suggestion-seed".to_string(),
            created_at: now(),
        };
        let suggestion_value = serde_json::to_value(&suggestion).unwrap();
        store
            .transactional_upserts_deletes(
                &[((
                    SUGGESTION_TABLE,
                    suggestion.suggestion_id.as_str(),
                    suggestion_value,
                ))],
                &[],
            )
            .unwrap();
        store
            .transactional_upserts_deletes(&[], &[(FACE_TABLE, face.face_id.as_str())])
            .unwrap();

        let error = store.preview_remove_person(&person.person_id).unwrap_err();
        assert!(error.contains("references 1 missing FaceObservation row(s)"));
        assert!(error.contains(&face.face_id));
        assert_eq!(store.count(SUGGESTION_TABLE).unwrap(), 1);
        assert_eq!(store.count(FACE_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn rejection_verbs_enforce_exact_single_and_batch_trigger_context_without_mutation() {
        let root = workspace("rejection-trigger-context");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Trigger Context", Vec::new()).unwrap();
        let confirmed_faces = ["confirmed-a", "confirmed-b"];
        for face_id in confirmed_faces {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            confirm(&store, face_id, &person);
        }
        let suggestion_faces = ["suggestion-a", "suggestion-b"];
        let mut suggestions_before = Vec::new();
        for face_id in suggestion_faces {
            store
                .create_face(manual_face(face_id, &format!("media/{face_id}.jpg")))
                .unwrap();
            suggestions_before.push(suggest_for_review(&store, face_id, &person));
        }
        let assignments_before = confirmed_faces
            .iter()
            .map(|face_id| {
                store
                    .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "Assignment")
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let execution_before = store.execution_state().unwrap();
        let operations_before = store.count(OPERATION_TABLE).unwrap();
        let constraints_before = store.count(CONSTRAINT_TABLE).unwrap();

        let different_error = store
            .different_correction(
                confirmed_faces[0],
                &person.person_id,
                &correction_for_target(&store, confirmed_faces[0], &person),
            )
            .unwrap_err();
        assert!(different_error.contains(
            "Different requires a matching current suggestion or committed_strict_automatic assignment"
        ));
        let confirmed_ids = confirmed_faces.map(str::to_string).to_vec();
        let confirmed_fences = confirmed_ids
            .iter()
            .map(|face_id| correction_for_target(&store, face_id, &person))
            .collect::<Vec<_>>();
        let batch_different_error = store
            .batch_correction_preflight(
                BatchCorrectionAction::Different,
                Some(&person.person_id),
                None,
                confirmed_ids,
                confirmed_fences,
            )
            .unwrap_err();
        assert!(batch_different_error.contains(
            "Different requires a matching current suggestion or committed_strict_automatic assignment"
        ));

        let this_is_not_error = store
            .this_is_not_correction(
                suggestion_faces[0],
                &person.person_id,
                &correction_for_target(&store, suggestion_faces[0], &person),
            )
            .unwrap_err();
        assert!(this_is_not_error.contains("This-is-not requires a matching committed assignment"));
        let suggestion_ids = suggestion_faces.map(str::to_string).to_vec();
        let suggestion_fences = suggestion_ids
            .iter()
            .map(|face_id| correction_for_target(&store, face_id, &person))
            .collect::<Vec<_>>();
        let batch_this_is_not_error = store
            .batch_correction_preflight(
                BatchCorrectionAction::ThisIsNot,
                Some(&person.person_id),
                None,
                suggestion_ids,
                suggestion_fences,
            )
            .unwrap_err();
        assert!(batch_this_is_not_error
            .contains("This-is-not requires a matching committed assignment"));

        assert_eq!(store.execution_state().unwrap(), execution_before);
        assert_eq!(store.count(OPERATION_TABLE).unwrap(), operations_before);
        assert_eq!(store.count(CONSTRAINT_TABLE).unwrap(), constraints_before);
        for (face_id, expected) in confirmed_faces.iter().zip(assignments_before) {
            assert_eq!(
                store
                    .require::<Assignment>(ASSIGNMENT_TABLE, face_id, "Assignment")
                    .unwrap(),
                expected
            );
        }
        for expected in suggestions_before {
            assert_eq!(
                store
                    .require::<Suggestion>(SUGGESTION_TABLE, &expected.suggestion_id, "Suggestion",)
                    .unwrap(),
                expected
            );
        }
        close(&root, store);
    }
}
