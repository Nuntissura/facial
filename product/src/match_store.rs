//! Durable Match identity graph, regenerable vector index, resumable job state,
//! revision-fenced projections, and bounded resource admission (WP-081).
//!
//! This module writes typed records directly into the existing embedded
//! SurrealDB application root. Face vectors intentionally never pass through
//! `surreal_kv`, whose hexadecimal value encoding is only a compatibility
//! surface for older media metadata.

mod clustering;
mod context;
mod corrections;
mod database;
mod exchange;
mod recovery;
mod search;
mod video;
mod worker;
pub use clustering::*;
pub use context::*;
pub use corrections::*;
pub use exchange::*;
pub use recovery::*;
pub use search::*;
pub use video::*;
pub use worker::*;

use crate::{
    identity::{
        strict_hnsw_ef_search, strict_runtime_configuration_digest, CalibrationVerification,
        EMBEDDING_DIM, STRICT_ANN_BUILD_ORDER, STRICT_ANN_ENGINE_VERSION,
        STRICT_HNSW_EF_CONSTRUCTION, STRICT_HNSW_M,
    },
    media_db::MediaDb,
    media_io::{IoPermit, MediaIoCoordinator, PermitOutcome, RootIdentity, WorkClass},
    surreal_store,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};
use surrealdb::types::SurrealValue;

const MATCH_SCHEMA_VERSION: u64 = 22;
const LEGACY_OPERATION_QUERY_PAGE_LIMIT: usize = 256;
const LEGACY_DIRECT_MEDIA_KINDS: &[&str] = &[
    "assign_operator_confirmed",
    "assign_committed_strict_automatic",
    "different",
    "not_sure",
    "move_to_look",
];
const MATCH_SCHEMA_GENERATION: &str = "match-schema-v2";
const EMBEDDING_INDEX: &str = "match_face_embedding_hnsw_v1";
const MAX_TEXT_BYTES: usize = 1024;
const TRUSTED_MIN_QUALITY: f32 = 0.70;
const TRUSTED_POLICY_VERSION: &str = "trusted-reference-v1";
const PROJECTION_CACHE_CAPACITY: usize = 4096;
const FAILURE_CODES: &[&str] = &[
    "safe_unit_timeout",
    "worker_failed",
    "worker_protocol",
    "stale_worker_result",
    "align",
    "cancelled",
    "decode",
    "detect",
    "embed",
    "internal",
    "io",
    "model_unavailable",
    "no_face",
    "persist",
    "resource_pressure",
    "suggest",
    "unsupported_media",
];

const PERSON_TABLE: &str = "match_person";
const LOOK_TABLE: &str = "match_look";
const TEMPLATE_SET_TABLE: &str = "match_trusted_template_set";
const TRUSTED_MEMBER_TABLE: &str = "match_trusted_member";
const FACE_TABLE: &str = "match_face_observation";
const EMBEDDING_TABLE: &str = "match_face_embedding";
const TRUSTED_SEARCH_TABLE: &str = "match_trusted_search_embedding";
const CALIBRATION_TABLE: &str = "match_calibration_activation";
const CALIBRATION_SPENT_TABLE: &str = "match_calibration_spent_set";
const TRUSTED_INDEX_BUILD_TABLE: &str = "match_trusted_index_build";
const ASSIGNMENT_TABLE: &str = "match_assignment";
const SUGGESTION_TABLE: &str = "match_suggestion";
const FACE_DISPOSITION_TABLE: &str = "match_face_disposition";
const CONSTRAINT_TABLE: &str = "match_constraint";
const OPERATION_TABLE: &str = "match_operation";
const JOB_TABLE: &str = "match_index_job";
const JOB_ASSET_TABLE: &str = "match_job_asset";
const EXECUTION_TABLE: &str = "match_execution";
const GENERATION_TABLE: &str = "match_model_generation";
const PROJECTION_TABLE: &str = "match_people_projection";
const ROOT_CONFIG_TABLE: &str = "match_index_root";

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct Person {
    pub person_id: String,
    pub name: String,
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_media_key: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub favorite: bool,
    pub revision: u64,
    pub catalog_revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct MatchIndexRoot {
    pub root_id: String,
    pub path: String,
    pub exclusions: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonCatalogRow {
    pub person: Person,
    pub assigned_face_count: u64,
    pub suggestion_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_source_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MatchCatalogSnapshot {
    pub total_people: u64,
    pub evidence: MatchCatalogEvidence,
    pub offset: usize,
    pub limit: usize,
    pub indexing_started: bool,
    pub partial: bool,
    pub settled: bool,
    pub rows: Vec<PersonCatalogRow>,
}

/// One canonical observation under the store read lock, not an interval fence.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MatchCatalogEvidence {
    pub total_people: u64,
    pub count_scope: &'static str,
    pub catalog_revision: u64,
    pub schema_generation: &'static str,
    pub store_session_id: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonGallerySnapshot {
    pub person: Person,
    pub total_media: usize,
    pub offset: usize,
    pub limit: usize,
    pub media_keys: Vec<String>,
    /// Canonical source paths resolved from durable job assets. The shared
    /// Media viewport opens these paths without treating normalized keys as
    /// filesystem paths.
    pub media_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct Look {
    pub look_id: String,
    pub person_id: String,
    pub name: String,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct TrustedTemplateSet {
    pub set_id: String,
    pub look_id: String,
    pub name: String,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct FaceObservation {
    pub face_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub source_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exif_orientation: Option<u8>,
    pub bounds_normalized: Vec<f32>,
    pub landmarks_normalized: Vec<Vec<f32>>,
    pub alignment_valid: bool,
    pub quality: f32,
    pub pose_bucket: String,
    pub operator_owned: bool,
    pub schema_generation: String,
    pub face_revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct FaceEmbedding {
    pub embedding_id: String,
    pub face_id: String,
    pub vector: Vec<f32>,
    pub model_generation: String,
    pub schema_generation: String,
    pub media_fingerprint: String,
    pub face_revision: u64,
    pub job_id: String,
    pub active: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, SurrealValue)]
pub(crate) struct FaceEmbeddingProvenance {
    embedding_id: String,
    face_id: String,
    model_generation: String,
    schema_generation: String,
    media_fingerprint: String,
    face_revision: u64,
    job_id: String,
    active: bool,
    created_at: String,
}

impl From<&FaceEmbedding> for FaceEmbeddingProvenance {
    fn from(embedding: &FaceEmbedding) -> Self {
        Self {
            embedding_id: embedding.embedding_id.clone(),
            face_id: embedding.face_id.clone(),
            model_generation: embedding.model_generation.clone(),
            schema_generation: embedding.schema_generation.clone(),
            media_fingerprint: embedding.media_fingerprint.clone(),
            face_revision: embedding.face_revision,
            job_id: embedding.job_id.clone(),
            active: embedding.active,
            created_at: embedding.created_at.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentState {
    Suggestion,
    CommittedStrictAutomatic,
    OperatorConfirmed,
}

impl AssignmentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Suggestion => "suggestion",
            Self::CommittedStrictAutomatic => "committed_strict_automatic",
            Self::OperatorConfirmed => "operator_confirmed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct Assignment {
    pub assignment_id: String,
    pub face_id: String,
    pub person_id: String,
    pub media_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub look_id: Option<String>,
    pub placement: String,
    pub state: String,
    pub provenance: String,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_hash: Option<String>,
    pub face_revision: u64,
    pub person_revision: u64,
    pub operation_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct Suggestion {
    pub suggestion_id: String,
    pub face_id: String,
    pub candidate_person_id: String,
    pub similarity: f32,
    pub model_generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_hash: Option<String>,
    pub media_fingerprint: String,
    pub face_revision: u64,
    pub person_revision: u64,
    pub job_id: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct CalibrationActivation {
    calibration_generation: String,
    model_generation: String,
    envelope_hash: String,
    runtime_configuration_digest: String,
    activation_integrity_digest: String,
    trusted_index_build_digest: String,
    gallery_members_digest: String,
    verifier_artifact_id: String,
    contract_sha256: String,
    raw_records_sha256: String,
    evidence_digest: String,
    review_digest: String,
    automatic_threshold: f32,
    suggestion_threshold: f32,
    runner_up_margin: f32,
    minimum_quality: f32,
    candidate_k: usize,
    rerank_k: usize,
    people_max: usize,
    looks_per_person_max: usize,
    templates_per_look_max: usize,
    total_templates_max: usize,
    verifier_verdict: String,
    independent_review_verdict: String,
    wp084_runtime_ready: bool,
    wp087_release_ready: bool,
    active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invalidation_reason: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
struct TrustedIndexBuildReceipt {
    receipt_id: String,
    build_digest: String,
    source_row_count: usize,
    engine_version: String,
    build_seed: u64,
    build_order: String,
    created_at: String,
}

#[derive(Serialize)]
struct CalibrationActivationIntegrity<'a> {
    calibration_generation: &'a str,
    model_generation: &'a str,
    envelope_hash: &'a str,
    runtime_configuration_digest: &'a str,
    trusted_index_build_digest: &'a str,
    gallery_members_digest: &'a str,
    verifier_artifact_id: &'a str,
    contract_sha256: &'a str,
    raw_records_sha256: &'a str,
    evidence_digest: &'a str,
    review_digest: &'a str,
    automatic_threshold_bits: u32,
    suggestion_threshold_bits: u32,
    runner_up_margin_bits: u32,
    minimum_quality_bits: u32,
    candidate_k: usize,
    rerank_k: usize,
    people_max: usize,
    looks_per_person_max: usize,
    templates_per_look_max: usize,
    total_templates_max: usize,
    verifier_verdict: &'a str,
    independent_review_verdict: &'a str,
    wp084_runtime_ready: bool,
    wp087_release_ready: bool,
    active: bool,
    invalidation_reason: &'a Option<String>,
    created_at: &'a str,
}

fn calibration_activation_integrity_digest(activation: &CalibrationActivation) -> String {
    use sha2::{Digest, Sha256};
    let payload = CalibrationActivationIntegrity {
        calibration_generation: &activation.calibration_generation,
        model_generation: &activation.model_generation,
        envelope_hash: &activation.envelope_hash,
        runtime_configuration_digest: &activation.runtime_configuration_digest,
        trusted_index_build_digest: &activation.trusted_index_build_digest,
        gallery_members_digest: &activation.gallery_members_digest,
        verifier_artifact_id: &activation.verifier_artifact_id,
        contract_sha256: &activation.contract_sha256,
        raw_records_sha256: &activation.raw_records_sha256,
        evidence_digest: &activation.evidence_digest,
        review_digest: &activation.review_digest,
        automatic_threshold_bits: activation.automatic_threshold.to_bits(),
        suggestion_threshold_bits: activation.suggestion_threshold.to_bits(),
        runner_up_margin_bits: activation.runner_up_margin.to_bits(),
        minimum_quality_bits: activation.minimum_quality.to_bits(),
        candidate_k: activation.candidate_k,
        rerank_k: activation.rerank_k,
        people_max: activation.people_max,
        looks_per_person_max: activation.looks_per_person_max,
        templates_per_look_max: activation.templates_per_look_max,
        total_templates_max: activation.total_templates_max,
        verifier_verdict: &activation.verifier_verdict,
        independent_review_verdict: &activation.independent_review_verdict,
        wp084_runtime_ready: activation.wp084_runtime_ready,
        wp087_release_ready: activation.wp087_release_ready,
        active: activation.active,
        invalidation_reason: &activation.invalidation_reason,
        created_at: &activation.created_at,
    };
    let bytes = serde_json::to_vec(&payload).expect("closed activation integrity serializes");
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
struct TrustedSearchEmbedding {
    membership_id: String,
    person_id: String,
    look_id: String,
    face_id: String,
    embedding_id: String,
    vector: Vec<f32>,
    model_generation: String,
    created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
struct CalibrationSpentSet {
    activation_run_id: String,
    fixture_manifest_sha256: String,
    evidence_digest: String,
    person_hashes: Vec<String>,
    acquisition_cluster_hashes: Vec<String>,
    created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct CannotLinkConstraint {
    pub constraint_id: String,
    pub face_id: String,
    pub person_id: String,
    pub operation_id: String,
    pub operator_owned: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct MatchOperation {
    pub operation_id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub face_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    pub before_json: String,
    pub after_json: String,
    pub reversible: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq)]
pub struct TrustedTemplateMembership {
    pub membership_id: String,
    pub set_id: String,
    pub look_id: String,
    pub face_id: String,
    pub authorized: bool,
    pub alignment_valid: bool,
    pub quality_passed: bool,
    pub pose_passed: bool,
    pub diversity_passed: bool,
    pub provenance: String,
    pub model_generation: String,
    pub embedding_id: String,
    pub media_fingerprint: String,
    pub face_revision: u64,
    pub quality_score: f32,
    pub quality_threshold: f32,
    pub pose_bucket: String,
    pub policy_version: String,
    pub operation_id: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrustedEligibility {
    pub provenance: String,
    pub model_generation: String,
    pub embedding_id: String,
    pub policy_version: String,
}

impl TrustedEligibility {
    fn is_declared(&self) -> bool {
        !self.provenance.trim().is_empty()
            && !self.model_generation.trim().is_empty()
            && !self.embedding_id.trim().is_empty()
            && self.policy_version == TRUSTED_POLICY_VERSION
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredMode {
    Running,
    OperatorPaused,
}

impl DesiredMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::OperatorPaused => "operator_paused",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(Self::Running),
            "operator_paused" => Ok(Self::OperatorPaused),
            other => Err(format!("unknown Match desired mode {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobLifecycle {
    Queued,
    Running,
    Pausing,
    Paused,
    Blocked,
    Cancelled,
    Completed,
    Failed,
    Partial,
    Retrying,
}

impl JobLifecycle {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Partial => "partial",
            Self::Retrying => "retrying",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "pausing" => Ok(Self::Pausing),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            "cancelled" => Ok(Self::Cancelled),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "partial" => Ok(Self::Partial),
            "retrying" => Ok(Self::Retrying),
            other => Err(format!("unknown Match job lifecycle {other}")),
        }
    }

    pub fn is_runnable(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Retrying)
    }

    pub fn is_terminal_or_failed(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed | Self::Failed)
    }

    fn accepts_inflight_result(self) -> bool {
        matches!(self, Self::Running | Self::Pausing | Self::Retrying)
    }

    fn accepts_discovery_result(self) -> bool {
        self.is_runnable() || self == Self::Pausing
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    ImmersiveFullscreen,
    ViewerPlayback,
    ResourcePressure,
    PowerSaver,
    DatabaseOwnerQuarantined,
}

impl HoldReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ImmersiveFullscreen => "immersive_fullscreen",
            Self::ViewerPlayback => "viewer_playback",
            Self::ResourcePressure => "resource_pressure",
            Self::PowerSaver => "power_saver",
            Self::DatabaseOwnerQuarantined => "database_owner_quarantined",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum JobStage {
    Discover,
    Detect,
    Align,
    Embed,
    Persist,
    Suggest,
    Complete,
}

impl JobStage {
    pub const ORDERED: [Self; 7] = [
        Self::Discover,
        Self::Detect,
        Self::Align,
        Self::Embed,
        Self::Persist,
        Self::Suggest,
        Self::Complete,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Detect => "detect",
            Self::Align => "align",
            Self::Embed => "embed",
            Self::Persist => "persist",
            Self::Suggest => "suggest",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        Self::ORDERED
            .into_iter()
            .find(|stage| stage.as_str() == value)
            .ok_or_else(|| format!("unknown Match job stage {value}"))
    }

    fn next(self) -> Self {
        match self {
            Self::Discover => Self::Detect,
            Self::Detect => Self::Align,
            Self::Align => Self::Embed,
            Self::Embed => Self::Persist,
            Self::Persist => Self::Suggest,
            Self::Suggest | Self::Complete => Self::Complete,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct IndexJob {
    pub job_id: String,
    pub root_key: String,
    pub lifecycle: String,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub discovered: u64,
    pub completed: u64,
    pub failed: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl IndexJob {
    pub fn lifecycle(&self) -> Result<JobLifecycle, String> {
        JobLifecycle::parse(&self.lifecycle)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct JobAsset {
    pub asset_id: String,
    pub job_id: String,
    pub media_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub media_fingerprint: String,
    pub next_stage: String,
    pub completed_stages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_message: Option<String>,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub updated_at: String,
}

impl JobAsset {
    pub fn next_stage(&self) -> Result<JobStage, String> {
        JobStage::parse(&self.next_stage)
    }
}

// Every consumer which resolves a media key to one historical JobAsset uses
// the v11 `match_job_asset_media` index and this ordering: newest `updated_at`
// first, then lexicographically smallest `asset_id`. The secondary key is the
// deterministic authority tie-break when two jobs settle in the same clock
// tick; changing it in only one Viewer/gallery/manual path would let the same
// media key resolve to different source truth.
const CANONICAL_JOB_ASSET_TIE_BREAK: &str = "updated_at DESC, asset_id ASC";

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
struct PersistedExecutionState {
    desired_mode: String,
    revision: u64,
    identity_revision: u64,
    catalog_revision: u64,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct ModelGeneration {
    pub generation: String,
    pub state: String,
    pub validated: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
pub struct PeopleProjection {
    pub media_key: String,
    pub media_fingerprint: String,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub person_ids: Vec<String>,
    pub published_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionFence {
    pub job_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub schema_generation: String,
    pub model_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorMutationFence {
    pub face_id: String,
    pub person_id: String,
    pub face_revision: u64,
    pub person_revision: u64,
    pub assignment_operation_id: Option<String>,
    pub identity_revision: u64,
    pub catalog_revision: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Neighbor {
    pub embedding_id: String,
    pub face_id: String,
    pub exact_cosine: f32,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NeighborQuery {
    pub neighbors: Vec<Neighbor>,
    pub plan_uses_hnsw: bool,
    pub plan: Value,
    pub candidate_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TrustedNeighbor {
    pub membership_id: String,
    pub person_id: String,
    pub look_id: String,
    pub face_id: String,
    pub embedding_id: String,
    pub exact_cosine: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrustedNeighborQuery {
    pub neighbors: Vec<TrustedNeighbor>,
    pub plan_uses_hnsw: bool,
    pub plan: Value,
    pub candidate_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StrictRecognitionOutcome {
    pub state: String,
    pub person_id: Option<String>,
    pub winning_look_id: Option<String>,
    pub winning_face_id: Option<String>,
    pub similarity: Option<f32>,
    pub runner_up_margin: Option<f32>,
    pub calibration_generation: String,
    pub envelope_hash: String,
}

// Provisional ceilings: real model/decode peaks must pass before acceptance.
pub(crate) const WORKER_MEMORY_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const WORKER_MEMORY_BUDGET_BYTES: u64 = 2 * WORKER_MEMORY_LIMIT_BYTES;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct ResourceBudget {
    pub admitted_items: u64,
    pub queued_items: u64,
    pub queued_bytes: u64,
    pub cpu_inference: u64,
    pub decoded_bytes: u64,
    pub gpu_vram_bytes: u64,
    pub worker_memory_bytes: u64,
    pub surreal_writes: u64,
    pub vector_index_builds: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            admitted_items: 16,
            queued_items: 512,
            queued_bytes: 256 * 1024 * 1024,
            cpu_inference: 2,
            decoded_bytes: 512 * 1024 * 1024,
            gpu_vram_bytes: 2 * 1024 * 1024 * 1024,
            worker_memory_bytes: WORKER_MEMORY_BUDGET_BYTES,
            surreal_writes: 2,
            vector_index_builds: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ResourceRequest {
    pub admitted_items: u64,
    pub queued_items: u64,
    pub queued_bytes: u64,
    pub cpu_inference: u64,
    pub decoded_bytes: u64,
    pub gpu_vram_bytes: u64,
    pub worker_memory_bytes: u64,
    pub surreal_writes: u64,
    pub vector_index_builds: u64,
}

pub type ResourceUsage = ResourceRequest;

/// Fixed-size lifetime accounting, updated under the admission lock. Peaks
/// include warmup and cannot be lost between diagnostic polls.
#[derive(Clone, Debug, Serialize)]
pub struct ResourceTelemetry {
    pub lifetime_id: String,
    pub scope: &'static str,
    pub current_usage: ResourceUsage,
    pub peak_usage: ResourceUsage,
    pub acquisitions: u64,
    pub replacements: u64,
    pub releases: u64,
    pub preparation_releases: u64,
    pub pressure_events: u64,
    pub overflow: bool,
    #[serde(skip)]
    interval: ResourceInterval,
    #[serde(skip)]
    activity: crate::runtime_evidence::EvidenceRing<LeaseActivity>,
    #[serde(skip)]
    stage_leases: [u64; 7],
    #[serde(skip)]
    unclassified_leases: u64,
}

#[derive(Clone, Debug, Serialize)]
struct ResourceInterval {
    scope: &'static str,
    runtime_id: String,
    lifetime_id: String,
    sequence: u64,
    start_us: u64,
    end_us: u64,
    opening_usage: ResourceUsage,
    closing_usage: ResourceUsage,
    opening_live_stage_leases: [u64; 7],
    closing_live_stage_leases: [u64; 7],
    opening_unclassified_leases: u64,
    closing_unclassified_leases: u64,
    peak_usage: ResourceUsage,
    acquisitions: u64,
    replacements: u64,
    releases: u64,
    preparation_releases: u64,
    pressure_events: u64,
    overflow: bool,
}

impl ResourceInterval {
    fn new(
        lifetime_id: String,
        sequence: u64,
        start_us: u64,
        opening_usage: ResourceUsage,
        stages: [u64; 7],
        unclassified: u64,
    ) -> Self {
        Self {
            scope: "governor_interval_between_dedicated_runtime_diagnostics",
            runtime_id: crate::runtime_evidence::clock().id.clone(),
            lifetime_id,
            sequence,
            start_us,
            end_us: start_us,
            opening_usage,
            closing_usage: opening_usage,
            opening_live_stage_leases: stages,
            closing_live_stage_leases: stages,
            opening_unclassified_leases: unclassified,
            closing_unclassified_leases: unclassified,
            peak_usage: opening_usage,
            acquisitions: 0,
            replacements: 0,
            releases: 0,
            preparation_releases: 0,
            pressure_events: 0,
            overflow: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct LeaseActivity {
    event: &'static str,
    usage: ResourceUsage,
    live_stage_leases: [u64; 7],
    unclassified_leases: u64,
}

fn raise_resource_peak(peak: &mut ResourceUsage, next: ResourceUsage) {
    peak.admitted_items = peak.admitted_items.max(next.admitted_items);
    peak.queued_items = peak.queued_items.max(next.queued_items);
    peak.queued_bytes = peak.queued_bytes.max(next.queued_bytes);
    peak.cpu_inference = peak.cpu_inference.max(next.cpu_inference);
    peak.decoded_bytes = peak.decoded_bytes.max(next.decoded_bytes);
    peak.gpu_vram_bytes = peak.gpu_vram_bytes.max(next.gpu_vram_bytes);
    peak.worker_memory_bytes = peak.worker_memory_bytes.max(next.worker_memory_bytes);
    peak.surreal_writes = peak.surreal_writes.max(next.surreal_writes);
    peak.vector_index_builds = peak.vector_index_builds.max(next.vector_index_builds);
}

#[derive(Clone, Copy)]
enum ResourceEvent {
    Acquire,
    Replace,
    Release,
    PreparationRelease,
    Pressure,
}

impl ResourceTelemetry {
    fn new() -> Self {
        let lifetime_id = uuid::Uuid::new_v4().to_string();
        let _ = crate::runtime_evidence::clock();
        let started = crate::runtime_evidence::timestamp(std::time::Instant::now());
        Self {
            interval: ResourceInterval::new(
                lifetime_id.clone(),
                1,
                started.unwrap_or(0),
                ResourceUsage::default(),
                [0; 7],
                0,
            ),
            activity: crate::runtime_evidence::EvidenceRing::new(
                "admitted_resource_leases_excluding_kernel_execution",
            ),
            stage_leases: [0; 7],
            unclassified_leases: 0,
            lifetime_id,
            scope: "governor_lifetime_including_warmup",
            current_usage: ResourceUsage::default(),
            peak_usage: ResourceUsage::default(),
            acquisitions: 0,
            replacements: 0,
            releases: 0,
            preparation_releases: 0,
            pressure_events: 0,
            overflow: started.is_none(),
        }
    }

    fn record(&mut self, event: ResourceEvent) {
        let counter = match event {
            ResourceEvent::Acquire => &mut self.acquisitions,
            ResourceEvent::Replace => &mut self.replacements,
            ResourceEvent::Release => &mut self.releases,
            ResourceEvent::PreparationRelease => &mut self.preparation_releases,
            ResourceEvent::Pressure => &mut self.pressure_events,
        };
        if let Some(next) = counter.checked_add(1) {
            *counter = next;
        } else {
            self.overflow = true;
        }
        let (counter, name) = match event {
            ResourceEvent::Acquire => (&mut self.interval.acquisitions, "acquire"),
            ResourceEvent::Replace => (&mut self.interval.replacements, "replace"),
            ResourceEvent::Release => (&mut self.interval.releases, "release"),
            ResourceEvent::PreparationRelease => (
                &mut self.interval.preparation_releases,
                "preparation_release",
            ),
            ResourceEvent::Pressure => (&mut self.interval.pressure_events, "pressure"),
        };
        if let Some(next) = counter.checked_add(1) {
            *counter = next;
        } else {
            self.interval.overflow = true;
            self.overflow = true;
        }
        raise_resource_peak(&mut self.interval.peak_usage, self.current_usage);
        self.push_activity(name);
    }

    fn push_activity(&mut self, event: &'static str) {
        self.activity.push(
            LeaseActivity {
                event,
                usage: self.current_usage,
                live_stage_leases: self.stage_leases,
                unclassified_leases: self.unclassified_leases,
            },
            std::time::Instant::now(),
        );
    }

    fn publish(&mut self, next: ResourceUsage, event: ResourceEvent) {
        self.current_usage = next;
        raise_resource_peak(&mut self.peak_usage, next);
        self.record(event);
    }
}

#[derive(Clone)]
pub struct MatchResourceGovernor {
    budget: ResourceBudget,
    usage: Arc<Mutex<ResourceTelemetry>>,
}

impl MatchResourceGovernor {
    pub fn new(budget: ResourceBudget) -> Result<Self, String> {
        if budget.admitted_items == 0
            || budget.queued_items == 0
            || budget.queued_bytes == 0
            || budget.cpu_inference == 0
            || budget.decoded_bytes == 0
            || budget.surreal_writes == 0
            || budget.vector_index_builds == 0
        {
            return Err("Match resource ceilings must be positive".to_string());
        }
        Ok(Self {
            budget,
            usage: Arc::new(Mutex::new(ResourceTelemetry::new())),
        })
    }

    pub fn try_acquire(&self, request: ResourceRequest) -> Result<MatchResourceLease, String> {
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| "Match resource governor is poisoned".to_string())?;
        let next = checked_usage(usage.current_usage, request)?;
        if exceeds(next, self.budget) {
            usage.record(ResourceEvent::Pressure);
            return Err("resource_pressure".to_string());
        }
        usage.unclassified_leases = usage
            .unclassified_leases
            .checked_add(1)
            .ok_or("Match lease count overflow")?;
        usage.publish(next, ResourceEvent::Acquire);
        Ok(MatchResourceLease {
            usage: Arc::clone(&self.usage),
            request,
            released: false,
            release_holds: None,
            stage: None,
        })
    }

    /// Atomically replace one live lease's accounting without creating an
    /// unaccounted ownership gap or temporarily double-counting the same
    /// payload. On resource pressure the original lease and aggregate usage
    /// remain unchanged.
    fn try_replace(
        &self,
        lease: &mut MatchResourceLease,
        replacement: ResourceRequest,
    ) -> Result<(), String> {
        if lease.released || !Arc::ptr_eq(&self.usage, &lease.usage) {
            return Err("Match resource lease does not belong to this governor".to_string());
        }
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| "Match resource governor is poisoned".to_string())?;
        let mut without_lease = usage.current_usage;
        subtract_usage(&mut without_lease, lease.request);
        let next = checked_usage(without_lease, replacement)?;
        if exceeds(next, self.budget) {
            usage.record(ResourceEvent::Pressure);
            return Err("resource_pressure".to_string());
        }
        usage.publish(next, ResourceEvent::Replace);
        lease.request = replacement;
        Ok(())
    }

    pub fn usage(&self) -> Result<ResourceUsage, String> {
        self.usage
            .lock()
            .map(|usage| usage.current_usage)
            .map_err(|_| "Match resource governor is poisoned".to_string())
    }

    pub fn telemetry(&self) -> Result<ResourceTelemetry, String> {
        self.usage
            .lock()
            .map(|usage| usage.clone())
            .map_err(|_| "Match resource governor is poisoned".to_string())
    }

    pub fn budget(&self) -> ResourceBudget {
        self.budget
    }

    /// Rotate only from the dedicated live-GUI diagnostics path; ordinary snapshots do not consume it.
    pub(crate) fn interval_checkpoint(&self) -> Result<Value, String> {
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| "Match resource governor is poisoned")?;
        let now = std::time::Instant::now();
        let end_us =
            crate::runtime_evidence::timestamp(now).ok_or("Match runtime clock overflow")?;
        if end_us < usage.interval.start_us {
            return Err("Match runtime clock reversed".into());
        }
        usage.interval.end_us = end_us;
        usage.interval.closing_usage = usage.current_usage;
        usage.interval.closing_live_stage_leases = usage.stage_leases;
        usage.interval.closing_unclassified_leases = usage.unclassified_leases;
        let overflow = usage.overflow;
        usage.interval.overflow |= overflow;
        let result = json!({"schema_version": 1, "runtime_id": crate::runtime_evidence::clock().id,
            "timestamp_scope": crate::runtime_evidence::TIMESTAMP_SCOPE, "captured_at_us": end_us,
            "governor_interval": usage.interval, "lease_activity": usage.activity.snapshot(now)});
        let sequence = usage
            .interval
            .sequence
            .checked_add(1)
            .ok_or("Match interval sequence overflow")?;
        usage.interval = ResourceInterval::new(
            usage.lifetime_id.clone(),
            sequence,
            end_us,
            usage.current_usage,
            usage.stage_leases,
            usage.unclassified_leases,
        );
        Ok(result)
    }
}

pub struct MatchResourceLease {
    usage: Arc<Mutex<ResourceTelemetry>>,
    request: ResourceRequest,
    released: bool,
    release_holds: Option<Arc<Mutex<BTreeSet<HoldReason>>>>,
    stage: Option<JobStage>,
}

/// Combined admission for one automatic Match stage. Both the shared
/// filesystem permit and Match's multi-axis resource lease are RAII-owned.
pub struct MatchStagePermit {
    owner: std::sync::Weak<surreal_store::Store>,
    external_holds: Arc<MatchExternalHolds>,
    admission_epoch: u64,
    io: Mutex<Option<IoPermit>>,
    resources: Mutex<Option<MatchResourceLease>>,
    store_session: String,
    fence: RevisionFence,
    stage: JobStage,
    consumed: AtomicBool,
    authorized_payload_bytes: u64,
    audit_only: bool,
    admitted_at: std::time::Instant,
}

/// Single-use authorization for one automatic discovery result write. Unlike
/// an asset-stage permit, discovery has no durable asset revision fence yet,
/// so the permit is bound to the store session and IndexJob instead.
pub struct MatchDiscoveryPermit {
    owner: std::sync::Weak<surreal_store::Store>,
    external_holds: Arc<MatchExternalHolds>,
    admission_epoch: u64,
    settling_observation: bool,
    resources: Mutex<Option<MatchResourceLease>>,
    store_session: String,
    job_id: String,
    media_key: String,
    consumed: AtomicBool,
    authorized_payload_bytes: u64,
    admitted_at: std::time::Instant,
}

/// Single-use proof that one filesystem discovery operation began while its
/// IndexJob was runnable. The operation may finish after the job enters
/// `Pausing`; its result can then acquire a bounded writer lease and settle,
/// while no new filesystem operation is admitted.
pub struct MatchDiscoveryObservation {
    store_session: String,
    job_id: String,
    consumed: AtomicBool,
}

impl Drop for MatchDiscoveryPermit {
    fn drop(&mut self) {
        if let Ok(resources) = self.resources.get_mut() {
            if let Some(owner) = self.owner.upgrade() {
                owner.retain_until_owner_exit(resources.take());
            }
        }
    }
}

impl Drop for MatchStagePermit {
    fn drop(&mut self) {
        self.release_database_leases(PermitOutcome::Cancelled);
    }
}

struct MatchDiscoveryUse<'a> {
    permit: &'a MatchDiscoveryPermit,
    _scope: surreal_store::MatchUnitScope,
}

impl Drop for MatchDiscoveryUse<'_> {
    fn drop(&mut self) {
        if let Ok(mut resources) = self.permit.resources.lock() {
            if let Some(owner) = self.permit.owner.upgrade() {
                owner.retain_until_owner_exit(resources.take());
            }
        }
    }
}

impl MatchDiscoveryPermit {
    pub(crate) fn begin_database_unit(&self) -> Result<surreal_store::MatchUnitScope, String> {
        let scope = (|| {
            if !self.settling_observation
                && (self.external_holds.blocked()
                    || self.external_holds.snapshot() != self.admission_epoch)
            {
                return Err("Match discovery admission changed before database execution".into());
            }
            self.owner
                .upgrade()
                .ok_or("Match database owner is closed")?
                .begin_match_unit(self.admitted_at + crate::match_worker::SAFE_UNIT_LIMIT)
        })();
        if scope.is_err() {
            if let Ok(mut resources) = self.resources.lock() {
                if let Some(owner) = self.owner.upgrade() {
                    owner.retain_until_owner_exit(resources.take());
                }
            }
        }
        scope
    }
    fn authorize_payload(&self, payload_bytes: usize) -> Result<(), String> {
        let resources = self
            .resources
            .lock()
            .map_err(|_| "Match discovery resource lease is poisoned".to_string())?;
        let lease = resources
            .as_ref()
            .ok_or("Match discovery resource lease was already released")?;
        if self.authorized_payload_bytes < payload_bytes as u64
            || lease.request.admitted_items == 0
            || lease.request.queued_items == 0
            || lease.request.queued_bytes == 0
            || lease.request.surreal_writes == 0
        {
            return Err("Match discovery resources do not cover the write payload".to_string());
        }
        Ok(())
    }

    fn consume_for(
        &self,
        store_session: &str,
        job_id: &str,
        media_key: &str,
    ) -> Result<MatchDiscoveryUse<'_>, String> {
        if self.store_session != store_session {
            return Err("Match discovery permit belongs to a different store session".to_string());
        }
        if self.consumed.swap(true, Ordering::AcqRel) {
            return Err("Match discovery permit was already consumed".to_string());
        }
        if self.job_id != job_id || self.media_key != media_key {
            if let Ok(mut resources) = self.resources.lock() {
                resources.take();
            }
            return Err(
                "Match discovery permit does not authorize this job and media key".to_string(),
            );
        }
        let scope = self.begin_database_unit()?;
        Ok(MatchDiscoveryUse {
            permit: self,
            _scope: scope,
        })
    }
}

struct MatchStageUse<'a> {
    permit: &'a MatchStagePermit,
    outcome: PermitOutcome,
    _scope: surreal_store::MatchUnitScope,
}

struct MatchDatabaseLeases {
    _resources: Option<MatchResourceLease>,
    io: Option<IoPermit>,
    outcome: PermitOutcome,
}

impl Drop for MatchDatabaseLeases {
    fn drop(&mut self) {
        if let Some(io) = self.io.take() {
            io.finish(self.outcome);
        }
    }
}

impl MatchStageUse<'_> {
    fn success(&mut self) {
        self.outcome = PermitOutcome::Success;
    }
}

impl Drop for MatchStageUse<'_> {
    fn drop(&mut self) {
        self.permit.release_database_leases(self.outcome);
    }
}

impl MatchStagePermit {
    fn begin_execution_scope(&self) -> Result<surreal_store::MatchUnitScope, String> {
        let scope = self.check_execution_admission().and_then(|()| {
            self.owner
                .upgrade()
                .ok_or_else(|| "Match database owner is closed".to_string())?
                .begin_match_unit(self.admitted_at + crate::match_worker::SAFE_UNIT_LIMIT)
        });
        if scope.is_err() {
            self.release_cancelled();
        }
        scope
    }

    fn check_execution_admission(&self) -> Result<(), String> {
        if self.external_holds.blocked() || self.external_holds.snapshot() != self.admission_epoch {
            return Err("Match admission changed before database execution".into());
        }
        Ok(())
    }
    fn release_database_leases(&self, outcome: PermitOutcome) {
        let resources = self
            .resources
            .lock()
            .ok()
            .and_then(|mut value| value.take());
        let io = self.io.lock().ok().and_then(|mut value| value.take());
        let leases = MatchDatabaseLeases {
            _resources: resources,
            io,
            outcome,
        };
        if let Some(owner) = self.owner.upgrade() {
            owner.retain_until_owner_exit(leases);
        }
    }

    pub fn finish(self, outcome: PermitOutcome) {
        self.release_database_leases(outcome);
    }

    fn authorize_payload(&self, payload_bytes: usize) -> Result<(), String> {
        let resources = self
            .resources
            .lock()
            .map_err(|_| "Match stage resource lease is poisoned".to_string())?;
        if self.authorized_payload_bytes < payload_bytes as u64 {
            return Err("Match stage resources do not cover the write payload".to_string());
        }
        if !self.audit_only {
            let lease = resources
                .as_ref()
                .ok_or("Match stage resource lease was already released")?;
            if lease.request.admitted_items == 0
                || lease.request.queued_items == 0
                || lease.request.surreal_writes == 0
            {
                return Err("Match stage resources do not cover the write payload".to_string());
            }
        }
        Ok(())
    }

    fn consume_for(
        &self,
        store_session: &str,
        fence: &RevisionFence,
        stage: JobStage,
    ) -> Result<MatchStageUse<'_>, String> {
        if self.store_session != store_session {
            return Err("Match stage permit belongs to a different store session".to_string());
        }
        if self.consumed.swap(true, Ordering::AcqRel) {
            return Err("Match stage permit was already consumed".to_string());
        }
        if self.fence != *fence || self.stage != stage {
            self.release_cancelled();
            return Err(
                "Match stage permit does not authorize this store/job/asset/stage".to_string(),
            );
        }
        let scope = self.begin_execution_scope()?;
        Ok(MatchStageUse {
            permit: self,
            outcome: PermitOutcome::Cancelled,
            _scope: scope,
        })
    }

    fn consume_for_any_stage(
        &self,
        store_session: &str,
        fence: &RevisionFence,
    ) -> Result<MatchStageUse<'_>, String> {
        if self.store_session != store_session {
            return Err("Match stage permit belongs to a different store session".to_string());
        }
        if self.consumed.swap(true, Ordering::AcqRel) {
            return Err("Match stage permit was already consumed".to_string());
        }
        if self.fence != *fence {
            self.release_cancelled();
            return Err("Match stage permit does not authorize this store/job/asset".to_string());
        }
        let scope = self.begin_execution_scope()?;
        Ok(MatchStageUse {
            permit: self,
            outcome: PermitOutcome::Cancelled,
            _scope: scope,
        })
    }

    fn release_cancelled(&self) {
        self.release_database_leases(PermitOutcome::Cancelled);
    }
}

impl MatchResourceLease {
    fn tag_stage(&mut self, stage: JobStage) -> Result<(), String> {
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| "Match resource governor is poisoned")?;
        let new_index = JobStage::ORDERED
            .iter()
            .position(|value| *value == stage)
            .ok_or("Match stage unknown")?;
        if self.released {
            return Err("Match stage lease already released".into());
        }
        if let Some(previous) = self.stage {
            let index = JobStage::ORDERED
                .iter()
                .position(|value| *value == previous)
                .unwrap();
            usage.stage_leases[index] = usage.stage_leases[index]
                .checked_sub(1)
                .ok_or("Match stage lease count mismatch")?;
        } else {
            usage.unclassified_leases = usage
                .unclassified_leases
                .checked_sub(1)
                .ok_or("Match unclassified lease count mismatch")?;
        }
        usage.stage_leases[new_index] = usage.stage_leases[new_index]
            .checked_add(1)
            .ok_or("Match stage lease count overflow")?;
        self.stage = Some(stage);
        usage.push_activity("stage_tagged");
        Ok(())
    }
    pub(crate) fn worker_memory_bytes(&self) -> u64 {
        self.request.worker_memory_bytes
    }
    pub(crate) fn release_worker_preparation_bytes(&mut self) -> Result<(), String> {
        if self.released || self.request.worker_memory_bytes == 0 {
            return Err("resident worker lease absent".into());
        }
        if self.request.queued_bytes == 0 {
            return Ok(());
        }
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| "Match resource governor is poisoned")?;
        usage.current_usage.queued_bytes = usage
            .current_usage
            .queued_bytes
            .checked_sub(self.request.queued_bytes)
            .ok_or("worker preparation accounting mismatch")?;
        self.request.queued_bytes = 0;
        usage.record(ResourceEvent::PreparationRelease);
        Ok(())
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        if let Ok(mut usage) = self.usage.lock() {
            subtract_usage(&mut usage.current_usage, self.request);
            let count = if let Some(stage) = self.stage {
                let index = JobStage::ORDERED
                    .iter()
                    .position(|value| *value == stage)
                    .unwrap();
                &mut usage.stage_leases[index]
            } else {
                &mut usage.unclassified_leases
            };
            if let Some(next) = count.checked_sub(1) {
                *count = next;
            } else {
                usage.overflow = true;
            }
            usage.record(ResourceEvent::Release);
        }
        if let Some(holds) = self.release_holds.take() {
            if let Ok(mut holds) = holds.lock() {
                holds.remove(&HoldReason::ResourcePressure);
            }
        }
        self.released = true;
    }
}

impl Drop for MatchResourceLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

fn checked_usage(left: ResourceUsage, right: ResourceRequest) -> Result<ResourceUsage, String> {
    Ok(ResourceUsage {
        admitted_items: left
            .admitted_items
            .checked_add(right.admitted_items)
            .ok_or("admitted item budget overflow")?,
        queued_items: left
            .queued_items
            .checked_add(right.queued_items)
            .ok_or("queued item budget overflow")?,
        queued_bytes: left
            .queued_bytes
            .checked_add(right.queued_bytes)
            .ok_or("queued byte budget overflow")?,
        cpu_inference: left
            .cpu_inference
            .checked_add(right.cpu_inference)
            .ok_or("CPU inference budget overflow")?,
        decoded_bytes: left
            .decoded_bytes
            .checked_add(right.decoded_bytes)
            .ok_or("decoded byte budget overflow")?,
        worker_memory_bytes: left
            .worker_memory_bytes
            .checked_add(right.worker_memory_bytes)
            .ok_or("worker memory budget overflow")?,
        gpu_vram_bytes: left
            .gpu_vram_bytes
            .checked_add(right.gpu_vram_bytes)
            .ok_or("GPU VRAM budget overflow")?,
        surreal_writes: left
            .surreal_writes
            .checked_add(right.surreal_writes)
            .ok_or("SurrealDB writer budget overflow")?,
        vector_index_builds: left
            .vector_index_builds
            .checked_add(right.vector_index_builds)
            .ok_or("vector index build budget overflow")?,
    })
}

fn exceeds(usage: ResourceUsage, budget: ResourceBudget) -> bool {
    usage.admitted_items > budget.admitted_items
        || usage.queued_items > budget.queued_items
        || usage.queued_bytes > budget.queued_bytes
        || usage.cpu_inference > budget.cpu_inference
        || usage.decoded_bytes > budget.decoded_bytes
        || usage.worker_memory_bytes > budget.worker_memory_bytes
        || usage.gpu_vram_bytes > budget.gpu_vram_bytes
        || usage.surreal_writes > budget.surreal_writes
        || usage.vector_index_builds > budget.vector_index_builds
}

fn validate_stage_resource_request(
    stage: JobStage,
    request: ResourceRequest,
) -> Result<(), String> {
    if request.worker_memory_bytes != 0
        || request.admitted_items == 0
        || request.queued_items == 0
        || request.queued_bytes == 0
        || request.surreal_writes == 0
    {
        return Err("Match stage requires non-zero item, byte, and write accounting".to_string());
    }
    if matches!(
        stage,
        JobStage::Detect | JobStage::Align | JobStage::Embed | JobStage::Suggest
    ) && request.cpu_inference == 0
    {
        return Err("Match inference stage requires CPU/inference accounting".to_string());
    }
    if matches!(stage, JobStage::Detect | JobStage::Align) && request.decoded_bytes == 0 {
        return Err("Match image stage requires decoded-image byte accounting".to_string());
    }
    if stage == JobStage::Embed
        && request.queued_bytes < (EMBEDDING_DIM * std::mem::size_of::<f32>()) as u64
    {
        return Err("Match embedding stage byte accounting is below vector size".to_string());
    }
    Ok(())
}

fn validate_discovery_snapshot_resource_request(request: ResourceRequest) -> Result<(), String> {
    if request.admitted_items == 0 || request.queued_items == 0 || request.queued_bytes == 0 {
        return Err(
            "Match discovery snapshot requires non-zero item and byte accounting".to_string(),
        );
    }
    if request.worker_memory_bytes != 0
        || request.cpu_inference != 0
        || request.decoded_bytes != 0
        || request.gpu_vram_bytes != 0
        || request.surreal_writes != 0
        || request.vector_index_builds != 0
    {
        return Err(
            "Match discovery snapshot cannot reserve inference, writer, or index resources"
                .to_string(),
        );
    }
    Ok(())
}

fn subtract_usage(usage: &mut ResourceUsage, request: ResourceRequest) {
    usage.worker_memory_bytes = usage
        .worker_memory_bytes
        .saturating_sub(request.worker_memory_bytes);
    usage.admitted_items = usage.admitted_items.saturating_sub(request.admitted_items);
    usage.queued_items = usage.queued_items.saturating_sub(request.queued_items);
    usage.queued_bytes = usage.queued_bytes.saturating_sub(request.queued_bytes);
    usage.cpu_inference = usage.cpu_inference.saturating_sub(request.cpu_inference);
    usage.decoded_bytes = usage.decoded_bytes.saturating_sub(request.decoded_bytes);
    usage.gpu_vram_bytes = usage.gpu_vram_bytes.saturating_sub(request.gpu_vram_bytes);
    usage.surreal_writes = usage.surreal_writes.saturating_sub(request.surreal_writes);
    usage.vector_index_builds = usage
        .vector_index_builds
        .saturating_sub(request.vector_index_builds);
}

#[derive(Clone, Debug, Default)]
struct MatchCaches {
    projections: BTreeMap<String, PeopleProjection>,
    autocomplete: AutocompleteIndex,
    identity_revision: u64,
    catalog_revision: u64,
    last_query_plan: Option<QueryPlanEvidence>,
}

fn insert_bounded_projection(
    projections: &mut BTreeMap<String, PeopleProjection>,
    projection: PeopleProjection,
) {
    projections.insert(projection.media_key.clone(), projection);
    while projections.len() > PROJECTION_CACHE_CAPACITY {
        let eviction = projections
            .values()
            .min_by(|left, right| {
                left.published_at
                    .cmp(&right.published_at)
                    .then_with(|| left.media_key.cmp(&right.media_key))
            })
            .map(|projection| projection.media_key.clone());
        let Some(eviction) = eviction else {
            break;
        };
        projections.remove(&eviction);
    }
}

#[derive(Clone, Debug, Default)]
struct AutocompleteIndex {
    /// False means the projection must be rebuilt from canonical Person rows
    /// before it can answer a query. Revision zero is a valid initial catalog
    /// revision, so validity cannot be inferred from the revision number.
    valid: bool,
    catalog_revision: u64,
    entries: Vec<AutocompleteEntry>,
}

#[derive(Clone, Debug)]
struct AutocompleteEntry {
    person_id: String,
    display_name: String,
    search: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AutocompleteResult {
    pub person_id: String,
    pub display_name: String,
    pub catalog_revision: u64,
}

#[derive(Clone, Debug, Serialize)]
struct QueryPlanEvidence {
    observed: bool,
    index: String,
    uses_hnsw: bool,
    exact_rerank: bool,
    model_generation: String,
    observed_at: String,
}

/// The low two bits are independent presentation holds; the remaining bits
/// fence changes. Publication never acquires a database or service lock.
#[derive(Default, Debug)]
pub struct MatchExternalHolds {
    state: std::sync::atomic::AtomicU64,
}

impl MatchExternalHolds {
    pub fn snapshot(&self) -> u64 {
        self.state.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_playback(&self, active: bool) -> u64 {
        self.set(1, active)
    }
    pub fn set_fullscreen(&self, active: bool) -> u64 {
        self.set(2, active)
    }
    pub fn playback(&self) -> bool {
        self.snapshot() & 1 != 0
    }
    pub fn fullscreen(&self) -> bool {
        self.snapshot() & 2 != 0
    }
    pub fn blocked(&self) -> bool {
        self.snapshot() & 3 != 0
    }

    fn set(&self, bit: u64, active: bool) -> u64 {
        use std::sync::atomic::Ordering;
        let mut prior = self.snapshot();
        loop {
            let bits = if active { prior | bit } else { prior & !bit };
            if bits == prior {
                return prior;
            }
            let next = (prior & !3).wrapping_add(4) | (bits & 3);
            match self
                .state
                .compare_exchange_weak(prior, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next,
                Err(observed) => prior = observed,
            }
        }
    }
}

#[derive(Clone)]
pub struct MatchStore {
    store: Arc<surreal_store::Store>,
    initializing: bool,
    session_id: String,
    transient_holds: Arc<Mutex<BTreeSet<HoldReason>>>,
    pending_database_failures: Arc<Mutex<Vec<Value>>>,
    pending_database_failures_evicted: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    next_checkpoint_ack_delay: Arc<Mutex<Option<std::time::Duration>>>,
    external_holds: Arc<MatchExternalHolds>,
    caches: Arc<RwLock<MatchCaches>>,
    governor: MatchResourceGovernor,
    filesystem_recovery_ready: Arc<AtomicBool>,
    #[cfg(test)]
    autocomplete_refresh_failures: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    trusted_search_reconcile_failures: Arc<std::sync::atomic::AtomicUsize>,
}

impl MatchStore {
    pub(crate) fn with_external_holds(mut self, holds: Arc<MatchExternalHolds>) -> Self {
        self.external_holds = holds;
        self
    }
    pub fn open(workspace_root: &Path) -> Result<Self, String> {
        let store = surreal_store::open(&MediaDb::db_path(workspace_root))?;
        let mut me = Self {
            store,
            initializing: true,
            session_id: new_id("match-session"),
            transient_holds: Arc::new(Mutex::new(BTreeSet::new())),
            pending_database_failures: Arc::new(Mutex::new(Vec::new())),
            pending_database_failures_evicted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            next_checkpoint_ack_delay: Arc::new(Mutex::new(None)),
            external_holds: Arc::new(MatchExternalHolds::default()),
            caches: Arc::new(RwLock::new(MatchCaches::default())),
            governor: MatchResourceGovernor::new(ResourceBudget::default())?,
            filesystem_recovery_ready: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            autocomplete_refresh_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            trusted_search_reconcile_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let startup_scope = me.store.begin_match_transaction()?;
        me.ensure_schema()?;
        me.recover_interrupted_clear()?;
        me.recover_interrupted_identity_import()?;
        me.reconcile_trusted_search()?;
        me.recover_interrupted_jobs()?;
        me.refresh_autocomplete()?;
        me.refresh_projection_cache()?;
        me.filesystem_recovery_ready.store(true, Ordering::Release);
        me.initializing = false;
        drop(startup_scope);
        Ok(me)
    }

    pub fn governor(&self) -> &MatchResourceGovernor {
        &self.governor
    }

    fn mutation_write_guard(
        &self,
        label: &str,
    ) -> Result<database::MatchMutationGuard<'_>, String> {
        let scope = self.store.begin_match_transaction()?;
        if let Some(deadline) = self.store.match_unit_deadline() {
            return self
                .persist_write_guard(deadline)
                .map(|guard| database::MatchMutationGuard::new(guard, scope));
        }
        let guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| format!("{label} lock is poisoned"))?;
        self.recover_before_mutation_unlocked(label)?;
        Ok(database::MatchMutationGuard::new(guard, scope))
    }

    fn recover_before_mutation_unlocked(&self, label: &str) -> Result<(), String> {
        if self.store.match_unit_deadline().is_some() {
            if !self.filesystem_recovery_ready.load(Ordering::Acquire) {
                return Err(format!("Match filesystem recovery is required before {label}; automatic admission is blocked"));
            }
            // Recovery files were reconciled before admission. A pending import
            // is checked canonically without doing filesystem work in this unit.
            self.require_no_pending_identity_import_unlocked()?;
            return Ok(());
        }
        self.recover_interrupted_clear_unlocked()
            .map_err(|error| format!("pending Match clear must recover before {label}: {error}"))?;
        self.recover_pending_identity_import_before_mutation_unlocked()
            .map_err(|error| {
                format!("pending Match import must recover before {label}: {error}")
            })?;
        self.reconcile_identity_import_recovery_artifacts_unlocked()
            .map_err(|error| {
                format!("Match import orphan reconciliation before {label}: {error}")
            })?;
        self.filesystem_recovery_ready
            .store(true, Ordering::Release);
        Ok(())
    }

    /// Keep lock wait, canonical recovery preflight, reads, and commit under
    /// the original admitted deadline. Engine work belongs to the shared owner.
    fn persist_write_guard(
        &self,
        deadline: std::time::Instant,
    ) -> Result<std::sync::RwLockWriteGuard<'_, ()>, String> {
        loop {
            require_persist_time_remaining(deadline)?;
            match self.store.transaction_lock().try_write() {
                Ok(guard) => {
                    require_persist_time_remaining(deadline)?;
                    self.recover_before_mutation_unlocked("Match projection publish")?;
                    require_persist_time_remaining(deadline)?;
                    return Ok(guard);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err("Match projection publish lock is poisoned".to_string());
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(
                        deadline
                            .saturating_duration_since(std::time::Instant::now())
                            .min(std::time::Duration::from_millis(2)),
                    );
                }
            }
        }
    }

    /// Wait only on a background worker. Admission is rechecked after the
    /// shared I/O permit arrives so a pause/fullscreen hold that began while
    /// queued prevents the stage from starting.
    pub fn acquire_discovery_write(
        &self,
        job_id: &str,
        media_key: &str,
        request: ResourceRequest,
    ) -> Result<MatchDiscoveryPermit, String> {
        let _database_unit = self.begin_database_unit()?;
        validate_media_key(media_key)?;
        validate_stage_resource_request(JobStage::Discover, request)?;
        let lifecycle = {
            let _guard = self.database_read_guard("Match discovery admission lock is poisoned")?;
            self.require_unlocked::<IndexJob>(JOB_TABLE, job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(lifecycle)? {
            return Err("Match discovery admission is paused, held, or terminal".to_string());
        }
        match self.governor.try_acquire(request) {
            Ok(mut resources) => {
                self.remove_hold(HoldReason::ResourcePressure)?;
                resources.release_holds = Some(Arc::clone(&self.transient_holds));
                resources.tag_stage(JobStage::Discover)?;
                Ok(MatchDiscoveryPermit {
                    owner: Arc::downgrade(&self.store),
                    external_holds: Arc::clone(&self.external_holds),
                    admission_epoch: self.external_admission_epoch(),
                    settling_observation: false,
                    admitted_at: std::time::Instant::now(),
                    resources: Mutex::new(Some(resources)),
                    store_session: self.session_id.clone(),
                    job_id: job_id.to_string(),
                    media_key: media_key.to_string(),
                    consumed: AtomicBool::new(false),
                    authorized_payload_bytes: request.queued_bytes,
                })
            }
            Err(error) => {
                self.add_hold(HoldReason::ResourcePressure)?;
                Err(error)
            }
        }
    }

    /// Admit exactly one filesystem observation immediately before the caller
    /// performs it under a shared Media-I/O permit.
    pub fn begin_discovery_observation(
        &self,
        job_id: &str,
    ) -> Result<MatchDiscoveryObservation, String> {
        let _database_unit = self.begin_database_unit()?;
        let lifecycle = {
            let _guard =
                self.database_read_guard("Match discovery observation lock is poisoned")?;
            self.require_unlocked::<IndexJob>(JOB_TABLE, job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(lifecycle)? {
            return Err("Match discovery observation is paused, held, or terminal".to_string());
        }
        Ok(MatchDiscoveryObservation {
            store_session: self.session_id.clone(),
            job_id: job_id.to_string(),
            consumed: AtomicBool::new(false),
        })
    }

    /// Acquire the database-writer lease for a result whose filesystem work
    /// was already admitted. This deliberately happens after traversal/stat/
    /// snapshot I/O, so a scarce writer is never held across filesystem
    /// latency. `Pausing` accepts the admitted result; paused/terminal jobs do
    /// not. Resource-pressure retries do not consume the observation token.
    pub fn acquire_discovery_write_for_observation(
        &self,
        job_id: &str,
        media_key: &str,
        request: ResourceRequest,
        observation: &MatchDiscoveryObservation,
    ) -> Result<MatchDiscoveryPermit, String> {
        let _database_unit = self.begin_database_unit()?;
        validate_media_key(media_key)?;
        validate_stage_resource_request(JobStage::Discover, request)?;
        if observation.store_session != self.session_id || observation.job_id != job_id {
            return Err(
                "Match discovery observation belongs to a different store or job".to_string(),
            );
        }
        if observation.consumed.load(Ordering::Acquire) {
            return Err("Match discovery observation was already consumed".to_string());
        }
        let lifecycle = {
            let _guard =
                self.database_read_guard("Match discovery result admission lock is poisoned")?;
            self.require_unlocked::<IndexJob>(JOB_TABLE, job_id, "IndexJob")?
                .lifecycle()?
        };
        if !lifecycle.accepts_discovery_result() {
            return Err("Match discovery result is paused or terminal".to_string());
        }
        match self.governor.try_acquire(request) {
            Ok(mut resources) => {
                if observation
                    .consumed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Err("Match discovery observation was already consumed".to_string());
                }
                self.remove_hold(HoldReason::ResourcePressure)?;
                resources.release_holds = Some(Arc::clone(&self.transient_holds));
                resources.tag_stage(JobStage::Discover)?;
                Ok(MatchDiscoveryPermit {
                    owner: Arc::downgrade(&self.store),
                    external_holds: Arc::clone(&self.external_holds),
                    admission_epoch: self.external_admission_epoch(),
                    settling_observation: true,
                    admitted_at: std::time::Instant::now(),
                    resources: Mutex::new(Some(resources)),
                    store_session: self.session_id.clone(),
                    job_id: job_id.to_string(),
                    media_key: media_key.to_string(),
                    consumed: AtomicBool::new(false),
                    authorized_payload_bytes: request.queued_bytes,
                })
            }
            Err(error) => {
                self.add_hold(HoldReason::ResourcePressure)?;
                Err(error)
            }
        }
    }

    /// Resource-only lease for an owned discovery snapshot that is not itself
    /// a database write. All discovery-result writers require a separate
    /// job-bound `MatchDiscoveryPermit`.
    pub fn acquire_discovery_snapshot_resources(
        &self,
        request: ResourceRequest,
    ) -> Result<MatchResourceLease, String> {
        validate_discovery_snapshot_resource_request(request)?;
        match self.governor.try_acquire(request) {
            Ok(mut resources) => {
                self.remove_hold(HoldReason::ResourcePressure)?;
                resources.release_holds = Some(Arc::clone(&self.transient_holds));
                Ok(resources)
            }
            Err(error) => {
                self.add_hold(HoldReason::ResourcePressure)?;
                Err(error)
            }
        }
    }

    /// Resize a live owned-snapshot lease from its conservative pre-read cap
    /// to the exact bytes now retained by the caller. The lease never stops
    /// owning the payload while its accounting changes.
    pub fn reaccount_discovery_snapshot_resources(
        &self,
        resources: &mut MatchResourceLease,
        request: ResourceRequest,
    ) -> Result<(), String> {
        validate_discovery_snapshot_resource_request(request)?;
        match self.governor.try_replace(resources, request) {
            Ok(()) => {
                self.remove_hold(HoldReason::ResourcePressure)?;
                Ok(())
            }
            Err(error) => {
                self.add_hold(HoldReason::ResourcePressure)?;
                Err(error)
            }
        }
    }

    /// Atomically transfer a live owned-snapshot lease into the physical
    /// stage permit that will consume those bytes. The caller retains the
    /// original snapshot lease on every pre-transfer or resource-pressure
    /// error, so retry/backoff cannot retain an unaccounted `Vec<u8>` and the
    /// replacement cannot deadlock by double-counting the same payload.
    pub fn acquire_background_stage_from_snapshot(
        &self,
        coordinator: &MediaIoCoordinator,
        root: RootIdentity,
        fence: &RevisionFence,
        stage: JobStage,
        request: ResourceRequest,
        snapshot_resources: &mut Option<MatchResourceLease>,
    ) -> Result<MatchStagePermit, String> {
        let _database_unit = self.begin_database_unit()?;
        validate_stage_resource_request(stage, request)?;
        let lifecycle = {
            let _guard = self.database_read_guard("Match stage admission lock is poisoned")?;
            let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
            let next_stage = asset.next_stage()?;
            if next_stage != stage
                && !asset
                    .completed_stages
                    .iter()
                    .any(|completed| completed == stage.as_str())
            {
                return Err(format!(
                    "Match stage admission out of order: expected {}, got {}",
                    asset.next_stage,
                    stage.as_str()
                ));
            }
            self.require_unlocked::<IndexJob>(JOB_TABLE, &asset.job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(lifecycle)? {
            return Err("Match stage admission is paused, held, or terminal".to_string());
        }

        let io = coordinator
            .enqueue(root, WorkClass::Background)
            .wait()
            .map_err(|error| format!("wait for Match background I/O permit: {error}"))?;
        let current_lifecycle = {
            let _guard = self.database_read_guard("Match stage admission lock is poisoned")?;
            let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
            let next_stage = asset.next_stage()?;
            if next_stage != stage
                && !asset
                    .completed_stages
                    .iter()
                    .any(|completed| completed == stage.as_str())
            {
                io.finish(PermitOutcome::Cancelled);
                return Err("Match stage admission changed while queued".to_string());
            }
            self.require_unlocked::<IndexJob>(JOB_TABLE, &asset.job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(current_lifecycle)? {
            io.finish(PermitOutcome::Cancelled);
            return Err("Match stage admission changed while queued".to_string());
        }

        let Some(resources) = snapshot_resources.as_mut() else {
            io.finish(PermitOutcome::Cancelled);
            return Err("Match snapshot resource lease was already transferred".to_string());
        };
        if let Err(error) = self.governor.try_replace(resources, request) {
            io.finish(PermitOutcome::Cancelled);
            self.add_hold(HoldReason::ResourcePressure)?;
            return Err(error);
        }
        if let Err(error) = self.remove_hold(HoldReason::ResourcePressure) {
            io.finish(PermitOutcome::Error);
            return Err(error);
        }
        let mut resources = snapshot_resources
            .take()
            .ok_or("Match snapshot resource lease was already transferred")?;
        resources.release_holds = Some(Arc::clone(&self.transient_holds));
        resources.tag_stage(stage)?;
        Ok(MatchStagePermit {
            owner: Arc::downgrade(&self.store),
            external_holds: Arc::clone(&self.external_holds),
            admission_epoch: self.external_admission_epoch(),
            io: Mutex::new(Some(io)),
            resources: Mutex::new(Some(resources)),
            store_session: self.session_id.clone(),
            fence: fence.clone(),
            stage,
            consumed: AtomicBool::new(false),
            authorized_payload_bytes: request.queued_bytes,
            audit_only: false,
            admitted_at: std::time::Instant::now(),
        })
    }

    pub fn acquire_background_stage(
        &self,
        coordinator: &MediaIoCoordinator,
        root: RootIdentity,
        fence: &RevisionFence,
        stage: JobStage,
        request: ResourceRequest,
    ) -> Result<MatchStagePermit, String> {
        let _database_unit = self.begin_database_unit()?;
        validate_stage_resource_request(stage, request)?;
        let lifecycle = {
            let _guard = self.database_read_guard("Match stage admission lock is poisoned")?;
            let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
            let next_stage = asset.next_stage()?;
            if next_stage != stage
                && !asset
                    .completed_stages
                    .iter()
                    .any(|completed| completed == stage.as_str())
            {
                return Err(format!(
                    "Match stage admission out of order: expected {}, got {}",
                    asset.next_stage,
                    stage.as_str()
                ));
            }
            self.require_unlocked::<IndexJob>(JOB_TABLE, &asset.job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(lifecycle)? {
            return Err("Match stage admission is paused, held, or terminal".to_string());
        }
        let mut resources = match self.governor.try_acquire(request) {
            Ok(mut resources) => {
                self.remove_hold(HoldReason::ResourcePressure)?;
                resources.release_holds = Some(Arc::clone(&self.transient_holds));
                resources
            }
            Err(error) => {
                self.add_hold(HoldReason::ResourcePressure)?;
                return Err(error);
            }
        };
        let io = coordinator
            .enqueue(root, WorkClass::Background)
            .wait()
            .map_err(|error| format!("wait for Match background I/O permit: {error}"))?;
        let current_lifecycle = {
            let _guard = self.database_read_guard("Match stage admission lock is poisoned")?;
            let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
            let next_stage = asset.next_stage()?;
            if next_stage != stage
                && !asset
                    .completed_stages
                    .iter()
                    .any(|completed| completed == stage.as_str())
            {
                io.finish(PermitOutcome::Cancelled);
                return Err("Match stage admission changed while queued".to_string());
            }
            self.require_unlocked::<IndexJob>(JOB_TABLE, &asset.job_id, "IndexJob")?
                .lifecycle()?
        };
        if !self.can_attempt_automatic(current_lifecycle)? {
            io.finish(PermitOutcome::Cancelled);
            return Err("Match stage admission changed while queued".to_string());
        }
        resources.tag_stage(stage)?;
        Ok(MatchStagePermit {
            owner: Arc::downgrade(&self.store),
            external_holds: Arc::clone(&self.external_holds),
            admission_epoch: self.external_admission_epoch(),
            io: Mutex::new(Some(io)),
            resources: Mutex::new(Some(resources)),
            store_session: self.session_id.clone(),
            fence: fence.clone(),
            stage,
            consumed: AtomicBool::new(false),
            authorized_payload_bytes: request.queued_bytes,
            audit_only: false,
            admitted_at: std::time::Instant::now(),
        })
    }

    /// Stage-scoped audit token for the Align and Embed components of the one
    /// fused identity kernel. Detect owns the single physical CPU/decode/I/O
    /// lease for that kernel; these tokens prevent double-counting the same
    /// physical work while still requiring explicit stage authorization and
    /// a bounded output budget before the atomic fused commit.
    pub fn acquire_fused_inference_audit_stage(
        &self,
        fence: &RevisionFence,
        stage: JobStage,
        authorized_payload_bytes: u64,
    ) -> Result<MatchStagePermit, String> {
        let _database_unit = self.begin_database_unit()?;
        if !matches!(stage, JobStage::Align | JobStage::Embed) || authorized_payload_bytes == 0 {
            return Err(
                "fused inference audit stage must be Align or Embed with a payload budget"
                    .to_string(),
            );
        }
        let _guard = self.database_read_guard("Match fused-stage audit lock is poisoned")?;
        let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if asset.next_stage()? != JobStage::Detect {
            return Err("fused inference audit stage requires the Detect cursor".to_string());
        }
        let lifecycle = self
            .require_unlocked::<IndexJob>(JOB_TABLE, &asset.job_id, "IndexJob")?
            .lifecycle()?;
        if !self.can_attempt_automatic(lifecycle)? {
            return Err("Match stage admission is paused, held, or terminal".to_string());
        }
        Ok(MatchStagePermit {
            owner: Arc::downgrade(&self.store),
            external_holds: Arc::clone(&self.external_holds),
            admission_epoch: self.external_admission_epoch(),
            io: Mutex::new(None),
            resources: Mutex::new(None),
            store_session: self.session_id.clone(),
            fence: fence.clone(),
            stage,
            consumed: AtomicBool::new(false),
            authorized_payload_bytes,
            audit_only: true,
            admitted_at: std::time::Instant::now(),
        })
    }

    fn ensure_schema(&self) -> Result<(), String> {
        let _guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| "Match schema lock is poisoned".to_string())?;
        let db = self.database();
        let updated_at = now();
        surreal_store::run(async move {
            db.query(MATCH_SCHEMA_MARKER_BOOTSTRAP_SQL)
                .await
                .map_err(|error| format!("bootstrap Match schema marker: {error}"))?
                .check()
                .map_err(|error| format!("bootstrap Match schema marker: {error}"))?;
            let mut compatibility = db
                .query("SELECT * OMIT id FROM ONLY type::record('match_schema_state', 'global');")
                .await
                .map_err(|error| format!("read Match schema marker: {error}"))?;
            let existing: Option<Value> = compatibility
                .take(0)
                .map_err(|error| format!("decode Match schema marker: {error}"))?;
            if let Some(existing) = existing {
                let mut schema_version = existing
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .ok_or("Match schema marker omitted schema_version")?;
                let engine_version = existing
                    .get("engine_version")
                    .and_then(Value::as_str)
                    .ok_or("Match schema marker omitted engine_version")?;
                let schema_generation = existing
                    .get("schema_generation")
                    .and_then(Value::as_str)
                    .ok_or("Match schema marker omitted schema_generation")?;
                if schema_version == 2
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V2_TO_V3_SQL)
                        .bind(("schema_version", 3_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v2 to v3: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v2 to v3: {error}"))?;
                    schema_version = 3;
                }
                if schema_version == 3
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V3_TO_V4_SQL)
                        .bind(("schema_version", 4_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v3 to v4: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v3 to v4: {error}"))?;
                    schema_version = 4;
                }
                if schema_version == 4
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V4_TO_V5_SQL)
                        .bind(("schema_version", 5_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v4 to v5: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v4 to v5: {error}"))?;
                    schema_version = 5;
                }
                if schema_version == 5
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V5_TO_V6_SQL)
                        .bind(("schema_version", 6_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v5 to v6: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v5 to v6: {error}"))?;
                    schema_version = 6;
                }
                if schema_version == 6
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V6_TO_V7_SQL)
                        .bind(("schema_version", 7_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v6 to v7: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v6 to v7: {error}"))?;
                    schema_version = 7;
                }
                if schema_version == 7
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V7_TO_V8_SQL)
                        .bind(("schema_version", 8_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v7 to v8: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v7 to v8: {error}"))?;
                    schema_version = 8;
                }
                if schema_version == 8
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V8_TO_V9_SQL)
                        .bind(("schema_version", 9_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v8 to v9: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v8 to v9: {error}"))?;
                    schema_version = 9;
                }
                if schema_version == 9
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V9_TO_V10_PREPARE_SQL)
                        .await
                        .map_err(|error| format!("prepare Match schema v9 to v10: {error}"))?
                        .check()
                        .map_err(|error| format!("prepare Match schema v9 to v10: {error}"))?;
                    let mut orphan_response = db
                        .query("SELECT count() AS count FROM match_assignment WHERE media_key = NONE GROUP ALL;")
                        .await
                        .map_err(|error| format!("verify Match v10 assignment backfill: {error}"))?;
                    let orphan_rows: Vec<Value> = orphan_response
                        .take(0)
                        .map_err(|error| format!("decode Match v10 orphan count: {error}"))?;
                    let orphan_count = orphan_rows
                        .first()
                        .and_then(|row| row.get("count"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if orphan_count != 0 {
                        return Err(format!(
                            "Match schema v10 migration found {orphan_count} assignments without a canonical face media key"
                        ));
                    }
                    db.query(MATCH_SCHEMA_V9_TO_V10_FINALIZE_SQL)
                        .bind(("schema_version", 10_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("finalize Match schema v9 to v10: {error}"))?
                        .check()
                        .map_err(|error| format!("finalize Match schema v9 to v10: {error}"))?;
                    schema_version = 10;
                }
                if schema_version == 10
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V10_TO_V11_SQL)
                        .bind(("schema_version", 11_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v10 to v11: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v10 to v11: {error}"))?;
                    schema_version = 11;
                }
                if schema_version == 11
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    let migration = format!(
                        "BEGIN TRANSACTION;\n{}\nUPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;\nCOMMIT TRANSACTION;",
                        corrections::WP084_CORRECTIONS_SCHEMA_SQL
                    );
                    db.query(migration)
                        .bind(("schema_version", 12_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v11 to v12: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v11 to v12: {error}"))?;
                    schema_version = 12;
                }
                if schema_version == 12
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V12_TO_V13_SQL)
                        .bind(("schema_version", 13_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v12 to v13: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v12 to v13: {error}"))?;
                    schema_version = 13;
                }
                if schema_version == 13
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V13_TO_V14_SQL)
                        .bind(("schema_version", 14_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v13 to v14: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v13 to v14: {error}"))?;
                    schema_version = 14;
                }
                if schema_version == 14
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V14_TO_V15_SQL)
                        .bind(("schema_version", 15_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v14 to v15: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v14 to v15: {error}"))?;
                    schema_version = 15;
                }
                if schema_version == 15
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V15_TO_V16_SQL)
                        .bind(("schema_version", 16_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v15 to v16: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v15 to v16: {error}"))?;
                    schema_version = 16;
                }
                if schema_version == 16
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V16_TO_V17_SQL)
                        .bind(("schema_version", 17_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v16 to v17: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v16 to v17: {error}"))?;
                    schema_version = 17;
                }
                if schema_version == 17
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    // Make the v18 provenance surface queryable without
                    // advancing the marker. A crash or later validation error
                    // leaves schema_version=17 and the migration safely resumes.
                    db.query(MATCH_SCHEMA_V17_TO_V18_PREPARE_SQL)
                        .await
                        .map_err(|error| format!("prepare Match schema v17 to v18: {error}"))?
                        .check()
                        .map_err(|error| format!("prepare Match schema v17 to v18: {error}"))?;
                    let direct_kinds = LEGACY_DIRECT_MEDIA_KINDS
                        .iter()
                        .map(|kind| (*kind).to_string())
                        .collect::<Vec<_>>();
                    let mut after_direct_operation_id = String::new();
                    loop {
                        let mut direct_response = db
                            .query(
                                "SELECT * OMIT id FROM match_operation WITH INDEX match_operation_kind_operation \
                                 WHERE kind IN $direct_kinds AND operation_id > $after_operation_id \
                                 ORDER BY operation_id ASC LIMIT 256;",
                            )
                            .bind(("direct_kinds", direct_kinds.clone()))
                            .bind(("after_operation_id", after_direct_operation_id.clone()))
                            .await
                            .map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "query direct-operation page: {error}"
                                ))
                            })?;
                        let direct_operations: Vec<MatchOperation> =
                            direct_response.take(0).map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "decode direct-operation page: {error}"
                                ))
                            })?;
                        if direct_operations.is_empty() {
                            break;
                        }
                        if direct_operations.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT
                            || direct_operations.iter().any(|operation| {
                                operation.operation_id <= after_direct_operation_id
                            })
                        {
                            return Err(legacy_direct_media_backfill_error(
                                "direct-operation page violated its stable cursor bound",
                            ));
                        }
                        let next_direct_operation_id = direct_operations
                            .last()
                            .expect("non-empty page")
                            .operation_id
                            .clone();
                        let relevant_face_ids = direct_operations
                            .iter()
                            .filter_map(|operation| operation.face_id.clone())
                            .collect::<BTreeSet<_>>();
                        let face_ids = relevant_face_ids.iter().cloned().collect::<Vec<_>>();
                        let mut face_response = db
                            .query(
                                "SELECT * OMIT id FROM match_face_observation \
                                 WHERE face_id IN $face_ids ORDER BY face_id ASC LIMIT 257;",
                            )
                            .bind(("face_ids", face_ids))
                            .await
                            .map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "query exact Face evidence page: {error}"
                                ))
                            })?;
                        let face_rows: Vec<FaceObservation> =
                            face_response.take(0).map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "decode exact Face evidence page: {error}"
                                ))
                            })?;
                        if face_rows.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT {
                            return Err(legacy_direct_media_backfill_error(
                                "Face evidence page exceeded the direct-operation page bound",
                            ));
                        }
                        let mut faces = BTreeMap::new();
                        let mut historical_evidence = LegacyDirectMediaEvidence::default();
                        for face in face_rows {
                            historical_evidence.bind_face(&face, "current FaceObservation")?;
                            if faces.insert(face.face_id.clone(), face).is_some() {
                                return Err(legacy_direct_media_backfill_error(
                                    "duplicate Face evidence for one face_id",
                                ));
                            }
                        }

                        let operation_ids = direct_operations
                            .iter()
                            .map(|operation| operation.operation_id.clone())
                            .collect::<Vec<_>>();
                        let mut mapping_response = db
                            .query(
                                "SELECT * OMIT id FROM match_correction_media_operation \
                                 WHERE operation_id IN $operation_ids \
                                 ORDER BY operation_id ASC, mapping_id ASC LIMIT 257;",
                            )
                            .bind(("operation_ids", operation_ids))
                            .await
                            .map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "query persisted direct-media mappings: {error}"
                                ))
                            })?;
                        let mapping_rows: Vec<corrections::CorrectionMediaOperation> =
                            mapping_response.take(0).map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "decode persisted direct-media mappings: {error}"
                                ))
                            })?;
                        if mapping_rows.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT {
                            return Err(legacy_direct_media_backfill_error(
                                "one or more direct operations have multiple persisted media mappings",
                            ));
                        }
                        let mut existing_mappings = BTreeMap::new();
                        for mapping in mapping_rows {
                            historical_evidence
                                .bind_mapping(&mapping, "persisted direct-media mapping")?;
                            if existing_mappings
                                .insert(mapping.operation_id.clone(), mapping)
                                .is_some()
                            {
                                return Err(legacy_direct_media_backfill_error(
                                    "one direct operation has multiple persisted media mappings",
                                ));
                            }
                        }

                        // A deleted Face remains recoverable from its exact typed
                        // correction snapshots. Walk stable bounded pages and scan
                        // to completion so disagreeing historical snapshots fail
                        // closed instead of whichever row happened to arrive first.
                        let has_deleted_face = relevant_face_ids
                            .iter()
                            .any(|face_id| !faces.contains_key(face_id));
                        if has_deleted_face {
                            let mut after_history_operation_id = String::new();
                            loop {
                                let mut history_response = db
                                    .query(
                                        "SELECT * OMIT id FROM match_operation \
                                         WHERE operation_id > $after_operation_id \
                                         ORDER BY operation_id ASC LIMIT 256;",
                                    )
                                    .bind((
                                        "after_operation_id",
                                        after_history_operation_id.clone(),
                                    ))
                                    .await
                                    .map_err(|error| {
                                        legacy_direct_media_backfill_error(format!(
                                            "query historical correction page: {error}"
                                        ))
                                    })?;
                                let history: Vec<MatchOperation> =
                                    history_response.take(0).map_err(|error| {
                                        legacy_direct_media_backfill_error(format!(
                                            "decode historical correction page: {error}"
                                        ))
                                    })?;
                                if history.is_empty() {
                                    break;
                                }
                                if history.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT
                                    || history.iter().any(|operation| {
                                        operation.operation_id <= after_history_operation_id
                                    })
                                {
                                    return Err(legacy_direct_media_backfill_error(
                                        "historical correction page violated its stable cursor bound",
                                    ));
                                }
                                for operation in &history {
                                    historical_evidence
                                        .bind_correction_snapshots(operation, &relevant_face_ids)?;
                                }
                                after_history_operation_id =
                                    history.last().expect("non-empty page").operation_id.clone();
                            }

                            let face_ids = relevant_face_ids.iter().cloned().collect::<Vec<_>>();
                            let mut after_provenance_id = String::new();
                            loop {
                                let mut provenance_response = db
                                    .query(
                                        "SELECT * OMIT id FROM match_suggestion_source_provenance \
                                         WHERE face_id IN $face_ids AND provenance_id > $after_provenance_id \
                                         ORDER BY provenance_id ASC LIMIT 256;",
                                    )
                                    .bind(("face_ids", face_ids.clone()))
                                    .bind(("after_provenance_id", after_provenance_id.clone()))
                                    .await
                                    .map_err(|error| {
                                        legacy_direct_media_backfill_error(format!(
                                            "query historical suggestion-provenance page: {error}"
                                        ))
                                    })?;
                                let provenance_rows: Vec<corrections::SuggestionSourceProvenance> =
                                    provenance_response.take(0).map_err(|error| {
                                        legacy_direct_media_backfill_error(format!(
                                            "decode historical suggestion-provenance page: {error}"
                                        ))
                                    })?;
                                if provenance_rows.is_empty() {
                                    break;
                                }
                                if provenance_rows.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT
                                    || provenance_rows
                                        .iter()
                                        .any(|row| row.provenance_id <= after_provenance_id)
                                {
                                    return Err(legacy_direct_media_backfill_error(
                                        "historical suggestion-provenance page violated its stable cursor bound",
                                    ));
                                }
                                for provenance in &provenance_rows {
                                    historical_evidence.bind_suggestion_provenance(
                                        provenance,
                                        &relevant_face_ids,
                                    )?;
                                }
                                after_provenance_id = provenance_rows
                                    .last()
                                    .expect("non-empty page")
                                    .provenance_id
                                    .clone();
                            }
                        }

                        let mut relevant_media_keys = BTreeSet::new();
                        for operation in &direct_operations {
                            let face_id = operation.face_id.as_deref().ok_or_else(|| {
                                legacy_direct_media_backfill_error(format!(
                                    "{} operation {} omitted face_id",
                                    operation.kind, operation.operation_id
                                ))
                            })?;
                            relevant_media_keys.insert(legacy_direct_operation_media_key(
                                operation,
                                faces.get(face_id),
                                historical_evidence.by_face.get(face_id),
                                existing_mappings.get(&operation.operation_id),
                            )?);
                        }

                        // Mapping rows are immutable exact-fingerprint evidence.
                        // Page across their complete history by stable mapping_id;
                        // this replaces the former undocumented 4096-row lifetime
                        // ceiling with bounded per-query memory.
                        let relevant_media_keys =
                            relevant_media_keys.into_iter().collect::<Vec<_>>();
                        if has_deleted_face {
                            let mut after_mapping_id = String::new();
                            loop {
                                let mut history_mapping_response = db
                                .query(
                                    "SELECT * OMIT id FROM match_correction_media_operation \
                                     WHERE media_key IN $media_keys AND mapping_id > $after_mapping_id \
                                     ORDER BY mapping_id ASC LIMIT 256;",
                                )
                                .bind(("media_keys", relevant_media_keys.clone()))
                                .bind(("after_mapping_id", after_mapping_id.clone()))
                                .await
                                .map_err(|error| {
                                    legacy_direct_media_backfill_error(format!(
                                        "query historical media-evidence page: {error}"
                                    ))
                                })?;
                                let history_mappings: Vec<corrections::CorrectionMediaOperation> =
                                    history_mapping_response.take(0).map_err(|error| {
                                        legacy_direct_media_backfill_error(format!(
                                            "decode historical media-evidence page: {error}"
                                        ))
                                    })?;
                                if history_mappings.is_empty() {
                                    break;
                                }
                                if history_mappings.len() > LEGACY_OPERATION_QUERY_PAGE_LIMIT
                                    || history_mappings
                                        .iter()
                                        .any(|mapping| mapping.mapping_id <= after_mapping_id)
                                {
                                    return Err(legacy_direct_media_backfill_error(
                                    "historical media-evidence page violated its stable cursor bound",
                                ));
                                }
                                for mapping in &history_mappings {
                                    historical_evidence
                                        .bind_mapping(mapping, "historical correction mapping")?;
                                }
                                after_mapping_id = history_mappings
                                    .last()
                                    .expect("non-empty page")
                                    .mapping_id
                                    .clone();
                            }
                        }

                        let direct_media_backfills = derive_legacy_direct_media_backfills(
                            &direct_operations,
                            &faces,
                            &historical_evidence,
                            &existing_mappings,
                        )?;
                        db.query(MATCH_SCHEMA_V17_TO_V18_BACKFILL_PAGE_SQL)
                            .bind(("direct_media_backfills", direct_media_backfills))
                            .await
                            .map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "persist direct-media backfill page: {error}"
                                ))
                            })?
                            .check()
                            .map_err(|error| {
                                legacy_direct_media_backfill_error(format!(
                                    "persist direct-media backfill page: {error}"
                                ))
                            })?;
                        after_direct_operation_id = next_direct_operation_id;
                    }
                    db.query(MATCH_SCHEMA_V17_TO_V18_SQL)
                        .bind(("schema_version", 18_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .bind((
                            "direct_media_backfills",
                            Vec::<corrections::CorrectionMediaOperation>::new(),
                        ))
                        .await
                        .map_err(|error| format!("migrate Match schema v17 to v18: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v17 to v18: {error}"))?;
                    schema_version = 18;
                }
                if schema_version == 18
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(MATCH_SCHEMA_V18_TO_V19_SQL)
                        .bind(("schema_version", 19_u64))
                        .bind(("updated_at", updated_at.clone()))
                        .await
                        .map_err(|error| format!("migrate Match schema v18 to v19: {error}"))?
                        .check()
                        .map_err(|error| format!("migrate Match schema v18 to v19: {error}"))?;
                    schema_version = 19;
                }
                if schema_version == 19
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(format!("BEGIN TRANSACTION; {} {} {} UPDATE match_schema_state SET schema_version = 20, updated_at = $updated_at WHERE schema_version = 19; COMMIT TRANSACTION;", worker::WP086_WORKER_SCHEMA_SQL, video::WP086_VIDEO_SCHEMA_SQL, context::WP086_CONTEXT_SCHEMA_SQL))
                        .bind(("updated_at", updated_at.clone()))
                        .await.map_err(|error| format!("migrate Match schema v19 to v20: {error}"))?
                        .check().map_err(|error| format!("migrate Match schema v19 to v20: {error}"))?;
                    schema_version = 20;
                }
                if schema_version == 20
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query(format!("BEGIN TRANSACTION; {} UPDATE match_schema_state SET schema_version = 21, updated_at = $updated_at WHERE schema_version = 20; COMMIT TRANSACTION;", context::MEDIA_CONTEXT_SCHEMA))
                        .bind(("updated_at", updated_at.clone())).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
                    schema_version = 21;
                }
                if schema_version == 21
                    && engine_version == surreal_store::ENGINE_VERSION
                    && schema_generation == MATCH_SCHEMA_GENERATION
                {
                    db.query("BEGIN TRANSACTION; DEFINE INDEX OVERWRITE match_job_asset_source ON TABLE match_job_asset FIELDS source_path, media_key; UPDATE match_schema_state:global SET schema_version = 22, updated_at = $updated_at; COMMIT TRANSACTION;")
                        .bind(("updated_at", updated_at.clone())).await
                        .map_err(|error| format!("migrate Match schema v21 to v22: {error}"))?
                        .check().map_err(|error| format!("migrate Match schema v21 to v22: {error}"))?;
                    schema_version = 22;
                }
                if schema_version != MATCH_SCHEMA_VERSION
                    || engine_version != surreal_store::ENGINE_VERSION
                    || schema_generation != MATCH_SCHEMA_GENERATION
                {
                    return Err(format!(
                        "incompatible Match schema marker: schema_version={schema_version}, engine_version={engine_version}, schema_generation={schema_generation}"
                    ));
                }
                return Ok(());
            }
            let schema_body = MATCH_SCHEMA_SQL
                .trim_end()
                .strip_suffix("COMMIT TRANSACTION;")
                .ok_or("Match schema transaction terminator missing")?;
            db.query(format!(
                "{} {} {} {} {} COMMIT TRANSACTION;",
                schema_body,
                worker::WP086_WORKER_SCHEMA_SQL,
                video::WP086_VIDEO_SCHEMA_SQL,
                context::WP086_CONTEXT_SCHEMA_SQL,
                context::MEDIA_CONTEXT_SCHEMA
            ))
            .bind(("schema_version", MATCH_SCHEMA_VERSION))
            .bind(("engine_version", surreal_store::ENGINE_VERSION))
            .bind(("schema_generation", MATCH_SCHEMA_GENERATION))
            .bind(("updated_at", updated_at))
            .await
            .map_err(|error| format!("define Match schema: {error}"))?
            .check()
            .map_err(|error| format!("define Match schema: {error}"))?;
            Ok(())
        })
    }

    pub fn create_person(&self, name: &str, aliases: Vec<String>) -> Result<Person, String> {
        validate_text("Person name", name)?;
        let mut normalized_aliases = aliases
            .into_iter()
            .map(|alias| alias.trim().to_string())
            .filter(|alias| !alias.is_empty())
            .collect::<Vec<_>>();
        normalized_aliases.sort_by_key(|alias| alias.to_lowercase());
        normalized_aliases.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        for alias in &normalized_aliases {
            validate_text("Person alias", alias)?;
        }
        let _guard = self.mutation_write_guard("Match Person creation")?;
        self.invalidate_calibrations_unlocked("person_created")?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, false, true)?;
        let now = now();
        let person = Person {
            person_id: new_id("person"),
            name: name.trim().to_string(),
            aliases: normalized_aliases,
            cover_media_key: None,
            hidden: false,
            favorite: false,
            revision: 1,
            catalog_revision: execution.catalog_revision,
            created_at: now.clone(),
            updated_at: now,
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    PERSON_TABLE,
                    &person.person_id,
                    serde_json::to_value(&person).map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(person)
    }

    pub fn update_person_preferences(
        &self,
        person_id: &str,
        expected_revision: u64,
        cover_media_key: Option<String>,
        hidden: bool,
        favorite: bool,
    ) -> Result<Person, String> {
        let cover_media_key = cover_media_key
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(value) = &cover_media_key {
            validate_media_key(value)?;
        }
        let _guard = self.mutation_write_guard("Match Person preference")?;
        let mut person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        if person.revision != expected_revision {
            return Err("stale Person revision".to_string());
        }
        if let Some(media_key) = &cover_media_key {
            let assignments = self.list_unlocked::<Assignment>(ASSIGNMENT_TABLE)?;
            let faces = self.list_unlocked::<FaceObservation>(FACE_TABLE)?;
            let assigned_face_ids = assignments
                .iter()
                .filter(|assignment| {
                    assignment.person_id == person_id
                        && assignment.state == AssignmentState::OperatorConfirmed.as_str()
                })
                .map(|assignment| assignment.face_id.as_str())
                .collect::<BTreeSet<_>>();
            if !faces.iter().any(|face| {
                face.media_key == *media_key && assigned_face_ids.contains(face.face_id.as_str())
            }) {
                return Err("Person cover must reference media in that Person gallery".to_string());
            }
        }
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, false, true)?;
        person.cover_media_key = cover_media_key;
        person.hidden = hidden;
        person.favorite = favorite;
        person.revision = person
            .revision
            .checked_add(1)
            .ok_or("Person revision overflow")?;
        person.catalog_revision = execution.catalog_revision;
        person.updated_at = now();
        self.commit_person_revision_change_unlocked(&person, &execution)?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(person)
    }

    pub fn configure_index_root(
        &self,
        path: &Path,
        exclusions: Vec<String>,
    ) -> Result<MatchIndexRoot, String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| format!("resolve Match indexing root: {error}"))?;
        if !canonical.is_dir() {
            return Err("Match indexing root must be an existing directory".to_string());
        }
        let canonical_text = canonical.to_string_lossy().to_string();
        validate_text("Match indexing root", &canonical_text)?;
        let mut normalized = Vec::new();
        for exclusion in exclusions {
            let exclusion = exclusion.trim().replace('\\', "/");
            if exclusion.is_empty() {
                continue;
            }
            let relative = Path::new(&exclusion);
            if relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err("Match exclusions must be root-relative paths".to_string());
            }
            validate_text("Match indexing exclusion", &exclusion)?;
            normalized.push(exclusion);
        }
        normalized.sort();
        normalized.dedup();
        let root_id = stable_pair_id("match-root", &canonical_text, "v1");
        let _guard = self.mutation_write_guard("Match indexing-root")?;
        let existing = self.get_one_unlocked::<MatchIndexRoot>(ROOT_CONFIG_TABLE, &root_id)?;
        let timestamp = now();
        let root = MatchIndexRoot {
            root_id: root_id.clone(),
            path: canonical_text,
            exclusions: normalized,
            enabled: true,
            created_at: existing
                .as_ref()
                .map(|root| root.created_at.clone())
                .unwrap_or_else(|| timestamp.clone()),
            updated_at: timestamp,
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                ROOT_CONFIG_TABLE,
                &root_id,
                serde_json::to_value(&root).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        Ok(root)
    }

    pub fn remove_index_root(&self, root_id: &str) -> Result<(), String> {
        validate_text("Match indexing root id", root_id)?;
        let _guard = self.mutation_write_guard("Match indexing-root removal")?;
        self.require_unlocked::<MatchIndexRoot>(ROOT_CONFIG_TABLE, root_id, "Match index root")?;
        self.transactional_upserts_deletes_unlocked(&[], &[(ROOT_CONFIG_TABLE, root_id)])
    }

    pub fn index_roots(&self) -> Result<Vec<MatchIndexRoot>, String> {
        let mut roots = self.list::<MatchIndexRoot>(ROOT_CONFIG_TABLE)?;
        roots.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(roots)
    }

    pub fn index_roots_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchIndexRoot>, String> {
        if limit == 0 || limit > 512 {
            return Err("Match root page limit must be between 1 and 512".to_string());
        }
        let _guard = self.database_read_guard("Match root page lock is poisoned")?;
        let db = self.database();
        surreal_store::run(async move {
            let mut response = db
                .query("SELECT * OMIT id FROM match_index_root ORDER BY path ASC, root_id ASC LIMIT $limit START $offset;")
                .bind(("limit", limit as u64))
                .bind(("offset", offset as u64))
                .await
                .map_err(|error| format!("query Match root page: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match root page: {error}"))
        })
    }

    pub fn index_root(&self, root_id: &str) -> Result<MatchIndexRoot, String> {
        self.require(ROOT_CONFIG_TABLE, root_id, "Match index root")
    }

    pub fn start_index_job(
        &self,
        root_id: &str,
        model_generation: &str,
    ) -> Result<IndexJob, String> {
        let root = self.index_root(root_id)?;
        if !root.enabled {
            return Err("Match indexing root is disabled".to_string());
        }
        self.create_job(root_id, model_generation)
    }

    pub fn jobs(&self) -> Result<Vec<IndexJob>, String> {
        let mut jobs = self.list::<IndexJob>(JOB_TABLE)?;
        jobs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        Ok(jobs)
    }

    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<IndexJob>, String> {
        self.recent_jobs_page(0, limit)
    }

    pub fn recent_jobs_page(&self, offset: usize, limit: usize) -> Result<Vec<IndexJob>, String> {
        if limit == 0 || limit > 512 {
            return Err("Match recent-job limit must be between 1 and 512".to_string());
        }
        let _guard = self.database_read_guard("Match recent-job lock is poisoned")?;
        let db = self.database();
        surreal_store::run(async move {
            let mut response = db
                .query("SELECT * OMIT id FROM match_index_job ORDER BY created_at DESC, job_id ASC LIMIT $limit START $offset;")
                .bind(("limit", limit as u64))
                .bind(("offset", offset as u64))
                .await
                .map_err(|error| format!("query recent Match jobs: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode recent Match jobs: {error}"))
        })
    }

    fn job_status_unlocked(&self) -> Result<(bool, bool, bool), String> {
        let db = self.database();
        let (totals, unsettled, partials): (Vec<Value>, Vec<Value>, Vec<Value>) =
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT count() AS count FROM match_index_job GROUP ALL;\
                         SELECT count() AS count FROM match_index_job WHERE lifecycle NOT IN ['completed', 'cancelled', 'failed', 'partial'] GROUP ALL;\
                         SELECT count() AS count FROM match_index_job WHERE failed > 0 OR completed < discovered OR lifecycle != 'completed' GROUP ALL;",
                    )
                    .await
                    .map_err(|error| format!("query Match job status aggregates: {error}"))?;
                Ok((
                    response
                        .take(0)
                        .map_err(|error| format!("decode Match job total: {error}"))?,
                    response
                        .take(1)
                        .map_err(|error| format!("decode Match unsettled total: {error}"))?,
                    response
                        .take(2)
                        .map_err(|error| format!("decode Match partial total: {error}"))?,
                ))
            })?;
        let count = |rows: &[Value]| {
            rows.first()
                .and_then(|row| row.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let total = count(&totals);
        Ok((
            total > 0,
            total > 0 && count(&partials) > 0,
            total > 0 && count(&unsettled) == 0,
        ))
    }

    pub fn job_assets(&self, job_id: &str) -> Result<Vec<JobAsset>, String> {
        let _database_unit = self.begin_database_unit()?;
        let mut assets = self
            .list::<JobAsset>(JOB_ASSET_TABLE)?
            .into_iter()
            .filter(|asset| asset.job_id == job_id)
            .collect::<Vec<_>>();
        assets.sort_by(|left, right| left.media_key.cmp(&right.media_key));
        Ok(assets)
    }

    pub fn job(&self, job_id: &str) -> Result<IndexJob, String> {
        let _database_unit = self.begin_database_unit()?;
        self.require(JOB_TABLE, job_id, "IndexJob")
    }

    pub fn job_asset(&self, asset_id: &str) -> Result<JobAsset, String> {
        let _database_unit = self.begin_database_unit()?;
        self.require(JOB_ASSET_TABLE, asset_id, "JobAsset")
    }

    fn prepare_job_retry(&self, job_id: &str) -> Result<IndexJob, String> {
        let _guard = self.mutation_write_guard("Match retry preparation")?;
        self.require_worker_exit_before_retry_unlocked(job_id)?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !matches!(
            job.lifecycle()?,
            JobLifecycle::Failed
                | JobLifecycle::Partial
                | JobLifecycle::Retrying
                | JobLifecycle::Paused
                | JobLifecycle::Blocked
        ) {
            return Err(
                "only failed, partial, paused, blocked, or retrying Match jobs can prepare retry"
                    .to_string(),
            );
        }
        let mut assets = self
            .list_unlocked::<JobAsset>(JOB_ASSET_TABLE)?
            .into_iter()
            .filter(|asset| asset.job_id == job_id)
            .collect::<Vec<_>>();
        let mut upserts = Vec::<(String, String, Value)>::new();
        for asset in &mut assets {
            if asset.skipped_code.is_some()
                || asset
                    .completed_stages
                    .iter()
                    .any(|stage| stage == JobStage::Complete.as_str())
            {
                continue;
            }
            asset.next_stage = JobStage::Discover.as_str().to_string();
            asset.completed_stages.clear();
            // A pre-fingerprint discovery placeholder remains an honest
            // failure until the retried walk sees the source again. Clearing
            // it here would let a disappeared source make the job settle
            // Completed without ever being rediscovered.
            if !asset.media_fingerprint.starts_with("unavailable:") {
                asset.failure_code = None;
                asset.failure_message = None;
            }
            asset.updated_at = now();
            upserts.push((
                JOB_ASSET_TABLE.to_string(),
                asset.asset_id.clone(),
                serde_json::to_value(&*asset).map_err(|error| error.to_string())?,
            ));
        }
        job.failed = assets
            .iter()
            .filter(|asset| asset.failure_code.is_some())
            .count() as u64;
        job.completed = assets
            .iter()
            .filter(|asset| {
                asset
                    .completed_stages
                    .iter()
                    .any(|stage| stage == JobStage::Complete.as_str())
            })
            .count() as u64;
        job.skipped = assets
            .iter()
            .filter(|asset| asset.skipped_code.is_some())
            .count() as u64;
        job.failure_code = None;
        job.failure_message = None;
        job.lifecycle = JobLifecycle::Retrying.as_str().to_string();
        job.updated_at = now();
        upserts.push((
            JOB_TABLE.to_string(),
            job.job_id.clone(),
            serde_json::to_value(&job).map_err(|error| error.to_string())?,
        ));
        let refs = upserts
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&refs, &[])?;
        Ok(job)
    }

    pub fn control_job(&self, job_id: &str, action: &str) -> Result<IndexJob, String> {
        if matches!(action, "resume" | "retry") {
            self.reconcile_database_operations()?;
        }
        let current = self.require::<IndexJob>(JOB_TABLE, job_id, "IndexJob")?;
        let owner_quarantined = self
            .holds()?
            .iter()
            .any(|reason| reason == "database_owner_quarantined");
        let result = match action {
            "pause" => match current.lifecycle()? {
                JobLifecycle::Running => self.set_job_lifecycle(job_id, JobLifecycle::Pausing),
                JobLifecycle::Queued | JobLifecycle::Retrying => {
                    self.set_job_lifecycle(job_id, JobLifecycle::Paused)
                }
                JobLifecycle::Paused => Ok(current),
                _ => Err("only queued, running, or retrying Match jobs can pause".to_string()),
            },
            "resume" => match current.lifecycle()? {
                JobLifecycle::Running if owner_quarantined => Ok(current),
                JobLifecycle::Paused | JobLifecycle::Retrying => {
                    self.set_job_lifecycle(job_id, JobLifecycle::Running)
                }
                JobLifecycle::Blocked => {
                    self.prepare_job_retry(job_id)?;
                    self.set_job_lifecycle(job_id, JobLifecycle::Running)
                }
                _ => Err("only paused, blocked, or retrying Match jobs can resume".to_string()),
            },
            "cancel" => self.set_job_lifecycle(job_id, JobLifecycle::Cancelled),
            "retry" => match current.lifecycle()? {
                JobLifecycle::Running if owner_quarantined => Ok(current),
                JobLifecycle::Failed | JobLifecycle::Partial => self.prepare_job_retry(job_id),
                _ => Err("only failed or partial Match jobs can retry".to_string()),
            },
            _ => Err("unknown Match job control action".to_string()),
        };
        if result.is_ok() && matches!(action, "resume" | "retry") {
            for job in self.jobs()? {
                self.job_assets(&job.job_id)?;
            }
            self.resolve_pending_database_failures(None)?;
            self.remove_hold(HoldReason::DatabaseOwnerQuarantined)?;
        }
        result
    }

    pub fn catalog_snapshot(
        &self,
        offset: usize,
        limit: usize,
        include_hidden: bool,
    ) -> Result<MatchCatalogSnapshot, String> {
        if limit == 0 || limit > 512 {
            return Err("Match catalog page limit must be between 1 and 512".to_string());
        }
        let _guard = self.database_read_guard("Match catalog snapshot lock is poisoned")?;
        let db = self.database();
        let (people, totals): (Vec<Person>, Vec<Value>) = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_person WHERE $include_hidden OR hidden = false ORDER BY favorite DESC, name ASC, person_id ASC LIMIT $limit START $offset;\
                     SELECT count() AS count FROM match_person WHERE $include_hidden OR hidden = false GROUP ALL;",
                )
                .bind(("include_hidden", include_hidden))
                .bind(("limit", limit as u64))
                .bind(("offset", offset as u64))
                .await
                .map_err(|error| format!("query Match catalog snapshot: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match people page: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match people count: {error}"))?,
            ))
        })?;
        let page_ids = people
            .iter()
            .map(|person| person.person_id.clone())
            .collect::<Vec<_>>();
        let cover_keys = people
            .iter()
            .filter_map(|person| person.cover_media_key.clone())
            .collect::<Vec<_>>();
        let db = self.database();
        let (assignment_counts, suggestion_counts, cover_assets): (
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
        ) = surreal_store::run(async move {
            let mut response = db
                    .query(
                        "SELECT person_id, count() AS count FROM match_assignment WHERE person_id IN $page_ids GROUP BY person_id;\
                         SELECT candidate_person_id, count() AS count FROM match_suggestion WHERE candidate_person_id IN $page_ids GROUP BY candidate_person_id;\
                         (SELECT id, media_key FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key IN $cover_keys GROUP BY media_key ORDER BY media_key ASC LIMIT 512).map(|$group| { SELECT media_key, source_path, updated_at, asset_id FROM ONLY $group.id ORDER BY updated_at DESC, asset_id ASC LIMIT 1 });",
                    )
                    .bind(("page_ids", page_ids))
                    .bind(("cover_keys", cover_keys))
                    .await
                    .map_err(|error| format!("query Match page counts: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match assignment counts: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match suggestion counts: {error}"))?,
                response
                    .take(2)
                    .map_err(|error| format!("decode Match cover paths: {error}"))?,
            ))
        })?;
        let assignment_counts = assignment_counts
            .into_iter()
            .filter_map(|row| {
                Some((
                    row.get("person_id")?.as_str()?.to_string(),
                    row.get("count")?.as_u64()?,
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let suggestion_counts = suggestion_counts
            .into_iter()
            .filter_map(|row| {
                Some((
                    row.get("candidate_person_id")?.as_str()?.to_string(),
                    row.get("count")?.as_u64()?,
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let cover_paths = cover_assets
            .into_iter()
            .filter_map(grouped_media_source_path)
            .fold(
                BTreeMap::<String, String>::new(),
                |mut paths, (key, path)| {
                    paths.entry(key).or_insert(path);
                    paths
                },
            );
        let (indexing_started, partial, settled) = self.job_status_unlocked()?;
        let total_people = totals
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rows = people
            .into_iter()
            .map(|person| {
                let cover_source_path = person
                    .cover_media_key
                    .as_ref()
                    .and_then(|key| cover_paths.get(key))
                    .cloned();
                PersonCatalogRow {
                    assigned_face_count: assignment_counts
                        .get(&person.person_id)
                        .copied()
                        .unwrap_or(0),
                    suggestion_count: suggestion_counts
                        .get(&person.person_id)
                        .copied()
                        .unwrap_or(0),
                    person,
                    cover_source_path,
                }
            })
            .collect();
        Ok(MatchCatalogSnapshot {
            total_people,
            evidence: self.catalog_evidence_unlocked(total_people, include_hidden)?,
            offset,
            limit,
            indexing_started,
            partial,
            settled,
            rows,
        })
    }

    pub fn person_gallery(
        &self,
        person_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<PersonGallerySnapshot, String> {
        if limit == 0 || limit > 512 {
            return Err("Match gallery page limit must be between 1 and 512".to_string());
        }
        let _guard = self.database_read_guard("Match gallery snapshot lock is poisoned")?;
        let person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let db = self.database();
        let (media_keys, grouped_count): (Vec<String>, Vec<Value>) = surreal_store::run(
            async move {
                let mut response = db
                .query(
                    "SELECT VALUE media_key FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP BY media_key ORDER BY media_key ASC LIMIT $limit START $offset;\
                     SELECT count() AS count FROM (SELECT media_key FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP BY media_key) GROUP ALL;",
                )
                .bind(("person_id", person_id.to_string()))
                .bind(("limit", limit as u64))
                .bind(("offset", offset as u64))
                .await
                .map_err(|error| format!("query Match Person gallery: {error}"))?;
                Ok((
                    response
                        .take(0)
                        .map_err(|error| format!("decode Match Person gallery page: {error}"))?,
                    response
                        .take(1)
                        .map_err(|error| format!("decode Match Person gallery count: {error}"))?,
                ))
            },
        )?;
        let total_media = grouped_count
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let page_keys = media_keys.clone();
        let db = self.database();
        let page_assets: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "(SELECT id, media_key FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key IN $page_keys GROUP BY media_key ORDER BY media_key ASC LIMIT 512).map(|$group| { SELECT media_key, source_path, updated_at, asset_id FROM ONLY $group.id ORDER BY updated_at DESC, asset_id ASC LIMIT 1 });",
                )
                .bind(("page_keys", page_keys))
                .await
                .map_err(|error| format!("query Match Person gallery paths: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match Person gallery paths: {error}"))
        })?;
        let resolved = page_assets
            .into_iter()
            .filter_map(grouped_media_source_path)
            .fold(
                BTreeMap::<String, String>::new(),
                |mut paths, (key, path)| {
                    paths.entry(key).or_insert(path);
                    paths
                },
            );
        let media_paths = media_keys
            .iter()
            .filter_map(|key| resolved.get(key).cloned())
            .collect::<Vec<_>>();
        Ok(PersonGallerySnapshot {
            person,
            total_media,
            offset,
            limit,
            media_keys,
            media_paths,
        })
    }

    /// Resolve one canonical JobAsset using the migration-backed media index
    /// and the repository-wide authority tie-break documented above.
    pub(crate) fn canonical_job_asset_for_media_unlocked(
        &self,
        media_key: &str,
    ) -> Result<Option<JobAsset>, String> {
        debug_assert_eq!(
            CANONICAL_JOB_ASSET_TIE_BREAK,
            "updated_at DESC, asset_id ASC"
        );
        let db = self.database();
        let key = media_key.to_string();
        let rows: Vec<JobAsset> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key = $media_key ORDER BY updated_at DESC, asset_id ASC LIMIT 1;",
                )
                .bind(("media_key", key))
                .await
                .map_err(|error| format!("query canonical Match JobAsset: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode canonical Match JobAsset: {error}"))
        })?;
        Ok(rows.into_iter().next())
    }

    pub fn canonical_media_key_for_source(
        &self,
        source_path: &Path,
    ) -> Result<Option<String>, String> {
        let _guard = self.database_read_guard("Match source lookup lock is poisoned")?;
        self.canonical_media_key_for_source_unlocked(source_path)
    }

    fn canonical_media_key_for_source_unlocked(
        &self,
        source_path: &Path,
    ) -> Result<Option<String>, String> {
        let candidates = match_source_path_candidates(source_path)?;
        let db = self.database();
        let bound_candidates = candidates.clone();
        let keys: Vec<String> = surreal_store::run(async move {
            let mut response = db.query("SELECT VALUE media_key FROM match_job_asset WITH INDEX match_job_asset_source WHERE source_path IN $source_paths GROUP BY media_key ORDER BY media_key ASC LIMIT 2;")
                .bind(("source_paths", bound_candidates)).await
                .map_err(|error| format!("query canonical Match source: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode canonical Match source: {error}"))
        })?;
        if keys.len() > 1 {
            return Err("match_source_resolution_ambiguous: source belongs to multiple canonical Match media keys".into());
        }
        let Some(key) = keys.into_iter().next() else {
            return Ok(None);
        };
        validate_media_key(&key)?;
        let asset = self
            .canonical_job_asset_for_media_unlocked(&key)?
            .ok_or("match_source_resolution_stale: canonical Match asset disappeared")?;
        if !asset
            .source_path
            .as_ref()
            .is_some_and(|path| candidates.contains(path))
        {
            return Err("match_source_resolution_stale: newest canonical Match asset has a different source".into());
        }
        Ok(Some(key))
    }

    pub(crate) fn active_model_generation_unlocked(&self) -> Result<Option<String>, String> {
        let mut generations = self
            .list_unlocked::<ModelGeneration>(GENERATION_TABLE)?
            .into_iter()
            .filter(|generation| generation.state == "active")
            .collect::<Vec<_>>();
        generations.sort_by(|left, right| left.generation.cmp(&right.generation));
        if generations.len() > 1 {
            return Err("multiple active Match model generations are invalid".to_string());
        }
        Ok(generations
            .pop()
            .filter(|generation| generation.validated)
            .map(|generation| generation.generation))
    }

    /// Resolve the one activation that is allowed to authenticate current
    /// strict-automatic truth. Multiple active rows, a model-generation drift,
    /// failed readiness/review gates, or an integrity mismatch all fail closed
    /// to `None`; callers must never select an arbitrary active activation.
    pub(crate) fn current_active_calibration_unlocked(
        &self,
        active_generation: Option<&str>,
    ) -> Result<Option<CalibrationActivation>, String> {
        let Some(active_generation) = active_generation else {
            return Ok(None);
        };
        let db = self.database();
        let rows: Vec<CalibrationActivation> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_calibration_activation WHERE active = true ORDER BY calibration_generation ASC LIMIT 2;",
                )
                .await
                .map_err(|error| format!("query active Match calibration activation: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode active Match calibration activation: {error}"))
        })?;
        let [activation] = rows.as_slice() else {
            return Ok(None);
        };
        let activation_created_at =
            match chrono::DateTime::parse_from_rfc3339(&activation.created_at) {
                Ok(created_at) => created_at,
                Err(_) => return Ok(None),
            };
        let is_current = activation.model_generation == active_generation
            && activation.verifier_verdict == "pass"
            && activation.independent_review_verdict == "pass"
            && activation.wp084_runtime_ready
            && activation.wp087_release_ready
            && activation.invalidation_reason.is_none()
            && activation.runtime_configuration_digest
                == strict_runtime_configuration_digest(activation.candidate_k, activation.rerank_k)
            && activation.activation_integrity_digest
                == calibration_activation_integrity_digest(activation)
            && activation_created_at <= chrono::Utc::now();
        Ok(is_current.then(|| activation.clone()))
    }

    fn durable_person_cover_exists_unlocked(
        &self,
        person_id: &str,
        media_key: &str,
    ) -> Result<bool, String> {
        let db = self.database();
        let person_id = person_id.to_string();
        let media_key = media_key.to_string();
        surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT count() AS count FROM match_assignment WITH INDEX match_assignment_person
                     WHERE person_id = $person_id AND media_key = $media_key AND state = $state GROUP ALL;",
                )
                .bind(("person_id", person_id))
                .bind(("media_key", media_key))
                .bind(("state", AssignmentState::OperatorConfirmed.as_str()))
                .await
                .map_err(|error| format!("query durable Match Person cover: {error}"))?;
            let rows: Vec<Value> = response
                .take(0)
                .map_err(|error| format!("decode durable Match Person cover: {error}"))?;
            Ok(rows
                .first()
                .and_then(|row| row.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0)
        })
    }

    pub(crate) fn suggestion_provenance_is_current_unlocked(
        &self,
        suggestion: &Suggestion,
        face: &FaceObservation,
        person: &Person,
        active_generation: Option<&str>,
        canonical_asset: Option<&JobAsset>,
        canonical_embedding: Option<&FaceEmbeddingProvenance>,
    ) -> bool {
        let Some(active_generation) = active_generation else {
            return false;
        };
        let Some(asset) = canonical_asset else {
            return false;
        };
        let Some(embedding) = canonical_embedding else {
            return false;
        };
        let Ok(embedding_created_at) = chrono::DateTime::parse_from_rfc3339(&embedding.created_at)
        else {
            return false;
        };
        let Ok(suggestion_created_at) =
            chrono::DateTime::parse_from_rfc3339(&suggestion.created_at)
        else {
            return false;
        };
        suggestion.face_id == face.face_id
            && suggestion.candidate_person_id == person.person_id
            && suggestion.model_generation == active_generation
            && suggestion.media_fingerprint == face.media_fingerprint
            && suggestion.face_revision == face.face_revision
            && suggestion.person_revision == person.revision
            && face.schema_generation == MATCH_SCHEMA_GENERATION
            && asset.job_id == suggestion.job_id
            && asset.media_key == face.media_key
            && asset.media_fingerprint == face.media_fingerprint
            && asset.model_generation == active_generation
            && asset.schema_generation == face.schema_generation
            && embedding.embedding_id
                == embedding_id(&suggestion.face_id, &suggestion.model_generation)
            && embedding.face_id == suggestion.face_id
            && embedding.face_revision == suggestion.face_revision
            && embedding.media_fingerprint == suggestion.media_fingerprint
            && embedding.model_generation == suggestion.model_generation
            && embedding.job_id == suggestion.job_id
            && embedding.schema_generation == face.schema_generation
            && embedding.active
            && embedding_created_at <= suggestion_created_at
    }

    pub(crate) fn strict_assignment_provenance_is_current_unlocked(
        &self,
        assignment: &Assignment,
        face: &FaceObservation,
        person: &Person,
        active_generation: Option<&str>,
        active_calibration: Option<&CalibrationActivation>,
        canonical_asset: Option<&JobAsset>,
        canonical_embedding: Option<&FaceEmbeddingProvenance>,
    ) -> bool {
        let (
            Some(active_generation),
            Some(model_generation),
            Some(calibration_generation),
            Some(envelope_hash),
            Some(calibration),
            Some(asset),
            Some(embedding),
        ) = (
            active_generation,
            assignment.model_generation.as_deref(),
            assignment.calibration_generation.as_deref(),
            assignment.envelope_hash.as_deref(),
            active_calibration,
            canonical_asset,
            canonical_embedding,
        )
        else {
            return false;
        };
        let Ok(embedding_created_at) = chrono::DateTime::parse_from_rfc3339(&embedding.created_at)
        else {
            return false;
        };
        let Ok(assignment_created_at) =
            chrono::DateTime::parse_from_rfc3339(&assignment.created_at)
        else {
            return false;
        };
        let Ok(calibration_created_at) =
            chrono::DateTime::parse_from_rfc3339(&calibration.created_at)
        else {
            return false;
        };
        assignment.state == AssignmentState::CommittedStrictAutomatic.as_str()
            && assignment.face_id == face.face_id
            && assignment.person_id == person.person_id
            && assignment.media_key == face.media_key
            && model_generation == active_generation
            && calibration.active
            && calibration.invalidation_reason.is_none()
            && calibration.model_generation == active_generation
            && calibration.calibration_generation == calibration_generation
            && calibration.envelope_hash == envelope_hash
            && calibration.activation_integrity_digest
                == calibration_activation_integrity_digest(calibration)
            && assignment.face_revision == face.face_revision
            && assignment.person_revision == person.revision
            && face.schema_generation == MATCH_SCHEMA_GENERATION
            && asset.media_key == face.media_key
            && asset.media_fingerprint == face.media_fingerprint
            && asset.model_generation == active_generation
            && asset.schema_generation == face.schema_generation
            && embedding.embedding_id == embedding_id(&assignment.face_id, model_generation)
            && embedding.face_id == assignment.face_id
            && embedding.face_revision == assignment.face_revision
            && embedding.media_fingerprint == face.media_fingerprint
            && embedding.model_generation == model_generation
            && embedding.job_id == asset.job_id
            && embedding.schema_generation == face.schema_generation
            && embedding.active
            && calibration_created_at <= assignment_created_at
            && embedding_created_at <= assignment_created_at
            && assignment_created_at <= chrono::Utc::now()
    }

    // Caller retains the same read guard used to read the canonical count/page.
    fn catalog_evidence_unlocked(
        &self,
        total_people: u64,
        include_hidden: bool,
    ) -> Result<MatchCatalogEvidence, String> {
        Ok(MatchCatalogEvidence {
            total_people,
            count_scope: if include_hidden {
                "canonical_all_people"
            } else {
                "canonical_nonhidden_people"
            },
            catalog_revision: self.execution_state_unlocked()?.catalog_revision,
            schema_generation: MATCH_SCHEMA_GENERATION,
            store_session_id: self.store.session_id().to_string(),
        })
    }

    fn catalog_status(&self) -> Result<(MatchCatalogEvidence, bool, bool, bool), String> {
        let _guard = self.database_read_guard("Match catalog status lock is poisoned")?;
        let db = self.database();
        let totals: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query("SELECT count() AS count FROM match_person WHERE hidden = false GROUP ALL;")
                .await
                .map_err(|error| format!("query Match catalog status: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match catalog status: {error}"))
        })?;
        let total_people = totals
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let (indexing_started, partial, settled) = self.job_status_unlocked()?;
        Ok((
            self.catalog_evidence_unlocked(total_people, false)?,
            indexing_started,
            partial,
            settled,
        ))
    }

    pub fn ui_snapshot(&self, offset: usize, limit: usize) -> Result<Value, String> {
        let catalog = self.catalog_snapshot(offset, limit, false)?;
        let _guard = self.database_read_guard("Match UI snapshot lock is poisoned")?;
        let db = self.database();
        let (suggestions, unidentified, failed_assets): (
            Vec<Suggestion>,
            Vec<String>,
            Vec<JobAsset>,
        ) = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_suggestion ORDER BY similarity DESC, suggestion_id ASC LIMIT 200;\
                     SELECT VALUE media_key FROM match_face_observation WHERE !record::exists(type::record('match_assignment', face_id)) GROUP BY media_key ORDER BY media_key ASC LIMIT 200;\
                     SELECT * OMIT id FROM match_job_asset WHERE failure_code != NONE OR skipped_code != NONE ORDER BY updated_at DESC, asset_id ASC LIMIT 200;",
                )
                .await
                .map_err(|error| format!("query bounded Match UI snapshot: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match suggestions: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match unidentified media: {error}"))?,
                response
                    .take(2)
                    .map_err(|error| format!("decode Match failure rows: {error}"))?,
            ))
        })?;
        drop(_guard);
        Ok(json!({
            "catalog": catalog,
            "suggestions": suggestions,
            "unidentified_media": unidentified,
            "roots": self.index_roots_page(0, 200)?,
            "failed_assets": failed_assets,
            "status": self.status()?,
        }))
    }

    /// Bounded, privacy-redacted state for receipts and no-context model
    /// diagnostics. Filesystem roots, media keys, face identifiers, similarity
    /// values, and failure messages intentionally stay out of this surface.
    pub fn public_snapshot(&self) -> Result<Value, String> {
        if self.database_recovery_pending() {
            return Ok(self.database_recovery_snapshot("database_recovery_pending"));
        }
        self.database_diagnostic_result(self.canonical_public_snapshot())
    }

    fn canonical_public_snapshot(&self) -> Result<Value, String> {
        let (catalog_evidence, indexing_started, partial, settled) = self.catalog_status()?;
        let jobs = self.recent_jobs(200)?;
        let db = self.database();
        let known_codes = FAILURE_CODES
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        let (failure_rows, skipped_rows, unknown_rows, root_rows, stage_rows, progress_rows): (
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
        ) = surreal_store::run(async move {
            let mut response = db
                    .query(
                        "SELECT failure_code, count() AS count FROM match_job_asset WHERE failure_code IN $known_codes GROUP BY failure_code LIMIT 32;\
                         SELECT skipped_code, count() AS count FROM match_job_asset WHERE skipped_code IN $known_codes GROUP BY skipped_code LIMIT 32;\
                         SELECT count() AS count FROM match_job_asset WHERE (failure_code != NONE AND failure_code NOT IN $known_codes) OR (skipped_code != NONE AND skipped_code NOT IN $known_codes) GROUP ALL;\
                         SELECT count() AS count FROM match_index_root GROUP ALL;\
                         SELECT next_stage, count() AS count FROM match_job_asset GROUP BY next_stage ORDER BY next_stage ASC LIMIT 8;\
                         SELECT math::sum(discovered) AS discovered, math::sum(completed) AS completed, math::sum(failed) AS failed, math::sum(skipped) AS skipped FROM match_index_job GROUP ALL;",
                    )
                    .bind(("known_codes", known_codes))
                    .await
                    .map_err(|error| format!("query Match public failure counts: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Match failure counts: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode Match skipped counts: {error}"))?,
                response
                    .take(2)
                    .map_err(|error| format!("decode Match unknown failure count: {error}"))?,
                response
                    .take(3)
                    .map_err(|error| format!("decode Match root count: {error}"))?,
                response
                    .take(4)
                    .map_err(|error| format!("decode Match persisted stage counts: {error}"))?,
                response
                    .take(5)
                    .map_err(|error| format!("decode Match canonical job progress: {error}"))?,
            ))
        })?;
        let mut failed_assets = BTreeMap::<String, u64>::new();
        for (rows, field) in [
            (failure_rows, "failure_code"),
            (skipped_rows, "skipped_code"),
        ] {
            for row in rows {
                let code = redacted_failure_code(
                    row.get(field).and_then(Value::as_str).unwrap_or("unknown"),
                )
                .to_string();
                let count = row.get("count").and_then(Value::as_u64).unwrap_or(0);
                *failed_assets.entry(code).or_default() += count;
            }
        }
        let unknown_count = unknown_rows
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unknown_count > 0 {
            *failed_assets.entry("unknown".to_string()).or_default() += unknown_count;
        }
        let resource_telemetry = self.governor.telemetry()?;
        let mut stage_counts = BTreeMap::<String, u64>::new();
        for row in stage_rows {
            let stage = row
                .get("next_stage")
                .and_then(Value::as_str)
                .ok_or("Match persisted stage is malformed")?;
            JobStage::parse(stage).map_err(|_| "Match persisted stage is unknown".to_string())?;
            let count = row
                .get("count")
                .and_then(Value::as_u64)
                .ok_or("Match persisted stage count is malformed")?;
            if stage_counts.insert(stage.to_string(), count).is_some() {
                return Err("Match persisted stage count is duplicated".into());
            }
        }
        let index_stage = stage_counts
            .iter()
            .map(|(stage, count)| format!("{stage}:{count}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut job_progress = BTreeMap::new();
        for field in ["discovered", "completed", "failed", "skipped"] {
            let count = match progress_rows.first() {
                None => 0,
                Some(row) => row
                    .get(field)
                    .and_then(Value::as_u64)
                    .ok_or("Match canonical job progress count is malformed")?,
            };
            job_progress.insert(field, count);
        }
        Ok(json!({
            "catalog": {
                "total_people": catalog_evidence.total_people,
                "evidence": catalog_evidence,
                "indexing_started": indexing_started,
                "partial": partial,
                "settled": settled,
                "materialized_people_rows": 0,
            },
            "execution": {
                "desired_mode": self.desired_mode()?,
                "database_owner": self.store.owner_diagnostics(),
                "pending_failure_records": self.pending_database_failure_snapshot(),
                "pending_failure_records_capacity": 16,
                "pending_failure_records_evicted": self.pending_database_failures_evicted.load(Ordering::Acquire),
                "pending_failure_records_authoritative": false,
                "holds": self.holds()?,
                "resource_usage": resource_telemetry.current_usage,
                "resource_budget": self.governor.budget(),
                "resource_telemetry": resource_telemetry,
                "index_stage": index_stage,
                "index_stage_counts": stage_counts,
                "index_stage_scope": "persisted_asset_next_stage_counts",
            },
            "roots": { "configured": root_rows.first().and_then(|row| row.get("count")).and_then(Value::as_u64).unwrap_or(0) },
            "jobs": jobs.into_iter().map(|job| json!({
                "job_id": job.job_id,
                "lifecycle": job.lifecycle,
                "discovered": job.discovered,
                "completed": job.completed,
                "failed": job.failed,
                "skipped": job.skipped,
                "failure_code": job.failure_code.as_deref().map(redacted_failure_code),
            })).collect::<Vec<_>>(),
            "failure_codes": failed_assets,
            "job_progress": job_progress,
            "job_progress_scope": "canonical_all_index_jobs",
            "privacy": {
                "paths": false,
                "media_keys": false,
                "face_ids": false,
                "similarities": false,
                "failure_messages": false,
                "embeddings": false,
            }
        }))
    }

    /// Operator-only Settings projection. It intentionally contains no
    /// Person rows, cover media, catalog counts, or gallery projection.
    pub fn settings_snapshot(&self) -> Result<Value, String> {
        self.settings_snapshot_page(0, 200)
    }

    pub fn settings_snapshot_page(&self, offset: usize, limit: usize) -> Result<Value, String> {
        if limit == 0 || limit > 512 {
            return Err("Match Settings page limit must be between 1 and 512".to_string());
        }
        let jobs = self.recent_jobs_page(offset, limit)?;
        let roots = self.index_roots_page(offset, limit)?;
        let db = self.database();
        let (failed_assets, totals): (Vec<JobAsset>, Vec<Value>) = surreal_store::run(
            async move {
                let mut response = db
                .query("SELECT * OMIT id FROM match_job_asset WHERE failure_code != NONE OR skipped_code != NONE ORDER BY updated_at DESC, asset_id ASC LIMIT $limit START $offset;\
                        SELECT count() AS roots FROM match_index_root GROUP ALL;\
                        SELECT count() AS jobs FROM match_index_job GROUP ALL;\
                        SELECT count() AS failures FROM match_job_asset WHERE failure_code != NONE OR skipped_code != NONE GROUP ALL;")
                .bind(("limit", limit as u64))
                .bind(("offset", offset as u64))
                .await
                .map_err(|error| format!("query Match Settings failure rows: {error}"))?;
                let failed_assets = response
                    .take(0)
                    .map_err(|error| format!("decode Match Settings failure rows: {error}"))?;
                let roots: Vec<Value> = response
                    .take(1)
                    .map_err(|error| format!("decode Match Settings root count: {error}"))?;
                let jobs: Vec<Value> = response
                    .take(2)
                    .map_err(|error| format!("decode Match Settings job count: {error}"))?;
                let failures: Vec<Value> = response
                    .take(3)
                    .map_err(|error| format!("decode Match Settings failure count: {error}"))?;
                Ok((
                    failed_assets,
                    vec![
                        roots.into_iter().next().unwrap_or_default(),
                        jobs.into_iter().next().unwrap_or_default(),
                        failures.into_iter().next().unwrap_or_default(),
                    ],
                ))
            },
        )?;
        let failed_assets = failed_assets
            .into_iter()
            .map(|asset| {
                json!({
                    "asset_id": asset.asset_id,
                    "job_id": asset.job_id,
                    "media_key": asset.media_key,
                    "source_path": asset.source_path,
                    "failure_code": asset.failure_code,
                    "failure_message": asset.failure_message,
                    "skipped_code": asset.skipped_code,
                    "skipped_message": asset.skipped_message,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "execution": {
                "desired_mode": self.desired_mode()?,
                "holds": self.holds()?,
            },
            "roots": roots,
            "jobs": jobs,
            "failed_assets": failed_assets,
            "materialized_people_rows": 0,
            "page": {
                "offset": offset,
                "limit": limit,
                "total_roots": totals[0].get("roots").and_then(Value::as_u64).unwrap_or(0),
                "total_jobs": totals[1].get("jobs").and_then(Value::as_u64).unwrap_or(0),
                "total_failures": totals[2].get("failures").and_then(Value::as_u64).unwrap_or(0),
            },
        }))
    }

    pub fn update_person(
        &self,
        person_id: &str,
        expected_revision: u64,
        name: &str,
        aliases: Vec<String>,
    ) -> Result<Person, String> {
        validate_text("Person name", name)?;
        let mut aliases = aliases
            .into_iter()
            .map(|alias| alias.trim().to_string())
            .filter(|alias| !alias.is_empty())
            .collect::<Vec<_>>();
        aliases.sort_by_key(|alias| alias.to_lowercase());
        aliases.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        for alias in &aliases {
            validate_text("Person alias", alias)?;
        }
        let _guard = self.mutation_write_guard("Match Person update")?;
        let mut person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        if person.revision != expected_revision {
            return Err("stale Person revision".to_string());
        }
        self.invalidate_calibrations_unlocked("person_updated")?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, false, true)?;
        person.name = name.trim().to_string();
        person.aliases = aliases;
        person.revision = person
            .revision
            .checked_add(1)
            .ok_or("Person revision overflow")?;
        person.catalog_revision = execution.catalog_revision;
        person.updated_at = now();
        if let Some(cover_media_key) = person.cover_media_key.as_deref() {
            if !self.durable_person_cover_exists_unlocked(&person.person_id, cover_media_key)? {
                person.cover_media_key = None;
            }
        }
        self.commit_person_revision_change_unlocked(&person, &execution)?;
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(person)
    }

    pub fn create_look(&self, person_id: &str, name: &str) -> Result<Look, String> {
        validate_text("Look name", name)?;
        let _guard = self.mutation_write_guard("Match Look creation")?;
        let _: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        self.invalidate_calibrations_unlocked("look_created")?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let now = now();
        let look = Look {
            look_id: new_id("look"),
            person_id: person_id.to_string(),
            name: name.trim().to_string(),
            revision: 1,
            created_at: now.clone(),
            updated_at: now,
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    LOOK_TABLE,
                    &look.look_id,
                    serde_json::to_value(&look).map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        Ok(look)
    }

    pub fn create_template_set(
        &self,
        look_id: &str,
        name: &str,
    ) -> Result<TrustedTemplateSet, String> {
        validate_text("TrustedTemplateSet name", name)?;
        let _guard = self.mutation_write_guard("Match template-set creation")?;
        let _: Look = self.require_unlocked(LOOK_TABLE, look_id, "Look")?;
        self.invalidate_calibrations_unlocked("template_set_created")?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let now = now();
        let set = TrustedTemplateSet {
            set_id: new_id("template_set"),
            look_id: look_id.to_string(),
            name: name.trim().to_string(),
            revision: 1,
            created_at: now.clone(),
            updated_at: now,
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    TEMPLATE_SET_TABLE,
                    &set.set_id,
                    serde_json::to_value(&set).map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        Ok(set)
    }

    pub fn create_face(&self, face: FaceObservation) -> Result<FaceObservation, String> {
        validate_face(&face)?;
        if face.schema_generation != MATCH_SCHEMA_GENERATION {
            return Err("stale schema generation".to_string());
        }
        if !face.operator_owned {
            return Err("derived face creation requires an IndexJob revision fence".to_string());
        }
        let _guard = self.mutation_write_guard("Match manual face creation")?;
        if self
            .get_one_unlocked::<FaceObservation>(FACE_TABLE, &face.face_id)?
            .is_some()
        {
            return Err("manual face id already exists".to_string());
        }
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    FACE_TABLE,
                    &face.face_id,
                    serde_json::to_value(&face).map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        Ok(face)
    }

    pub fn create_derived_face(
        &self,
        mut face: FaceObservation,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<FaceObservation, String> {
        let mut stage_use = stage_permit.consume_for(&self.session_id, fence, JobStage::Detect)?;
        validate_face(&face)?;
        if face.operator_owned {
            return Err("operator-owned face creation must not use an IndexJob fence".to_string());
        }
        let supplied_payload_bytes = serde_json::to_vec(&face)
            .map_err(|error| error.to_string())?
            .len();
        let supplied_face = face.clone();
        face.face_id = derived_face_id(
            &face.media_key,
            &face.media_fingerprint,
            face.source_index,
            &face.schema_generation,
        );
        let canonical_payload_bytes = serde_json::to_vec(&face)
            .map_err(|error| error.to_string())?
            .len();
        stage_permit.authorize_payload(supplied_payload_bytes.max(canonical_payload_bytes))?;
        if face.media_key != fence.media_key
            || face.media_fingerprint != fence.media_fingerprint
            || face.schema_generation != fence.schema_generation
        {
            return Err("stale derived-face revision fence".to_string());
        }
        let _guard = self.mutation_write_guard("Match derived-face")?;
        let asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if asset.next_stage()? != JobStage::Detect {
            return Err(
                "derived-face write no longer matches the durable stage cursor".to_string(),
            );
        }
        if let Some(existing) = self
            .list_unlocked::<FaceObservation>(FACE_TABLE)?
            .into_iter()
            .find(|existing| {
                existing.media_key == face.media_key
                    && existing.media_fingerprint == face.media_fingerprint
                    && existing.source_index == face.source_index
                    && existing.schema_generation == face.schema_generation
            })
        {
            if same_face_payload(&existing, &face) || same_face_payload(&existing, &supplied_face) {
                stage_use.success();
                return Ok(existing);
            }
            return Err("canonical derived face retry changed its persisted payload".to_string());
        }
        if let Some(existing) =
            self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &face.face_id)?
        {
            if same_face_payload(&existing, &face) {
                stage_use.success();
                return Ok(existing);
            }
            return Err("canonical derived face retry changed its persisted payload".to_string());
        }
        self.transactional_upserts_deletes_unlocked(
            &[(
                FACE_TABLE,
                &face.face_id,
                serde_json::to_value(&face).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        stage_use.success();
        Ok(face)
    }

    /// Preserve persisted fingerprint encoding (and therefore derived FaceIds)
    /// after a byte-identical historical source is observed again.
    pub fn observed_media_fingerprint(
        &self,
        media_key: &str,
        observed: &str,
    ) -> Result<Option<String>, String> {
        let _database_unit = self.begin_database_unit()?;
        let digest = canonical_media_sha256(observed)
            .ok_or("observed media fingerprint must contain a lowercase SHA-256 digest")?;
        validate_media_key(media_key)?;
        let _guard = self.database_read_guard("media fingerprint evidence lock is poisoned")?;
        #[derive(Deserialize, SurrealValue)]
        struct FingerprintRow {
            media_fingerprint: String,
            operator_owned: bool,
        }
        let db = self.database();
        let key = media_key.to_string();
        let fingerprint_encodings = vec![digest.to_string(), format!("sha256:{digest}")];
        let (faces, assets): (Vec<FingerprintRow>, Vec<Value>) = surreal_store::run(async move {
            let mut response = db.query(
                "SELECT media_fingerprint, operator_owned FROM match_face_observation WITH INDEX match_face_media WHERE media_key = $media_key AND media_fingerprint IN $fingerprint_encodings GROUP BY media_fingerprint, operator_owned LIMIT 5; \
                 SELECT media_fingerprint, updated_at, asset_id FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key = $media_key ORDER BY updated_at DESC, asset_id ASC LIMIT 1;"
            ).bind(("media_key", key)).bind(("fingerprint_encodings", fingerprint_encodings)).await
                .map_err(|error| format!("query media fingerprint evidence: {error}"))?;
            Ok((
                response
                    .take(0)
                    .map_err(|error| format!("decode Face fingerprint evidence: {error}"))?,
                response
                    .take(1)
                    .map_err(|error| format!("decode JobAsset fingerprint evidence: {error}"))?,
            ))
        })?;
        if faces.len() > 4 {
            return Err(
                "media fingerprint encoding groups exceed the exact compatibility bound"
                    .to_string(),
            );
        }
        let encodings = faces
            .iter()
            .filter(|row| !row.operator_owned)
            .filter(|row| canonical_media_sha256(&row.media_fingerprint) == Some(digest))
            .map(|row| row.media_fingerprint.clone())
            .collect::<BTreeSet<_>>();
        if encodings.len() > 1 {
            return Err("ambiguous persisted derived Face fingerprint encodings".to_string());
        }
        if let Some(encoding) = encodings.into_iter().next() {
            return Ok(Some(encoding));
        }
        let manual_encodings = faces
            .iter()
            .filter(|row| canonical_media_sha256(&row.media_fingerprint) == Some(digest))
            .map(|row| row.media_fingerprint.clone())
            .collect::<BTreeSet<_>>();
        if manual_encodings.len() > 1 {
            return Err("ambiguous persisted manual Face fingerprint encodings".to_string());
        }
        if let Some(encoding) = manual_encodings.into_iter().next() {
            return Ok(Some(encoding));
        }
        Ok(assets
            .first()
            .and_then(|row| row.get("media_fingerprint"))
            .and_then(Value::as_str)
            .filter(|fingerprint| canonical_media_sha256(fingerprint) == Some(digest))
            .map(str::to_string))
    }

    /// Bounded lookup for committed fused faces when a restart resumes Suggest.
    pub fn derived_faces_for_asset(
        &self,
        asset: &JobAsset,
    ) -> Result<Vec<FaceObservation>, String> {
        let _database_unit = self.begin_database_unit()?;
        let media_key = asset.media_key.clone();
        let media_fingerprint = asset.media_fingerprint.clone();
        let schema_generation = asset.schema_generation.clone();
        let db = self.database();
        surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_face_observation \
                     WHERE media_key = $media_key \
                     AND media_fingerprint = $media_fingerprint \
                     AND schema_generation = $schema_generation \
                     AND operator_owned = false \
                     ORDER BY source_index ASC LIMIT 4096;",
                )
                .bind(("media_key", media_key))
                .bind(("media_fingerprint", media_fingerprint))
                .bind(("schema_generation", schema_generation))
                .await
                .map_err(|error| {
                    format!("query committed faces for Match Suggest resume: {error}")
                })?;
            response.take(0).map_err(|error| {
                format!("decode committed faces for Match Suggest resume: {error}")
            })
        })
    }

    /// Atomically publishes the complete fused identity-kernel result and
    /// advances Detect -> Align -> Embed in one durable transaction. The
    /// identity engine computes these outputs as one fused call, so exposing
    /// intermediate durable cursors would make a restart replay work already
    /// claimed complete. Holding and consuming all three stage permits keeps
    /// resource accounting honest while the atomic cursor prevents replay.
    pub fn commit_fused_identity_inference(
        &self,
        mut faces: Vec<FaceObservation>,
        mut embeddings: Vec<FaceEmbedding>,
        fence: &RevisionFence,
        detect_permit: &MatchStagePermit,
        align_permit: &MatchStagePermit,
        embed_permit: &MatchStagePermit,
    ) -> Result<Vec<FaceObservation>, String> {
        let mut detect_use =
            detect_permit.consume_for(&self.session_id, fence, JobStage::Detect)?;
        let mut align_use = align_permit.consume_for(&self.session_id, fence, JobStage::Align)?;
        let mut embed_use = embed_permit.consume_for(&self.session_id, fence, JobStage::Embed)?;
        if faces.is_empty() || faces.len() != embeddings.len() {
            return Err("fused Match inference requires one embedding per face".to_string());
        }
        for face in &mut faces {
            if face.operator_owned {
                return Err("fused Match inference cannot publish operator-owned faces".to_string());
            }
            face.face_id = derived_face_id(
                &face.media_key,
                &face.media_fingerprint,
                face.source_index,
                &face.schema_generation,
            );
            validate_face(face)?;
            if face.media_key != fence.media_key
                || face.media_fingerprint != fence.media_fingerprint
                || face.schema_generation != fence.schema_generation
            {
                return Err("stale fused derived-face revision fence".to_string());
            }
        }
        validate_fused_publication_uniqueness(&faces, &embeddings)?;
        for (face, embedding) in faces.iter().zip(&embeddings) {
            validate_vector(&embedding.vector)?;
            if embedding.embedding_id
                != embedding_id(&embedding.face_id, &embedding.model_generation)
                || embedding.face_id != face.face_id
                || embedding.schema_generation != face.schema_generation
                || embedding.media_fingerprint != face.media_fingerprint
                || embedding.face_revision != face.face_revision
            {
                return Err("fused Match embedding does not match its canonical face".to_string());
            }
        }
        let face_payload = serde_json::to_vec(&faces).map_err(|error| error.to_string())?;
        let embedding_payload =
            serde_json::to_vec(&embeddings).map_err(|error| error.to_string())?;
        detect_permit.authorize_payload(face_payload.len())?;
        align_permit.authorize_payload(face_payload.len())?;
        embed_permit.authorize_payload(embedding_payload.len())?;

        let _guard = self.mutation_write_guard("Match fused inference")?;
        let mut asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if asset.next_stage()? != JobStage::Detect {
            return Err("fused Match inference no longer owns the Detect cursor".to_string());
        }
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        for (face, embedding) in faces.iter().zip(&embeddings) {
            validate_embedding_job_provenance(embedding, face, &job, &asset, fence)?;
        }
        let generation: ModelGeneration =
            self.require_unlocked(GENERATION_TABLE, &job.model_generation, "model generation")?;
        if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
            return Err("fused Match inference generation is not usable".to_string());
        }
        let mut suppressed_face_ids = BTreeSet::new();
        for face in &faces {
            if self
                .get_one_unlocked::<Value>(FACE_DISPOSITION_TABLE, &face.face_id)?
                .is_some()
            {
                suppressed_face_ids.insert(face.face_id.clone());
            }
        }
        embeddings.retain(|embedding| !suppressed_face_ids.contains(&embedding.face_id));
        for face in &mut faces {
            if let Some(existing) =
                self.get_one_unlocked::<FaceObservation>(FACE_TABLE, &face.face_id)?
            {
                let prior_embedding = self.get_one_unlocked::<FaceEmbedding>(
                    EMBEDDING_TABLE,
                    &embedding_id(&face.face_id, &job.model_generation),
                )?;
                if !same_face_payload(&existing, face)
                    && !can_reobserve_face_for_new_job(
                        &existing,
                        face,
                        &job,
                        prior_embedding.as_ref(),
                    )
                {
                    return Err(
                        "canonical fused face retry changed its persisted payload".to_string()
                    );
                }
                // Keep the byte-for-byte durable value as the transaction
                // input. This prevents an accepted retry from becoming a
                // rewrite if serialization defaults evolve later.
                *face = existing;
            }
        }
        for embedding in &embeddings {
            if let Some(existing) =
                self.get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &embedding.embedding_id)?
            {
                canonical_embedding_retry_disposition(&existing, embedding).map_err(|_| {
                    "canonical fused embedding retry changed its persisted payload".to_string()
                })?;
            }
        }

        let had_failure = asset.failure_code.is_some();
        let had_skip = asset.skipped_code.is_some();
        for stage in [JobStage::Detect, JobStage::Align, JobStage::Embed] {
            if !asset
                .completed_stages
                .iter()
                .any(|completed| completed == stage.as_str())
            {
                asset.completed_stages.push(stage.as_str().to_string());
            }
        }
        asset.next_stage = JobStage::Persist.as_str().to_string();
        asset.failure_code = None;
        asset.failure_message = None;
        asset.skipped_code = None;
        asset.skipped_message = None;
        asset.updated_at = now();
        if had_failure {
            job.failed = job.failed.saturating_sub(1);
        }
        if had_skip {
            job.skipped = job.skipped.saturating_sub(1);
        }
        job.updated_at = now();

        let face_values = faces
            .iter()
            .map(|face| {
                serde_json::to_value(face)
                    .map(|value| (FACE_TABLE, face.face_id.as_str(), value))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let embedding_values = embeddings
            .iter()
            .map(|embedding| {
                serde_json::to_value(embedding)
                    .map(|value| (EMBEDDING_TABLE, embedding.embedding_id.as_str(), value))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let asset_value = serde_json::to_value(&asset).map_err(|error| error.to_string())?;
        let job_value = serde_json::to_value(&job).map_err(|error| error.to_string())?;
        let mut upserts = Vec::with_capacity(face_values.len() + embedding_values.len() + 2);
        upserts.extend(face_values);
        upserts.extend(embedding_values);
        upserts.push((JOB_ASSET_TABLE, asset.asset_id.as_str(), asset_value));
        upserts.push((JOB_TABLE, job.job_id.as_str(), job_value));
        self.transactional_upserts_deletes_unlocked(&upserts, &[])?;
        detect_use.success();
        align_use.success();
        embed_use.success();
        Ok(faces)
    }

    pub fn put_embedding(
        &self,
        embedding: FaceEmbedding,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<(), String> {
        let mut stage_use = stage_permit.consume_for(&self.session_id, fence, JobStage::Embed)?;
        validate_vector(&embedding.vector)?;
        stage_permit.authorize_payload(
            serde_json::to_vec(&embedding)
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        if embedding.embedding_id != embedding_id(&embedding.face_id, &embedding.model_generation) {
            return Err("embedding id is not canonical for face/model generation".to_string());
        }
        let _guard = self.mutation_write_guard("Match embedding")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, &embedding.face_id, "FaceObservation")?;
        if self
            .get_one_unlocked::<Value>(FACE_DISPOSITION_TABLE, &embedding.face_id)?
            .is_some()
        {
            return Err("suppressed face must not receive an automatic embedding".to_string());
        }
        if embedding.schema_generation != MATCH_SCHEMA_GENERATION
            || embedding.schema_generation != face.schema_generation
            || embedding.media_fingerprint != face.media_fingerprint
            || embedding.face_revision != face.face_revision
        {
            return Err("stale embedding revision fence".to_string());
        }
        let generation: ModelGeneration = self.require_unlocked(
            GENERATION_TABLE,
            &embedding.model_generation,
            "model generation",
        )?;
        if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
            return Err("embedding generation is not validated and usable".to_string());
        }
        let (job, asset) =
            self.require_current_job_asset_unlocked(&embedding.job_id, &face.media_key, false)?;
        validate_asset_fence(&asset, fence)?;
        if asset.next_stage()? != JobStage::Embed {
            return Err("embedding write no longer matches the durable stage cursor".to_string());
        }
        if job.model_generation != embedding.model_generation
            || job.schema_generation != embedding.schema_generation
            || asset.media_fingerprint != embedding.media_fingerprint
            || asset.model_generation != embedding.model_generation
            || asset.schema_generation != embedding.schema_generation
        {
            return Err("stale embedding job revision fence".to_string());
        }
        validate_embedding_job_provenance(&embedding, &face, &job, &asset, fence)?;
        if let Some(existing) =
            self.get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &embedding.embedding_id)?
        {
            match canonical_embedding_retry_disposition(&existing, &embedding).map_err(|_| {
                "canonical embedding retry changed its persisted payload".to_string()
            })? {
                EmbeddingRetryDisposition::Exact => {
                    self.reconcile_trusted_search_unlocked()?;
                    stage_use.success();
                    return Ok(());
                }
                EmbeddingRetryDisposition::RefreshCurrentJob => {
                    self.transactional_upserts_deletes_unlocked(
                        &[(
                            EMBEDDING_TABLE,
                            &embedding.embedding_id,
                            serde_json::to_value(&embedding).map_err(|error| error.to_string())?,
                        )],
                        &[],
                    )?;
                    self.reconcile_trusted_search_unlocked()?;
                    stage_use.success();
                    return Ok(());
                }
            }
        }
        self.transactional_upserts_deletes_unlocked(
            &[(
                EMBEDDING_TABLE,
                &embedding.embedding_id,
                serde_json::to_value(&embedding).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        self.reconcile_trusted_search_unlocked()?;
        stage_use.success();
        Ok(())
    }

    /// Rebuild the derived trusted-only ANN source from durable authorization
    /// plus currently valid embeddings. Invalid or stale memberships remain as
    /// operator history but are excluded from search. Any changed derived set
    /// invalidates strict calibration before publication.
    pub fn reconcile_trusted_search(&self) -> Result<usize, String> {
        let _guard = self.mutation_write_guard("Match trusted-search reconciliation")?;
        self.reconcile_trusted_search_unlocked()
    }

    fn reconcile_trusted_search_unlocked(&self) -> Result<usize, String> {
        #[cfg(test)]
        if self
            .trusted_search_reconcile_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err("injected Match trusted-search reconciliation failure".to_string());
        }
        let memberships = self.list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?;
        let assignments = self.list_unlocked::<Assignment>(ASSIGNMENT_TABLE)?;
        let faces = self.list_unlocked::<FaceObservation>(FACE_TABLE)?;
        let embeddings = self.list_unlocked::<FaceEmbedding>(EMBEDDING_TABLE)?;
        let generations = self.list_unlocked::<ModelGeneration>(GENERATION_TABLE)?;
        let mut desired = Vec::new();
        for membership in memberships {
            let assignment = assignments.iter().find(|assignment| {
                assignment.face_id == membership.face_id
                    && assignment.state == AssignmentState::OperatorConfirmed.as_str()
                    && assignment.look_id.as_deref() == Some(membership.look_id.as_str())
            });
            let face = faces.iter().find(|face| face.face_id == membership.face_id);
            let embedding = embeddings.iter().find(|embedding| {
                embedding.embedding_id == membership.embedding_id
                    && embedding.face_id == membership.face_id
                    && embedding.model_generation == membership.model_generation
                    && embedding.active
            });
            let generation_ready = generations.iter().any(|generation| {
                generation.generation == membership.model_generation
                    && generation.validated
                    && matches!(generation.state.as_str(), "usable" | "active")
            });
            if let (Some(assignment), Some(face), Some(embedding)) = (assignment, face, embedding) {
                if membership.authorized
                    && membership.alignment_valid
                    && membership.quality_passed
                    && membership.pose_passed
                    && membership.diversity_passed
                    && membership.policy_version == TRUSTED_POLICY_VERSION
                    && face.alignment_valid
                    && face.quality >= membership.quality_threshold
                    && face.media_fingerprint == membership.media_fingerprint
                    && face.face_revision == membership.face_revision
                    && embedding.media_fingerprint == face.media_fingerprint
                    && embedding.face_revision == face.face_revision
                    && generation_ready
                {
                    desired.push(TrustedSearchEmbedding {
                        membership_id: membership.membership_id,
                        person_id: assignment.person_id.clone(),
                        look_id: membership.look_id,
                        face_id: face.face_id.clone(),
                        embedding_id: embedding.embedding_id.clone(),
                        vector: embedding.vector.clone(),
                        model_generation: embedding.model_generation.clone(),
                        created_at: membership.created_at,
                    });
                }
            }
        }
        desired.sort_by(|left, right| left.membership_id.cmp(&right.membership_id));
        let mut current = self.list_unlocked::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)?;
        current.sort_by(|left, right| left.membership_id.cmp(&right.membership_id));
        let expected_build_digest = trusted_index_build_digest(&desired)?;
        let receipt =
            self.get_one_unlocked::<TrustedIndexBuildReceipt>(TRUSTED_INDEX_BUILD_TABLE, "global")?;
        if current == desired
            && receipt.as_ref().is_some_and(|receipt| {
                receipt.build_digest == expected_build_digest
                    && receipt.source_row_count == desired.len()
                    && receipt.engine_version == STRICT_ANN_ENGINE_VERSION
                    && receipt.build_seed == 0
                    && receipt.build_order == STRICT_ANN_BUILD_ORDER
            })
        {
            return Ok(desired.len());
        }
        self.invalidate_calibrations_unlocked("trusted_search_reconciled")?;
        self.replace_trusted_search_index_unlocked(&desired)?;
        Ok(desired.len())
    }

    /// Replace the complete derived source in canonical membership-id order,
    /// then recreate the HNSW index under the process-pinned build seed. This
    /// makes the graph independent of authorization history.
    fn replace_trusted_search_index_unlocked(
        &self,
        desired: &[TrustedSearchEmbedding],
    ) -> Result<(), String> {
        let mut desired = desired.to_vec();
        desired.sort_by(|left, right| left.membership_id.cmp(&right.membership_id));
        let mut sql = format!("BEGIN TRANSACTION;\nDELETE {TRUSTED_SEARCH_TABLE};\n");
        for index in 0..desired.len() {
            sql.push_str(&format!(
                "UPSERT type::record('{TRUSTED_SEARCH_TABLE}', $membership_id_{index}) CONTENT $membership_{index};\n"
            ));
        }
        sql.push_str("COMMIT TRANSACTION;");
        let db = self.database();
        let mut query = db.query(sql);
        for (index, row) in desired.iter().enumerate() {
            query = query
                .bind((format!("membership_id_{index}"), row.membership_id.clone()))
                .bind((
                    format!("membership_{index}"),
                    serde_json::to_value(row).map_err(|error| error.to_string())?,
                ));
        }
        surreal_store::run(async move {
            query
                .await
                .map_err(|error| format!("replace trusted Match index source: {error}"))?
                .check()
                .map_err(|error| format!("replace trusted Match index source: {error}"))?;
            Ok(())
        })?;
        let db = self.database();
        surreal_store::run(async move {
            db.query(
                "REMOVE INDEX match_trusted_search_hnsw_v1 ON TABLE match_trusted_search_embedding;\n\
                 DEFINE INDEX OVERWRITE match_trusted_search_hnsw_v1 ON TABLE match_trusted_search_embedding FIELDS vector HNSW DIMENSION 512 DIST COSINE TYPE F32 EFC 150 M 12;",
            )
            .await
            .map_err(|error| format!("rebuild trusted Match HNSW index: {error}"))?
            .check()
            .map_err(|error| format!("rebuild trusted Match HNSW index: {error}"))?;
            Ok(())
        })?;
        let build_digest = trusted_index_build_digest(&desired)?;
        let receipt = TrustedIndexBuildReceipt {
            receipt_id: "global".to_string(),
            build_digest,
            source_row_count: desired.len(),
            engine_version: STRICT_ANN_ENGINE_VERSION.to_string(),
            build_seed: 0,
            build_order: STRICT_ANN_BUILD_ORDER.to_string(),
            created_at: now(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                TRUSTED_INDEX_BUILD_TABLE,
                "global",
                serde_json::to_value(receipt).map_err(|error| error.to_string())?,
            )],
            &[],
        )
    }

    pub fn record_suggestion(
        &self,
        suggestion: Suggestion,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<(), String> {
        let mut stage_use = stage_permit.consume_for(&self.session_id, fence, JobStage::Suggest)?;
        if !suggestion.similarity.is_finite() {
            return Err("suggestion similarity must be finite".to_string());
        }
        if suggestion.suggestion_id
            != suggestion_id(&suggestion.face_id, &suggestion.candidate_person_id)
        {
            return Err("suggestion id is not the canonical face/Person pair id".to_string());
        }
        stage_permit.authorize_payload(
            serde_json::to_vec(&suggestion)
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        let _guard = self.mutation_write_guard("Match suggestion")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, &suggestion.face_id, "FaceObservation")?;
        if self
            .get_one_unlocked::<Value>(FACE_DISPOSITION_TABLE, &suggestion.face_id)?
            .is_some()
        {
            return Err("suppressed face must not receive an automatic suggestion".to_string());
        }
        let person: Person =
            self.require_unlocked(PERSON_TABLE, &suggestion.candidate_person_id, "Person")?;
        let generation: ModelGeneration = self.require_unlocked(
            GENERATION_TABLE,
            &suggestion.model_generation,
            "model generation",
        )?;
        let (job, asset) =
            self.require_current_job_asset_unlocked(&suggestion.job_id, &face.media_key, true)?;
        validate_asset_fence(&asset, fence)?;
        if asset.next_stage()? != JobStage::Suggest {
            return Err("suggestion write no longer matches the durable stage cursor".to_string());
        }
        if face.face_revision != suggestion.face_revision
            || face.media_fingerprint != suggestion.media_fingerprint
            || person.revision != suggestion.person_revision
            || !generation.validated
            || !matches!(generation.state.as_str(), "usable" | "active")
            || job.model_generation != suggestion.model_generation
            || job.schema_generation != face.schema_generation
            || asset.media_fingerprint != suggestion.media_fingerprint
            || asset.model_generation != suggestion.model_generation
            || asset.schema_generation != face.schema_generation
        {
            return Err("stale suggestion revision fence".to_string());
        }
        if self
            .get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, &suggestion.face_id)?
            .is_some()
        {
            return Err("suggestion cannot create or replace a committed assignment".to_string());
        }
        if self
            .get_one_unlocked::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &cannot_link_id(&suggestion.face_id, &suggestion.candidate_person_id),
            )?
            .is_some()
        {
            return Err("cannot-linked Person cannot be suggested for this face".to_string());
        }
        self.transactional_upserts_deletes_unlocked(
            &[(
                SUGGESTION_TABLE,
                &suggestion.suggestion_id,
                serde_json::to_value(&suggestion).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        stage_use.success();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn assign_face(
        &self,
        face_id: &str,
        person_id: &str,
        look_id: Option<&str>,
        state: AssignmentState,
        provenance: &str,
        model_generation: Option<&str>,
        calibration_generation: Option<&str>,
        envelope_hash: Option<&str>,
        operator_fence: Option<&OperatorMutationFence>,
        automatic_fence: Option<&RevisionFence>,
        automatic_stage_permit: Option<&MatchStagePermit>,
        expected_face_revision: u64,
        expected_person_revision: u64,
    ) -> Result<Assignment, String> {
        let mut automatic_use = if state == AssignmentState::CommittedStrictAutomatic {
            let fence = automatic_fence
                .ok_or("strict automatic assignment requires an IndexJob revision fence")?;
            let permit = automatic_stage_permit
                .ok_or("strict automatic assignment requires governed stage admission")?;
            Some(permit.consume_for(&self.session_id, fence, JobStage::Suggest)?)
        } else {
            None
        };
        if state == AssignmentState::Suggestion {
            return Err("suggestion is not a committed assignment".to_string());
        }
        validate_text("assignment provenance", provenance)?;
        if automatic_use.is_some() {
            let permit = automatic_stage_permit
                .expect("strict automatic admission already required its stage permit");
            let initial_payload = serde_json::to_vec(&(
                face_id,
                person_id,
                look_id,
                provenance,
                model_generation,
                calibration_generation,
                envelope_hash,
            ))
            .map_err(|error| error.to_string())?
            .len();
            permit.authorize_payload(initial_payload)?;
        }
        let _guard = self.mutation_write_guard("Match assignment")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        if state == AssignmentState::CommittedStrictAutomatic
            && self
                .get_one_unlocked::<Value>(FACE_DISPOSITION_TABLE, face_id)?
                .is_some()
        {
            return Err("suppressed face must not receive an automatic assignment".to_string());
        }
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        if face.face_revision != expected_face_revision
            || person.revision != expected_person_revision
        {
            return Err("stale assignment revision fence".to_string());
        }
        let look_id = if let Some(look_id) = look_id {
            let look: Look = self.require_unlocked(LOOK_TABLE, look_id, "Look")?;
            if look.person_id != person_id {
                return Err("Look does not belong to the assigned Person".to_string());
            }
            Some(look_id.to_string())
        } else {
            None
        };
        let generation = match state {
            AssignmentState::CommittedStrictAutomatic => Some(
                model_generation
                    .filter(|generation| !generation.trim().is_empty())
                    .ok_or("strict automatic assignment requires model generation")?
                    .to_string(),
            ),
            AssignmentState::OperatorConfirmed => None,
            AssignmentState::Suggestion => unreachable!(),
        };
        let (calibration_generation, envelope_hash) = match state {
            AssignmentState::CommittedStrictAutomatic => (
                Some(
                    calibration_generation
                        .filter(|value| !value.trim().is_empty())
                        .ok_or("strict automatic assignment requires calibration generation")?
                        .to_string(),
                ),
                Some(
                    envelope_hash
                        .filter(|value| !value.trim().is_empty())
                        .ok_or("strict automatic assignment requires gallery envelope hash")?
                        .to_string(),
                ),
            ),
            AssignmentState::OperatorConfirmed => {
                if calibration_generation.is_some() || envelope_hash.is_some() {
                    return Err(
                        "operator-confirmed assignment must not claim calibration evidence"
                            .to_string(),
                    );
                }
                (None, None)
            }
            AssignmentState::Suggestion => unreachable!(),
        };
        if let Some(generation) = &generation {
            if operator_fence.is_some() {
                return Err(
                    "strict automatic assignment must not use an operator fence".to_string()
                );
            }
            let automatic_fence = automatic_fence
                .ok_or("strict automatic assignment requires an IndexJob revision fence")?;
            automatic_stage_permit
                .ok_or("strict automatic assignment requires governed stage admission")?;
            let record: ModelGeneration =
                self.require_unlocked(GENERATION_TABLE, generation, "model generation")?;
            if !record.validated || !matches!(record.state.as_str(), "usable" | "active") {
                return Err("strict automatic assignment generation is not usable".to_string());
            }
            let calibration: CalibrationActivation = self.require_unlocked(
                CALIBRATION_TABLE,
                calibration_generation
                    .as_deref()
                    .expect("strict calibration generation validated above"),
                "calibration activation",
            )?;
            if !calibration.active
                || calibration.verifier_verdict != "pass"
                || calibration.independent_review_verdict != "pass"
                || !calibration.wp084_runtime_ready
                || !calibration.wp087_release_ready
                || calibration.model_generation != *generation
                || Some(calibration.envelope_hash.as_str()) != envelope_hash.as_deref()
            {
                return Err("strict automatic calibration activation is not valid".to_string());
            }
            if self.trusted_gallery_members_digest_unlocked()? != calibration.gallery_members_digest
            {
                return Err("strict automatic gallery composition has drifted".to_string());
            }
            self.validate_gallery_envelope_unlocked(
                &calibration.model_generation,
                calibration.people_max,
                calibration.looks_per_person_max,
                calibration.templates_per_look_max,
                calibration.total_templates_max,
            )?;
            let fence = automatic_fence;
            if fence.media_key != face.media_key
                || fence.media_fingerprint != face.media_fingerprint
                || fence.model_generation != *generation
            {
                return Err("stale strict-assignment revision fence".to_string());
            }
            let asset = self.require_valid_asset_fence_unlocked(fence, true)?;
            if asset.next_stage()? != JobStage::Suggest {
                return Err(
                    "strict assignment no longer matches the durable stage cursor".to_string(),
                );
            }
        } else {
            if automatic_fence.is_some() || automatic_stage_permit.is_some() {
                return Err(
                    "operator-confirmed assignment must not use an automatic fence".to_string(),
                );
            }
            self.validate_operator_fence_unlocked(
                operator_fence.ok_or("operator-confirmed assignment requires an operator fence")?,
                &face,
                &person,
            )?;
        }
        if self
            .get_one_unlocked::<CannotLinkConstraint>(
                CONSTRAINT_TABLE,
                &cannot_link_id(face_id, person_id),
            )?
            .is_some()
        {
            return Err("cannot-linked Person cannot be assigned to this face".to_string());
        }
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        if prior.as_ref().is_some_and(|assignment| {
            assignment.state == AssignmentState::OperatorConfirmed.as_str()
                && state != AssignmentState::OperatorConfirmed
        }) {
            return Err("machine assignment cannot overwrite operator-confirmed truth".to_string());
        }
        let operation_id = new_id("operation");
        let now = now();
        let assignment = Assignment {
            assignment_id: face_id.to_string(),
            face_id: face_id.to_string(),
            person_id: person_id.to_string(),
            media_key: face.media_key.clone(),
            look_id: look_id.clone(),
            placement: if look_id.is_some() {
                "look"
            } else {
                "unsorted"
            }
            .to_string(),
            state: state.as_str().to_string(),
            provenance: provenance.trim().to_string(),
            locked: state == AssignmentState::OperatorConfirmed,
            model_generation: generation,
            calibration_generation,
            envelope_hash,
            face_revision: face.face_revision,
            person_revision: person.revision,
            operation_id: operation_id.clone(),
            created_at: if state == AssignmentState::CommittedStrictAutomatic {
                now.clone()
            } else {
                prior
                    .as_ref()
                    .map(|assignment| assignment.created_at.clone())
                    .unwrap_or_else(|| now.clone())
            },
            updated_at: now.clone(),
        };
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: if state == AssignmentState::OperatorConfirmed {
                "assign_operator_confirmed"
            } else {
                "assign_committed_strict_automatic"
            }
            .to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(person_id.to_string()),
            before_json: serde_json::to_string(&prior).map_err(|error| error.to_string())?,
            after_json: serde_json::to_string(&assignment).map_err(|error| error.to_string())?,
            reversible: true,
            created_at: now,
        };
        let operation_media_fingerprint =
            canonical_correction_media_fingerprint(&face.media_fingerprint)?;
        let operation_media_mapping = corrections::CorrectionMediaOperation {
            mapping_id: corrections::correction_media_mapping_id(&operation_id, &face.media_key),
            media_key: face.media_key.clone(),
            media_fingerprint: operation_media_fingerprint,
            operation_id: operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        };
        let mut execution = self.execution_state_unlocked()?;
        if state == AssignmentState::OperatorConfirmed {
            self.bump_revisions(&mut execution, true, false)?;
        }
        let suggestion_ids = self
            .list_unlocked::<Suggestion>(SUGGESTION_TABLE)?
            .into_iter()
            .filter(|suggestion| suggestion.face_id == face_id)
            .map(|suggestion| suggestion.suggestion_id)
            .collect::<Vec<_>>();
        let mut deletes = suggestion_ids
            .iter()
            .map(|id| (SUGGESTION_TABLE, id.as_str()))
            .collect::<Vec<_>>();
        deletes.push((PROJECTION_TABLE, face.media_key.as_str()));
        let trusted_members = self
            .list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?
            .into_iter()
            .filter(|membership| membership.face_id == face_id)
            .map(|membership| membership.membership_id)
            .collect::<Vec<_>>();
        for membership_id in &trusted_members {
            deletes.push((TRUSTED_MEMBER_TABLE, membership_id.as_str()));
            deletes.push((TRUSTED_SEARCH_TABLE, membership_id.as_str()));
        }
        if !trusted_members.is_empty() {
            self.invalidate_calibrations_unlocked("trusted_face_assignment_changed")?;
        }
        if let Some(permit) = automatic_stage_permit {
            let payload_bytes = serde_json::to_vec(&(
                &assignment,
                &operation,
                &operation_media_mapping,
                &execution,
            ))
            .map_err(|error| error.to_string())?
            .len();
            permit.authorize_payload(payload_bytes)?;
        }
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    ASSIGNMENT_TABLE,
                    face_id,
                    serde_json::to_value(&assignment).unwrap(),
                ),
                (
                    OPERATION_TABLE,
                    &operation_id,
                    serde_json::to_value(&operation).unwrap(),
                ),
                (
                    corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                    &operation_media_mapping.mapping_id,
                    serde_json::to_value(&operation_media_mapping).unwrap(),
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &deletes,
        )?;
        self.caches
            .write()
            .map_err(|_| "Match projection cache is poisoned".to_string())?
            .projections
            .remove(&face.media_key);
        if let Some(stage_use) = &mut automatic_use {
            stage_use.success();
        }
        drop(_guard);
        Ok(assignment)
    }

    pub fn review_same(
        &self,
        face_id: &str,
        person_id: &str,
        operator_fence: &OperatorMutationFence,
    ) -> Result<Assignment, String> {
        self.assign_face(
            face_id,
            person_id,
            None,
            AssignmentState::OperatorConfirmed,
            "review_same",
            None,
            None,
            None,
            Some(operator_fence),
            None,
            None,
            operator_fence.face_revision,
            operator_fence.person_revision,
        )
    }

    pub fn review_different(
        &self,
        face_id: &str,
        person_id: &str,
        operator_fence: &OperatorMutationFence,
    ) -> Result<(), String> {
        let _guard = self.mutation_write_guard("Match review")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        self.validate_operator_fence_unlocked(operator_fence, &face, &person)?;
        let prior = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        if prior
            .as_ref()
            .is_some_and(|assignment| assignment.person_id != person_id)
        {
            return Err("Different candidate does not match the committed Person".to_string());
        }
        let operation_id = new_id("operation");
        let constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(face_id, person_id),
            face_id: face_id.to_string(),
            person_id: person_id.to_string(),
            operation_id: operation_id.clone(),
            operator_owned: true,
            created_at: now(),
        };
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: "different".to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(person_id.to_string()),
            before_json: serde_json::to_string(&prior).map_err(|error| error.to_string())?,
            after_json: serde_json::to_string(&constraint).map_err(|error| error.to_string())?,
            reversible: true,
            created_at: now(),
        };
        let operation_media_fingerprint =
            canonical_correction_media_fingerprint(&face.media_fingerprint)?;
        let operation_media_mapping = corrections::CorrectionMediaOperation {
            mapping_id: corrections::correction_media_mapping_id(&operation_id, &face.media_key),
            media_key: face.media_key.clone(),
            media_fingerprint: operation_media_fingerprint,
            operation_id: operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        };
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let trusted_members = self
            .list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?
            .into_iter()
            .filter(|membership| membership.face_id == face_id)
            .map(|membership| membership.membership_id)
            .collect::<Vec<_>>();
        let suggestion_key = suggestion_id(face_id, person_id);
        let mut deletes = vec![
            (ASSIGNMENT_TABLE, face_id),
            (SUGGESTION_TABLE, suggestion_key.as_str()),
            (PROJECTION_TABLE, face.media_key.as_str()),
        ];
        for membership_id in &trusted_members {
            deletes.push((TRUSTED_MEMBER_TABLE, membership_id.as_str()));
            deletes.push((TRUSTED_SEARCH_TABLE, membership_id.as_str()));
        }
        if !trusted_members.is_empty() {
            self.invalidate_calibrations_unlocked("trusted_face_rejected")?;
        }
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    CONSTRAINT_TABLE,
                    &constraint.constraint_id,
                    serde_json::to_value(&constraint).unwrap(),
                ),
                (
                    OPERATION_TABLE,
                    &operation_id,
                    serde_json::to_value(&operation).unwrap(),
                ),
                (
                    corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                    &operation_media_mapping.mapping_id,
                    serde_json::to_value(&operation_media_mapping).unwrap(),
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &deletes,
        )?;
        self.caches
            .write()
            .map_err(|_| "Match projection cache is poisoned".to_string())?
            .projections
            .remove(&face.media_key);
        drop(_guard);
        Ok(())
    }

    pub fn review_not_sure(
        &self,
        face_id: &str,
        person_id: &str,
        operator_fence: &OperatorMutationFence,
    ) -> Result<(), String> {
        let _guard = self.mutation_write_guard("Match not-sure review")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        self.validate_operator_fence_unlocked(operator_fence, &face, &person)?;
        let before_assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        let before_constraint = self.get_one_unlocked::<CannotLinkConstraint>(
            CONSTRAINT_TABLE,
            &cannot_link_id(face_id, person_id),
        )?;
        let operation = MatchOperation {
            operation_id: new_id("operation"),
            kind: "not_sure".to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(person_id.to_string()),
            before_json: serde_json::to_string(&(before_assignment, before_constraint))
                .map_err(|error| error.to_string())?,
            after_json: "null".to_string(),
            reversible: false,
            created_at: now(),
        };
        let operation_media_fingerprint =
            canonical_correction_media_fingerprint(&face.media_fingerprint)?;
        let operation_media_mapping = corrections::CorrectionMediaOperation {
            mapping_id: corrections::correction_media_mapping_id(
                &operation.operation_id,
                &face.media_key,
            ),
            media_key: face.media_key.clone(),
            media_fingerprint: operation_media_fingerprint,
            operation_id: operation.operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    OPERATION_TABLE,
                    &operation.operation_id,
                    serde_json::to_value(&operation).map_err(|error| error.to_string())?,
                ),
                (
                    corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                    &operation_media_mapping.mapping_id,
                    serde_json::to_value(&operation_media_mapping)
                        .map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )
    }

    pub fn this_is_not(
        &self,
        face_id: &str,
        person_id: &str,
        operator_fence: &OperatorMutationFence,
    ) -> Result<(), String> {
        self.review_different(face_id, person_id, operator_fence)
    }

    pub fn move_to_look(
        &self,
        face_id: &str,
        look_id: &str,
        operator_fence: &OperatorMutationFence,
    ) -> Result<Assignment, String> {
        let _guard = self.mutation_write_guard("Match Look move")?;
        let mut assignment: Assignment =
            self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let look: Look = self.require_unlocked(LOOK_TABLE, look_id, "Look")?;
        let person: Person =
            self.require_unlocked(PERSON_TABLE, &assignment.person_id, "Person")?;
        self.validate_operator_fence_unlocked(operator_fence, &face, &person)?;
        if look.person_id != assignment.person_id {
            return Err("Look does not belong to the assigned Person".to_string());
        }
        let prior = assignment.clone();
        assignment.look_id = Some(look_id.to_string());
        assignment.placement = "look".to_string();
        assignment.updated_at = now();
        assignment.operation_id = new_id("operation");
        let operation = MatchOperation {
            operation_id: assignment.operation_id.clone(),
            kind: "move_to_look".to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(assignment.person_id.clone()),
            before_json: serde_json::to_string(&prior).map_err(|error| error.to_string())?,
            after_json: serde_json::to_string(&assignment).map_err(|error| error.to_string())?,
            reversible: true,
            created_at: assignment.updated_at.clone(),
        };
        let operation_media_fingerprint =
            canonical_correction_media_fingerprint(&face.media_fingerprint)?;
        let operation_media_mapping = corrections::CorrectionMediaOperation {
            mapping_id: corrections::correction_media_mapping_id(
                &operation.operation_id,
                &face.media_key,
            ),
            media_key: face.media_key.clone(),
            media_fingerprint: operation_media_fingerprint,
            operation_id: operation.operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        };
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        let trusted_members = self
            .list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?
            .into_iter()
            .filter(|membership| membership.face_id == face_id)
            .map(|membership| membership.membership_id)
            .collect::<Vec<_>>();
        let mut deletes = vec![(PROJECTION_TABLE, face.media_key.as_str())];
        for membership_id in &trusted_members {
            deletes.push((TRUSTED_MEMBER_TABLE, membership_id.as_str()));
            deletes.push((TRUSTED_SEARCH_TABLE, membership_id.as_str()));
        }
        if !trusted_members.is_empty() {
            self.invalidate_calibrations_unlocked("trusted_face_look_changed")?;
        }
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    ASSIGNMENT_TABLE,
                    face_id,
                    serde_json::to_value(&assignment).map_err(|error| error.to_string())?,
                ),
                (
                    OPERATION_TABLE,
                    &operation.operation_id,
                    serde_json::to_value(&operation).map_err(|error| error.to_string())?,
                ),
                (
                    corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                    &operation_media_mapping.mapping_id,
                    serde_json::to_value(&operation_media_mapping)
                        .map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &deletes,
        )?;
        self.caches
            .write()
            .map_err(|_| "Match projection cache is poisoned".to_string())?
            .projections
            .remove(&face.media_key);
        drop(_guard);
        Ok(assignment)
    }

    pub fn authorize_trusted_reference(
        &self,
        set_id: &str,
        face_id: &str,
        authorized: bool,
        eligibility: TrustedEligibility,
        operator_fence: &OperatorMutationFence,
    ) -> Result<TrustedTemplateMembership, String> {
        if !authorized {
            return Err("trusted-reference authorization must be explicit".to_string());
        }
        if !eligibility.is_declared() {
            return Err("trusted-reference evidence declaration is incomplete".to_string());
        }
        let _guard = self.mutation_write_guard("Match trusted-reference")?;
        let set: TrustedTemplateSet =
            self.require_unlocked(TEMPLATE_SET_TABLE, set_id, "TrustedTemplateSet")?;
        let assignment: Assignment =
            self.require_unlocked(ASSIGNMENT_TABLE, face_id, "Assignment")?;
        if assignment.state != AssignmentState::OperatorConfirmed.as_str() {
            return Err(
                "trusted reference requires operator-confirmed identity evidence".to_string(),
            );
        }
        if assignment.look_id.as_deref() != Some(set.look_id.as_str()) {
            return Err("trusted reference must be assigned to the template set Look".to_string());
        }
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let person: Person =
            self.require_unlocked(PERSON_TABLE, &assignment.person_id, "Person")?;
        self.validate_operator_fence_unlocked(operator_fence, &face, &person)?;
        if !face.alignment_valid {
            return Err("invalid alignment cannot become a trusted reference".to_string());
        }
        if face.quality < TRUSTED_MIN_QUALITY {
            return Err("face quality is below the trusted-reference threshold".to_string());
        }
        let pose_passed = is_real_yaw_bucket(&face.pose_bucket);
        if !pose_passed {
            return Err("face pose evidence is not eligible for trusted reference".to_string());
        }
        let generation: ModelGeneration = self.require_unlocked(
            GENERATION_TABLE,
            &eligibility.model_generation,
            "model generation",
        )?;
        if !generation.validated || generation.state != "active" {
            return Err("trusted reference requires the active validated generation".to_string());
        }
        let embedding: FaceEmbedding =
            self.require_unlocked(EMBEDDING_TABLE, &eligibility.embedding_id, "face embedding")?;
        if !embedding.active
            || embedding.face_id != face.face_id
            || embedding.model_generation != eligibility.model_generation
            || embedding.media_fingerprint != face.media_fingerprint
            || embedding.face_revision != face.face_revision
        {
            return Err("trusted-reference embedding evidence is stale or mismatched".to_string());
        }
        let existing_members = self
            .list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?
            .into_iter()
            .filter(|membership| membership.set_id == set_id && membership.face_id != face_id)
            .collect::<Vec<_>>();
        let diversity_passed = if existing_members.is_empty() {
            true
        } else {
            existing_members
                .iter()
                .any(|membership| membership.pose_bucket != face.pose_bucket)
        };
        if !diversity_passed {
            return Err("trusted-reference pose diversity gate did not pass".to_string());
        }
        let operation_id = new_id("operation");
        let prior = self.get_one_unlocked::<TrustedTemplateMembership>(
            TRUSTED_MEMBER_TABLE,
            &trusted_member_id(set_id, face_id),
        )?;
        let membership = TrustedTemplateMembership {
            membership_id: trusted_member_id(set_id, face_id),
            set_id: set_id.to_string(),
            look_id: set.look_id,
            face_id: face_id.to_string(),
            authorized,
            alignment_valid: face.alignment_valid,
            quality_passed: true,
            pose_passed,
            diversity_passed,
            provenance: eligibility.provenance,
            model_generation: eligibility.model_generation,
            embedding_id: embedding.embedding_id.clone(),
            media_fingerprint: face.media_fingerprint,
            face_revision: face.face_revision,
            quality_score: face.quality,
            quality_threshold: TRUSTED_MIN_QUALITY,
            pose_bucket: face.pose_bucket,
            policy_version: eligibility.policy_version,
            operation_id: operation_id.clone(),
            created_at: now(),
        };
        let operation = MatchOperation {
            operation_id: operation_id.clone(),
            kind: "authorize_trusted_reference".to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some(assignment.person_id),
            before_json: serde_json::to_string(&prior).map_err(|error| error.to_string())?,
            after_json: serde_json::to_string(&membership).map_err(|error| error.to_string())?,
            reversible: true,
            created_at: membership.created_at.clone(),
        };
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        self.invalidate_calibrations_unlocked("trusted_gallery_membership_changed")?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    TRUSTED_MEMBER_TABLE,
                    &membership.membership_id,
                    serde_json::to_value(&membership).map_err(|error| error.to_string())?,
                ),
                (
                    OPERATION_TABLE,
                    &operation_id,
                    serde_json::to_value(&operation).map_err(|error| error.to_string())?,
                ),
                (
                    EXECUTION_TABLE,
                    "global",
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ),
            ],
            &[(TRUSTED_INDEX_BUILD_TABLE, "global")],
        )?;
        // Trusted search is a derived projection. Once the authorization,
        // operation receipt, and identity revision commit, a projection
        // rebuild failure must not turn the durable mutation into an ambiguous
        // error. Ordinary reconciliation remains restart-safe and retries from
        // the canonical membership on the next explicit/open-time rebuild.
        let _ = self.reconcile_trusted_search_unlocked();
        Ok(membership)
    }

    /// Persist an independently verified activation generation. WP-082 alone
    /// cannot set `active`: runtime readiness from WP-084 and release readiness
    /// from WP-087 are explicit hard predecessors.
    pub fn register_calibration_verification(
        &self,
        verification: &CalibrationVerification,
    ) -> Result<(), String> {
        let claim = verification
            .verified_claim()
            .ok_or("calibration verification did not issue a verified claim")?;
        let observed = verification
            .observed_trial_claim()
            .ok_or("calibration verification did not issue an observed-trial claim")?;
        // A signed, authenticated trial is irrevocably spent before any
        // activation-specific validation.  Otherwise a verifier/runtime bound
        // mismatch could return early and leave the same observations reusable.
        let _guard = self.mutation_write_guard("Match calibration registration")?;
        self.consume_observed_trial_unlocked(observed)?;
        validate_text("calibration generation", claim.calibration_generation())?;
        validate_text("calibration model generation", claim.model_generation())?;
        validate_sha256("gallery envelope hash", claim.envelope_hash())?;
        validate_sha256(
            "runtime configuration digest",
            claim.runtime_configuration_digest(),
        )?;
        validate_sha256("gallery members digest", claim.gallery_members_digest())?;
        validate_text("verifier artifact id", claim.artifact_id())?;
        validate_sha256("calibration contract hash", claim.contract_sha256())?;
        validate_sha256("calibration raw-record hash", claim.raw_records_sha256())?;
        validate_sha256("calibration evidence digest", claim.evidence_digest())?;
        validate_sha256("calibration review digest", claim.review_digest())?;
        validate_text("calibration activation run", claim.activation_run_id())?;
        validate_sha256("fixture manifest hash", claim.fixture_manifest_sha256())?;

        let automatic_threshold = claim.automatic_threshold();
        let suggestion_threshold = claim.suggestion_threshold();
        let runner_up_margin = claim.runner_up_margin();
        let minimum_quality = claim.minimum_quality();
        if !automatic_threshold.is_finite()
            || !suggestion_threshold.is_finite()
            || !runner_up_margin.is_finite()
            || !minimum_quality.is_finite()
            || !(0.0..=1.0).contains(&suggestion_threshold)
            || !(suggestion_threshold..=1.0).contains(&automatic_threshold)
            || !(0.0..=1.0).contains(&runner_up_margin)
            || !(0.0..=1.0).contains(&minimum_quality)
            || claim.candidate_k() == 0
            || claim.candidate_k() > 1000
            || claim.rerank_k() == 0
            || claim.rerank_k() > claim.candidate_k()
            || claim.people_max() == 0
            || claim.looks_per_person_max() == 0
            || claim.trusted_templates_per_look_max() == 0
            || claim.total_templates_max() == 0
        {
            return Err("verified calibration claim has an invalid runtime envelope".to_string());
        }
        let expected_runtime_digest =
            strict_runtime_configuration_digest(claim.candidate_k(), claim.rerank_k());
        if claim.runtime_configuration_digest() != expected_runtime_digest {
            return Err(
                "verified calibration claim does not match the shipped runtime pipeline"
                    .to_string(),
            );
        }

        self.reconcile_trusted_search_unlocked()?;
        let trusted_index_build: TrustedIndexBuildReceipt = self.require_unlocked(
            TRUSTED_INDEX_BUILD_TABLE,
            "global",
            "trusted index build receipt",
        )?;
        let generation: ModelGeneration = self.require_unlocked(
            GENERATION_TABLE,
            claim.model_generation(),
            "model generation",
        )?;
        if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
            return Err("calibration activation model generation is not usable".to_string());
        }
        let current_digest = self.trusted_gallery_members_digest_unlocked()?;
        if current_digest != claim.gallery_members_digest() {
            return Err(
                "verified calibration gallery does not match the live trusted gallery".to_string(),
            );
        }
        self.validate_gallery_envelope_unlocked(
            claim.model_generation(),
            claim.people_max(),
            claim.looks_per_person_max(),
            claim.trusted_templates_per_look_max(),
            claim.total_templates_max(),
        )?;

        let timestamp = now();
        let mut activation = CalibrationActivation {
            calibration_generation: claim.calibration_generation().to_string(),
            model_generation: claim.model_generation().to_string(),
            envelope_hash: claim.envelope_hash().to_string(),
            runtime_configuration_digest: claim.runtime_configuration_digest().to_string(),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: trusted_index_build.build_digest,
            gallery_members_digest: claim.gallery_members_digest().to_string(),
            verifier_artifact_id: claim.artifact_id().to_string(),
            contract_sha256: claim.contract_sha256().to_string(),
            raw_records_sha256: claim.raw_records_sha256().to_string(),
            evidence_digest: claim.evidence_digest().to_string(),
            review_digest: claim.review_digest().to_string(),
            automatic_threshold: automatic_threshold as f32,
            suggestion_threshold: suggestion_threshold as f32,
            runner_up_margin: runner_up_margin as f32,
            minimum_quality: minimum_quality as f32,
            candidate_k: claim.candidate_k(),
            rerank_k: claim.rerank_k(),
            people_max: claim.people_max(),
            looks_per_person_max: claim.looks_per_person_max(),
            templates_per_look_max: claim.trusted_templates_per_look_max(),
            total_templates_max: claim.total_templates_max(),
            verifier_verdict: "pass".to_string(),
            independent_review_verdict: "pass".to_string(),
            wp084_runtime_ready: false,
            wp087_release_ready: false,
            active: false,
            invalidation_reason: None,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        activation.activation_integrity_digest =
            calibration_activation_integrity_digest(&activation);
        let activation_value =
            serde_json::to_value(&activation).map_err(|error| error.to_string())?;
        self.transactional_upserts_deletes_unlocked(
            &[(
                CALIBRATION_TABLE,
                claim.calibration_generation(),
                activation_value,
            )],
            &[],
        )
    }

    /// Consume signed observation identities even when the evaluated candidate
    /// fails statistical or activation gates. This prevents trial-and-error
    /// reuse from selecting a lucky candidate.
    pub fn consume_calibration_observation(
        &self,
        verification: &CalibrationVerification,
    ) -> Result<(), String> {
        let claim = verification
            .observed_trial_claim()
            .ok_or("calibration verification did not issue an observed-trial claim")?;
        let _guard = self.mutation_write_guard("Match calibration spending")?;
        self.consume_observed_trial_unlocked(claim)
    }

    fn consume_observed_trial_unlocked(
        &self,
        claim: &crate::identity::ObservedCalibrationClaim,
    ) -> Result<(), String> {
        validate_text("calibration activation run", claim.activation_run_id())?;
        validate_sha256("fixture manifest hash", claim.fixture_manifest_sha256())?;
        validate_sha256("calibration evidence digest", claim.evidence_digest())?;
        let person_hashes = claim
            .observed_person_id_hashes()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let acquisition_hashes = claim
            .observed_acquisition_cluster_id_hashes()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if person_hashes.len() != claim.observed_person_id_hashes().len()
            || acquisition_hashes.len() != claim.observed_acquisition_cluster_id_hashes().len()
            || person_hashes.is_empty()
            || acquisition_hashes.is_empty()
            || person_hashes
                .iter()
                .any(|hash| validate_sha256("spent Person hash", hash).is_err())
            || acquisition_hashes
                .iter()
                .any(|hash| validate_sha256("spent acquisition hash", hash).is_err())
        {
            return Err("verified calibration spent-set identifiers are invalid".to_string());
        }
        for prior in self.list_unlocked::<CalibrationSpentSet>(CALIBRATION_SPENT_TABLE)? {
            if prior.activation_run_id == claim.activation_run_id()
                || prior.fixture_manifest_sha256 == claim.fixture_manifest_sha256()
                || prior.evidence_digest == claim.evidence_digest()
                || prior
                    .person_hashes
                    .iter()
                    .any(|hash| person_hashes.contains(hash))
                || prior
                    .acquisition_cluster_hashes
                    .iter()
                    .any(|hash| acquisition_hashes.contains(hash))
            {
                return Err("calibration evidence was already spent".to_string());
            }
        }
        let spent_set = CalibrationSpentSet {
            activation_run_id: claim.activation_run_id().to_string(),
            fixture_manifest_sha256: claim.fixture_manifest_sha256().to_string(),
            evidence_digest: claim.evidence_digest().to_string(),
            person_hashes: person_hashes.into_iter().collect(),
            acquisition_cluster_hashes: acquisition_hashes.into_iter().collect(),
            created_at: now(),
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                CALIBRATION_SPENT_TABLE,
                claim.activation_run_id(),
                serde_json::to_value(spent_set).map_err(|error| error.to_string())?,
            )],
            &[],
        )
    }

    pub fn register_model_generation(
        &self,
        generation: &str,
        validated: bool,
    ) -> Result<ModelGeneration, String> {
        validate_text("model generation", generation)?;
        if let Some(existing) = self.get_one::<ModelGeneration>(GENERATION_TABLE, generation)? {
            if existing.validated == validated
                && matches!(existing.state.as_str(), "usable" | "active")
            {
                return Ok(existing);
            }
        }
        let now = now();
        let record = ModelGeneration {
            generation: generation.to_string(),
            state: if validated { "usable" } else { "building" }.to_string(),
            validated,
            created_at: now.clone(),
            updated_at: now,
        };
        self.upsert_json(GENERATION_TABLE, generation, &record)?;
        Ok(record)
    }

    pub fn activate_model_generation(&self, generation: &str) -> Result<(), String> {
        let _guard = self.mutation_write_guard("Match generation switch")?;
        let target: ModelGeneration =
            self.require_unlocked(GENERATION_TABLE, generation, "model generation")?;
        if !target.validated || !matches!(target.state.as_str(), "usable" | "active") {
            return Err("model generation must validate before activation".to_string());
        }
        if target.state == "active" {
            return Ok(());
        }
        let generations = self.list_unlocked::<ModelGeneration>(GENERATION_TABLE)?;
        let projections = self.list_unlocked::<PeopleProjection>(PROJECTION_TABLE)?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, true, false)?;
        self.invalidate_calibrations_unlocked("active_model_generation_changed")?;
        let now = now();
        let mut upserts = Vec::with_capacity(generations.len());
        for mut item in generations {
            item.state = if item.generation == generation {
                "active"
            } else if item.validated {
                "usable"
            } else {
                "building"
            }
            .to_string();
            item.updated_at = now.clone();
            upserts.push((
                GENERATION_TABLE,
                item.generation.clone(),
                serde_json::to_value(item).unwrap(),
            ));
        }
        upserts.push((
            EXECUTION_TABLE,
            "global".to_string(),
            serde_json::to_value(&execution).map_err(|error| error.to_string())?,
        ));
        let refs = upserts
            .iter()
            .map(|(table, id, value)| (*table, id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        let projection_ids = projections
            .iter()
            .map(|projection| projection.media_key.as_str())
            .collect::<Vec<_>>();
        let deletes = projection_ids
            .iter()
            .map(|id| (PROJECTION_TABLE, *id))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&refs, &deletes)?;
        self.caches
            .write()
            .map_err(|_| "Match projection cache is poisoned".to_string())?
            .projections
            .clear();
        drop(_guard);
        Ok(())
    }

    pub fn set_desired_mode(&self, mode: DesiredMode) -> Result<(), String> {
        let _guard = self.mutation_write_guard("Match desired-mode")?;
        let mut state: PersistedExecutionState =
            self.require_unlocked(EXECUTION_TABLE, "global", "Match execution state")?;
        state.desired_mode = mode.as_str().to_string();
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("Match execution revision overflow")?;
        state.updated_at = now();
        self.transactional_upserts_deletes_unlocked(
            &[(
                EXECUTION_TABLE,
                "global",
                serde_json::to_value(&state).map_err(|error| error.to_string())?,
            )],
            &[],
        )
    }

    pub fn desired_mode(&self) -> Result<DesiredMode, String> {
        DesiredMode::parse(&self.execution_state()?.desired_mode)
    }

    pub fn add_hold(&self, reason: HoldReason) -> Result<(), String> {
        self.transient_holds
            .lock()
            .map_err(|_| "Match transient hold set is poisoned".to_string())?
            .insert(reason);
        Ok(())
    }

    pub fn remove_hold(&self, reason: HoldReason) -> Result<(), String> {
        self.transient_holds
            .lock()
            .map_err(|_| "Match transient hold set is poisoned".to_string())?
            .remove(&reason);
        Ok(())
    }

    pub fn holds(&self) -> Result<Vec<String>, String> {
        let mut holds = self
            .transient_holds
            .lock()
            .map_err(|_| "Match transient hold set is poisoned".to_string())?
            .iter()
            .map(|reason| reason.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let external = self.external_holds.snapshot();
        if external & 1 != 0 {
            holds.insert(HoldReason::ViewerPlayback.as_str().to_string());
        }
        if external & 2 != 0 {
            holds.insert(HoldReason::ImmersiveFullscreen.as_str().to_string());
        }
        Ok(holds.into_iter().collect())
    }

    pub fn can_admit(&self, lifecycle: JobLifecycle) -> Result<bool, String> {
        if self.external_holds.blocked() {
            return Ok(false);
        }
        if lifecycle.is_terminal_or_failed() || !lifecycle.is_runnable() {
            return Ok(false);
        }
        Ok(self.desired_mode()? == DesiredMode::Running && self.holds()?.is_empty())
    }

    pub fn can_attempt_automatic(&self, lifecycle: JobLifecycle) -> Result<bool, String> {
        let _database_unit = self.begin_database_unit()?;
        if self.external_holds.blocked() {
            return Ok(false);
        }
        if lifecycle.is_terminal_or_failed() || !lifecycle.is_runnable() {
            return Ok(false);
        }
        let blocking_holds = self
            .transient_holds
            .lock()
            .map_err(|_| "Match transient hold set is poisoned".to_string())?
            .iter()
            .any(|reason| *reason != HoldReason::ResourcePressure);
        Ok(self.desired_mode()? == DesiredMode::Running
            && !blocking_holds
            && !self.external_holds.blocked())
    }

    pub fn create_job(&self, root_key: &str, model_generation: &str) -> Result<IndexJob, String> {
        validate_text("job root key", root_key)?;
        let _guard = self.mutation_write_guard("Match job creation")?;
        let generation: ModelGeneration =
            self.require_unlocked(GENERATION_TABLE, model_generation, "model generation")?;
        if !generation.validated || !matches!(generation.state.as_str(), "usable" | "active") {
            return Err("IndexJob model generation is not validated and usable".to_string());
        }
        let execution = self.execution_state_unlocked()?;
        let now = now();
        let job = IndexJob {
            job_id: new_id("job"),
            root_key: root_key.to_string(),
            lifecycle: JobLifecycle::Queued.as_str().to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: model_generation.to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            discovered: 0,
            completed: 0,
            failed: 0,
            skipped: 0,
            failure_code: None,
            failure_message: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.transactional_upserts_deletes_unlocked(
            &[(
                JOB_TABLE,
                &job.job_id,
                serde_json::to_value(&job).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        Ok(job)
    }

    pub fn set_job_lifecycle(&self, job_id: &str, next: JobLifecycle) -> Result<IndexJob, String> {
        let _guard = self.mutation_write_guard("Match job transition")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        let current = job.lifecycle()?;
        validate_job_transition(current, next)?;
        job.lifecycle = next.as_str().to_string();
        job.updated_at = now();
        self.transactional_upserts_deletes_unlocked(
            &[(
                JOB_TABLE,
                job_id,
                serde_json::to_value(&job).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        Ok(job)
    }

    pub fn record_job_failure(
        &self,
        job_id: &str,
        code: &str,
        message: &str,
    ) -> Result<IndexJob, String> {
        let _database_unit = self.begin_database_unit()?;
        validate_failure_code(code)?;
        let _guard = self.mutation_write_guard("Match job failure")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !matches!(
            job.lifecycle()?,
            JobLifecycle::Running | JobLifecycle::Retrying | JobLifecycle::Pausing
        ) {
            return Err(
                "only running, retrying, or pausing Match jobs can record a job failure"
                    .to_string(),
            );
        }
        job.lifecycle = JobLifecycle::Failed.as_str().to_string();
        job.failure_code = Some(code.to_string());
        job.failure_message = Some(message.chars().take(2048).collect());
        job.updated_at = now();
        self.transactional_upserts_deletes_unlocked(
            &[(
                JOB_TABLE,
                job_id,
                serde_json::to_value(&job).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        Ok(job)
    }

    #[cfg(test)]
    pub fn enqueue_asset(
        &self,
        job_id: &str,
        media_key: &str,
        media_fingerprint: &str,
    ) -> Result<JobAsset, String> {
        self.enqueue_asset_with_source_path_for_test(job_id, media_key, media_fingerprint, None)
    }

    #[cfg(test)]
    fn enqueue_asset_with_source_path_for_test(
        &self,
        job_id: &str,
        media_key: &str,
        media_fingerprint: &str,
        source_path: Option<&Path>,
    ) -> Result<JobAsset, String> {
        let permit = self.acquire_discovery_write(
            job_id,
            media_key,
            ResourceRequest {
                admitted_items: 1,
                queued_items: 1,
                queued_bytes: 64 * 1024,
                surreal_writes: 1,
                ..ResourceRequest::default()
            },
        )?;
        self.enqueue_asset_with_source_path(
            job_id,
            media_key,
            media_fingerprint,
            source_path,
            &permit,
        )
    }

    #[cfg(test)]
    pub fn enqueue_asset_with_source_path(
        &self,
        job_id: &str,
        media_key: &str,
        media_fingerprint: &str,
        source_path: Option<&Path>,
        discovery_permit: &MatchDiscoveryPermit,
    ) -> Result<JobAsset, String> {
        self.enqueue_asset_with_source_identity(
            job_id,
            media_key,
            media_fingerprint,
            source_path,
            discovery_permit,
            false,
        )
    }

    /// The production discovery worker already checked the root and final
    /// source identity in its isolated fingerprint unit. Publication repeats
    /// lexical confinement without filesystem calls under the writer lease.
    pub(crate) fn enqueue_asset_from_isolated_source(
        &self,
        job_id: &str,
        media_key: &str,
        media_fingerprint: &str,
        source_path: &Path,
        discovery_permit: &MatchDiscoveryPermit,
    ) -> Result<JobAsset, String> {
        self.enqueue_asset_with_source_identity(
            job_id,
            media_key,
            media_fingerprint,
            Some(source_path),
            discovery_permit,
            true,
        )
    }

    fn enqueue_asset_with_source_identity(
        &self,
        job_id: &str,
        media_key: &str,
        media_fingerprint: &str,
        source_path: Option<&Path>,
        discovery_permit: &MatchDiscoveryPermit,
        isolated_identity: bool,
    ) -> Result<JobAsset, String> {
        validate_media_key(media_key)?;
        validate_text("media fingerprint", media_fingerprint)?;
        let _discovery_use = discovery_permit.consume_for(&self.session_id, job_id, media_key)?;
        let _guard = self.mutation_write_guard("Match asset enqueue")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !job.lifecycle()?.accepts_discovery_result() {
            return Err("terminal or non-runnable IndexJob cannot enqueue assets".to_string());
        }
        let execution = self.execution_state_unlocked()?;
        let generation: ModelGeneration =
            self.require_unlocked(GENERATION_TABLE, &job.model_generation, "model generation")?;
        if job.identity_revision != execution.identity_revision
            || job.catalog_revision != execution.catalog_revision
            || !generation.validated
            || !matches!(generation.state.as_str(), "usable" | "active")
        {
            return Err("stale IndexJob cannot enqueue assets".to_string());
        }
        let canonical_source = if let Some(source_path) = source_path {
            let root: MatchIndexRoot =
                self.require_unlocked(ROOT_CONFIG_TABLE, &job.root_key, "Match index root")?;
            if isolated_identity {
                if !source_path.is_absolute()
                    || source_path.components().any(|component| {
                        matches!(component, Component::ParentDir | Component::CurDir)
                    })
                    || !source_path.starts_with(Path::new(&root.path))
                    || source_path == Path::new(&root.path)
                {
                    return Err("isolated Match source path escaped its configured root".into());
                }
                Some(source_path.to_string_lossy().to_string())
            } else {
                let canonical_root = Path::new(&root.path)
                    .canonicalize()
                    .map_err(|error| format!("canonicalize Match index root: {error}"))?;
                if canonical_root != Path::new(&root.path) {
                    return Err("configured Match root identity changed after opt-in".to_string());
                }
                let canonical = source_path
                    .canonicalize()
                    .map_err(|error| format!("canonicalize Match source path: {error}"))?;
                if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
                    return Err(
                        "Match source path must be a file inside its configured root".to_string(),
                    );
                }
                Some(canonical.to_string_lossy().to_string())
            }
        } else {
            None
        };
        let asset_id = job_asset_id(job_id, media_key);
        if let Some(mut existing) = self.get_one_unlocked::<JobAsset>(JOB_ASSET_TABLE, &asset_id)? {
            if existing.media_fingerprint != media_fingerprint {
                if existing.media_fingerprint.starts_with("unavailable:")
                    && existing.next_stage()? == JobStage::Discover
                    && existing.completed_stages.is_empty()
                {
                    let had_failure = existing.failure_code.is_some();
                    existing.media_fingerprint = media_fingerprint.to_string();
                    existing.source_path = canonical_source;
                    existing.failure_code = None;
                    existing.failure_message = None;
                    existing.updated_at = now();
                    if had_failure {
                        job.failed = job
                            .failed
                            .checked_sub(1)
                            .ok_or("Match failed count underflow while resolving discovery")?;
                        job.updated_at = now();
                    }
                    let existing_value =
                        serde_json::to_value(&existing).map_err(|error| error.to_string())?;
                    let job_value =
                        serde_json::to_value(&job).map_err(|error| error.to_string())?;
                    let payload_bytes = serde_json::to_vec(&existing_value)
                        .map_err(|error| error.to_string())?
                        .len()
                        .checked_add(
                            serde_json::to_vec(&job_value)
                                .map_err(|error| error.to_string())?
                                .len(),
                        )
                        .ok_or("Match discovery payload size overflow")?;
                    discovery_permit.authorize_payload(payload_bytes)?;
                    self.transactional_upserts_deletes_unlocked(
                        &[
                            (JOB_ASSET_TABLE, &asset_id, existing_value),
                            (JOB_TABLE, job_id, job_value),
                        ],
                        &[],
                    )?;
                    return Ok(existing);
                }
                return Err("job asset media fingerprint changed".to_string());
            }
            if canonical_source.is_some() && existing.source_path != canonical_source {
                return Err("job asset source path changed".to_string());
            }
            discovery_permit.authorize_payload(
                serde_json::to_vec(&existing)
                    .map_err(|error| error.to_string())?
                    .len(),
            )?;
            return Ok(existing);
        }
        let asset = JobAsset {
            asset_id: asset_id.clone(),
            job_id: job_id.to_string(),
            media_key: media_key.to_string(),
            source_path: canonical_source,
            media_fingerprint: media_fingerprint.to_string(),
            next_stage: JobStage::Discover.as_str().to_string(),
            completed_stages: Vec::new(),
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: job.schema_generation.clone(),
            model_generation: job.model_generation.clone(),
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
            updated_at: now(),
        };
        job.discovered = job
            .discovered
            .checked_add(1)
            .ok_or("Match discovered count overflow")?;
        job.updated_at = now();
        let asset_value = serde_json::to_value(&asset).map_err(|error| error.to_string())?;
        let job_value = serde_json::to_value(&job).map_err(|error| error.to_string())?;
        let payload_bytes = serde_json::to_vec(&asset_value)
            .map_err(|error| error.to_string())?
            .len()
            .checked_add(
                serde_json::to_vec(&job_value)
                    .map_err(|error| error.to_string())?
                    .len(),
            )
            .ok_or("Match discovery payload size overflow")?;
        discovery_permit.authorize_payload(payload_bytes)?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (JOB_ASSET_TABLE, &asset_id, asset_value),
                (JOB_TABLE, job_id, job_value),
            ],
            &[],
        )?;
        Ok(asset)
    }

    /// Persist a failure that occurs before a source fingerprint can be
    /// established. The opaque media key and unavailable fingerprint are
    /// stable across retry, while the canonical source path remains confined
    /// to operator-only snapshots.
    #[cfg(test)]
    fn record_discovery_failure_for_test(
        &self,
        job_id: &str,
        media_key: &str,
        source_path: Option<&Path>,
        code: &str,
        message: &str,
    ) -> Result<JobAsset, String> {
        let permit = self.acquire_discovery_write(
            job_id,
            media_key,
            ResourceRequest {
                admitted_items: 1,
                queued_items: 1,
                queued_bytes: 64 * 1024,
                surreal_writes: 1,
                ..ResourceRequest::default()
            },
        )?;
        self.record_discovery_failure(job_id, media_key, source_path, code, message, &permit)
    }

    pub fn record_discovery_failure(
        &self,
        job_id: &str,
        media_key: &str,
        source_path: Option<&Path>,
        code: &str,
        message: &str,
        discovery_permit: &MatchDiscoveryPermit,
    ) -> Result<JobAsset, String> {
        validate_media_key(media_key)?;
        validate_failure_code(code)?;
        validate_text("discovery failure message", message)?;
        let _discovery_use = discovery_permit.consume_for(&self.session_id, job_id, media_key)?;
        let _guard = self.mutation_write_guard("Match discovery failure")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !job.lifecycle()?.accepts_discovery_result() {
            return Err(
                "terminal or non-runnable IndexJob cannot record discovery failure".to_string(),
            );
        }
        let root: MatchIndexRoot =
            self.require_unlocked(ROOT_CONFIG_TABLE, &job.root_key, "Match index root")?;
        // This is diagnostic provenance for a failed observation, never a
        // source-identity proof. Retry must revalidate through the isolated worker.
        let canonical_root = Path::new(&root.path);
        let canonical_source = source_path
            .filter(|path| {
                path.is_absolute()
                    && path.starts_with(canonical_root)
                    && !path.components().any(|component| {
                        matches!(component, Component::ParentDir | Component::CurDir)
                    })
            })
            .map(|path| path.to_string_lossy().to_string());
        let asset_id = job_asset_id(job_id, media_key);
        let placeholder = format!("unavailable:{:x}", Sha256::digest(media_key.as_bytes()));
        let mut asset = self
            .get_one_unlocked::<JobAsset>(JOB_ASSET_TABLE, &asset_id)?
            .unwrap_or_else(|| JobAsset {
                asset_id: asset_id.clone(),
                job_id: job_id.to_string(),
                media_key: media_key.to_string(),
                source_path: canonical_source.clone(),
                media_fingerprint: placeholder,
                next_stage: JobStage::Discover.as_str().to_string(),
                completed_stages: Vec::new(),
                failure_code: None,
                failure_message: None,
                skipped_code: None,
                skipped_message: None,
                schema_generation: job.schema_generation.clone(),
                model_generation: job.model_generation.clone(),
                identity_revision: job.identity_revision,
                catalog_revision: job.catalog_revision,
                updated_at: now(),
            });
        if asset.failure_code.is_some() {
            discovery_permit.authorize_payload(
                serde_json::to_vec(&asset)
                    .map_err(|error| error.to_string())?
                    .len(),
            )?;
            return Ok(asset);
        }
        let is_new = self
            .get_one_unlocked::<JobAsset>(JOB_ASSET_TABLE, &asset_id)?
            .is_none();
        asset.source_path = asset.source_path.or(canonical_source);
        asset.failure_code = Some(code.to_string());
        asset.failure_message = Some(message.chars().take(2048).collect());
        asset.updated_at = now();
        if is_new {
            job.discovered = job
                .discovered
                .checked_add(1)
                .ok_or("Match discovered count overflow")?;
        }
        job.failed = job
            .failed
            .checked_add(1)
            .ok_or("Match failed count overflow")?;
        job.updated_at = now();
        let asset_value = serde_json::to_value(&asset).map_err(|error| error.to_string())?;
        let job_value = serde_json::to_value(&job).map_err(|error| error.to_string())?;
        let payload_bytes = serde_json::to_vec(&asset_value)
            .map_err(|error| error.to_string())?
            .len()
            .checked_add(
                serde_json::to_vec(&job_value)
                    .map_err(|error| error.to_string())?
                    .len(),
            )
            .ok_or("Match discovery payload size overflow")?;
        discovery_permit.authorize_payload(payload_bytes)?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (JOB_ASSET_TABLE, &asset_id, asset_value),
                (JOB_TABLE, job_id, job_value),
            ],
            &[],
        )?;
        Ok(asset)
    }

    /// Remove a pre-fingerprint discovery placeholder only after the retried
    /// walker has positively observed that same entry. This keeps vanished or
    /// still-unreadable sources in failure accounting while allowing recovered
    /// directory-walk errors to clear without becoming media assets.
    #[cfg(test)]
    fn resolve_discovery_failure_for_test(
        &self,
        job_id: &str,
        media_key: &str,
    ) -> Result<bool, String> {
        let permit = self.acquire_discovery_write(
            job_id,
            media_key,
            ResourceRequest {
                admitted_items: 1,
                queued_items: 1,
                queued_bytes: 64 * 1024,
                surreal_writes: 1,
                ..ResourceRequest::default()
            },
        )?;
        self.resolve_discovery_failure(job_id, media_key, &permit)
    }

    pub fn resolve_discovery_failure(
        &self,
        job_id: &str,
        media_key: &str,
        discovery_permit: &MatchDiscoveryPermit,
    ) -> Result<bool, String> {
        validate_media_key(media_key)?;
        let _discovery_use = discovery_permit.consume_for(&self.session_id, job_id, media_key)?;
        let _guard = self.mutation_write_guard("Match discovery resolution")?;
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !job.lifecycle()?.accepts_discovery_result() {
            return Err(
                "terminal or non-runnable IndexJob cannot resolve discovery failure".to_string(),
            );
        }
        let asset_id = job_asset_id(job_id, media_key);
        let Some(asset) = self.get_one_unlocked::<JobAsset>(JOB_ASSET_TABLE, &asset_id)? else {
            discovery_permit.authorize_payload(1)?;
            return Ok(false);
        };
        if !asset.media_fingerprint.starts_with("unavailable:")
            || asset.next_stage()? != JobStage::Discover
            || !asset.completed_stages.is_empty()
            || asset.failure_code.is_none()
        {
            discovery_permit.authorize_payload(
                serde_json::to_vec(&asset)
                    .map_err(|error| error.to_string())?
                    .len(),
            )?;
            return Ok(false);
        }
        job.discovered = job
            .discovered
            .checked_sub(1)
            .ok_or("Match discovered count underflow while resolving discovery")?;
        job.failed = job
            .failed
            .checked_sub(1)
            .ok_or("Match failed count underflow while resolving discovery")?;
        job.updated_at = now();
        let job_value = serde_json::to_value(&job).map_err(|error| error.to_string())?;
        let payload_bytes = serde_json::to_vec(&job_value)
            .map_err(|error| error.to_string())?
            .len()
            .checked_add(asset_id.len())
            .ok_or("Match discovery payload size overflow")?;
        discovery_permit.authorize_payload(payload_bytes)?;
        self.transactional_upserts_deletes_unlocked(
            &[(JOB_TABLE, job_id, job_value)],
            &[(JOB_ASSET_TABLE, &asset_id)],
        )?;
        Ok(true)
    }

    pub fn commit_asset_stage(
        &self,
        asset_id: &str,
        stage: JobStage,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<JobAsset, String> {
        let mut stage_use = stage_permit.consume_for(&self.session_id, fence, stage)?;
        let _guard = self.mutation_write_guard("Match stage commit")?;
        let mut asset: JobAsset = self.require_unlocked(JOB_ASSET_TABLE, asset_id, "job asset")?;
        let current_asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if current_asset.asset_id != asset.asset_id {
            return Err("job asset fence resolves to a different asset".to_string());
        }
        if asset
            .completed_stages
            .iter()
            .any(|done| done == stage.as_str())
        {
            stage_permit.authorize_payload(
                serde_json::to_vec(&asset)
                    .map_err(|error| error.to_string())?
                    .len(),
            )?;
            stage_use.success();
            return Ok(asset);
        }
        if current_asset.next_stage()? != stage_permit.stage {
            return Err("stage commit no longer matches the durable stage cursor".to_string());
        }
        if asset.next_stage()? != stage {
            return Err(format!(
                "job stage out of order: expected {}, got {}",
                asset.next_stage,
                stage.as_str()
            ));
        }
        let had_failure = asset.failure_code.is_some();
        let had_skip = asset.skipped_code.is_some();
        asset.completed_stages.push(stage.as_str().to_string());
        asset.next_stage = stage.next().as_str().to_string();
        asset.failure_code = None;
        asset.failure_message = None;
        asset.skipped_code = None;
        asset.skipped_message = None;
        asset.updated_at = now();
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        if had_failure {
            job.failed = job.failed.saturating_sub(1);
        }
        if had_skip {
            job.skipped = job.skipped.saturating_sub(1);
        }
        if stage == JobStage::Complete {
            job.completed = job
                .completed
                .checked_add(1)
                .ok_or("Match completed count overflow")?;
        }
        job.updated_at = now();
        stage_permit.authorize_payload(
            serde_json::to_vec(&(&asset, &job))
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    JOB_ASSET_TABLE,
                    asset_id,
                    serde_json::to_value(&asset).map_err(|error| error.to_string())?,
                ),
                (
                    JOB_TABLE,
                    &job.job_id,
                    serde_json::to_value(&job).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        stage_use.success();
        Ok(asset)
    }

    pub fn record_asset_failure(
        &self,
        asset_id: &str,
        code: &str,
        message: &str,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<JobAsset, String> {
        let mut stage_use = stage_permit.consume_for_any_stage(&self.session_id, fence)?;
        validate_failure_code(code)?;
        validate_text("failure message", message)?;
        let _guard = self.mutation_write_guard("Match failure commit")?;
        let mut asset: JobAsset = self.require_unlocked(JOB_ASSET_TABLE, asset_id, "job asset")?;
        let current_asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if current_asset.asset_id != asset.asset_id {
            return Err("job asset fence resolves to a different asset".to_string());
        }
        if current_asset.next_stage()? != stage_permit.stage {
            return Err("failure write no longer matches the durable stage cursor".to_string());
        }
        let first_failure = asset.failure_code.is_none();
        asset.failure_code = Some(code.to_string());
        asset.failure_message = Some(message.to_string());
        asset.updated_at = now();
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        if first_failure {
            job.failed = job
                .failed
                .checked_add(1)
                .ok_or("Match failed count overflow")?;
        }
        job.updated_at = now();
        stage_permit.authorize_payload(
            serde_json::to_vec(&(&asset, &job))
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    JOB_ASSET_TABLE,
                    asset_id,
                    serde_json::to_value(&asset).map_err(|error| error.to_string())?,
                ),
                (
                    JOB_TABLE,
                    &job.job_id,
                    serde_json::to_value(&job).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        stage_use.success();
        Ok(asset)
    }

    pub fn record_asset_skipped(
        &self,
        asset_id: &str,
        code: &str,
        message: &str,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<JobAsset, String> {
        let mut stage_use = stage_permit.consume_for_any_stage(&self.session_id, fence)?;
        validate_failure_code(code)?;
        validate_text("skip message", message)?;
        let _guard = self.mutation_write_guard("Match skip commit")?;
        let mut asset: JobAsset = self.require_unlocked(JOB_ASSET_TABLE, asset_id, "job asset")?;
        let current_asset = self.require_valid_asset_fence_unlocked(fence, false)?;
        if current_asset.asset_id != asset.asset_id
            || current_asset.next_stage()? != stage_permit.stage
        {
            return Err("skip write no longer matches the durable stage cursor".to_string());
        }
        let first_skip = asset.skipped_code.is_none();
        asset.skipped_code = Some(code.to_string());
        asset.skipped_message = Some(message.to_string());
        asset.updated_at = now();
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        if first_skip {
            job.skipped = job
                .skipped
                .checked_add(1)
                .ok_or("Match skipped count overflow")?;
        }
        job.updated_at = now();
        stage_permit.authorize_payload(
            serde_json::to_vec(&(&asset, &job))
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    JOB_ASSET_TABLE,
                    asset_id,
                    serde_json::to_value(&asset).map_err(|error| error.to_string())?,
                ),
                (
                    JOB_TABLE,
                    &job.job_id,
                    serde_json::to_value(&job).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        stage_use.success();
        Ok(asset)
    }

    pub fn publish_projection(
        &self,
        projection: PeopleProjection,
        current: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<(), String> {
        let mut stage_use =
            stage_permit.consume_for(&self.session_id, current, JobStage::Persist)?;
        let deadline = stage_permit.admitted_at + crate::match_worker::SAFE_UNIT_LIMIT;
        require_persist_time_remaining(deadline)?;
        stage_permit.authorize_payload(
            serde_json::to_vec(&projection)
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        if projection.media_key != current.media_key
            || projection.media_fingerprint != current.media_fingerprint
            || projection.schema_generation != current.schema_generation
            || projection.model_generation != current.model_generation
            || projection.identity_revision != current.identity_revision
            || projection.catalog_revision != current.catalog_revision
        {
            return Err("stale People projection revision fence".to_string());
        }
        let _guard = self.persist_write_guard(deadline)?;
        let asset_id = job_asset_id(&current.job_id, &current.media_key);
        let mut asset: JobAsset = self.require_unlocked(JOB_ASSET_TABLE, &asset_id, "job asset")?;
        let current_asset = self.require_valid_asset_fence_unlocked(current, true)?;
        if current_asset.asset_id != asset.asset_id {
            return Err("projection fence resolves to a different asset".to_string());
        }
        if current_asset.next_stage()? != JobStage::Persist {
            return Err("projection write no longer matches the durable stage cursor".to_string());
        }
        let mut job: IndexJob = self.require_unlocked(JOB_TABLE, &asset.job_id, "IndexJob")?;
        if asset.failure_code.is_some() {
            job.failed = job.failed.saturating_sub(1);
        }
        if asset.skipped_code.is_some() {
            job.skipped = job.skipped.saturating_sub(1);
        }
        asset
            .completed_stages
            .push(JobStage::Persist.as_str().to_string());
        asset.next_stage = JobStage::Suggest.as_str().to_string();
        asset.failure_code = None;
        asset.failure_message = None;
        asset.skipped_code = None;
        asset.skipped_message = None;
        asset.updated_at = now();
        job.updated_at = asset.updated_at.clone();
        stage_permit.authorize_payload(
            serde_json::to_vec(&(&projection, &asset, &job))
                .map_err(|error| error.to_string())?
                .len(),
        )?;
        require_persist_time_remaining(deadline)?;
        // One checkpoint: never expose a projection whose Persist cursor was
        // not committed with it. Keep both leases until the engine responds.
        self.transactional_upserts_deletes_unlocked(
            &[
                (
                    PROJECTION_TABLE,
                    &projection.media_key,
                    serde_json::to_value(&projection).map_err(|error| error.to_string())?,
                ),
                (
                    JOB_ASSET_TABLE,
                    &asset_id,
                    serde_json::to_value(&asset).map_err(|error| error.to_string())?,
                ),
                (
                    JOB_TABLE,
                    &job.job_id,
                    serde_json::to_value(&job).map_err(|error| error.to_string())?,
                ),
            ],
            &[],
        )?;
        self.cache_projection(projection)?;
        stage_use.success();
        drop(_guard);
        Ok(())
    }

    pub fn cached_projection(&self, media_key: &str) -> Result<Option<PeopleProjection>, String> {
        let caches = self
            .caches
            .read()
            .map_err(|_| "Match projection cache is poisoned".to_string())?;
        Ok(caches
            .projections
            .get(media_key)
            .filter(|projection| {
                projection.identity_revision == caches.identity_revision
                    && projection.catalog_revision == caches.catalog_revision
            })
            .cloned())
    }

    /// Off-render cache warm path for a media item that was not among the most
    /// recent projections hydrated at startup. Paint remains cache-only.
    pub fn warm_projection(&self, media_key: &str) -> Result<Option<PeopleProjection>, String> {
        validate_media_key(media_key)?;
        let _guard = self.database_read_guard("Match projection warm lock is poisoned")?;
        let execution = self.execution_state_unlocked()?;
        let projection = self
            .get_one_unlocked::<PeopleProjection>(PROJECTION_TABLE, media_key)?
            .filter(|projection| {
                projection.identity_revision == execution.identity_revision
                    && projection.catalog_revision == execution.catalog_revision
            });
        if let Some(projection) = projection.clone() {
            self.cache_projection(projection)?;
        }
        drop(_guard);
        Ok(projection)
    }

    fn refresh_projection_cache(&self) -> Result<usize, String> {
        let _guard = self.database_read_guard("Match projection hydration lock is poisoned")?;
        let execution = self.execution_state_unlocked()?;
        let db = self.database();
        let identity_revision = execution.identity_revision;
        let catalog_revision = execution.catalog_revision;
        let mut projections: Vec<PeopleProjection> = surreal_store::run(async move {
            let mut response = db
                .query(
                    "SELECT * OMIT id FROM match_people_projection \
                     WHERE identity_revision = $identity_revision \
                     AND catalog_revision = $catalog_revision \
                     ORDER BY published_at DESC, media_key ASC LIMIT 4096;",
                )
                .bind(("identity_revision", identity_revision))
                .bind(("catalog_revision", catalog_revision))
                .await
                .map_err(|error| format!("hydrate Match projection cache: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match projection cache: {error}"))
        })?;
        projections.truncate(PROJECTION_CACHE_CAPACITY);
        let hydrated = projections.len();
        let mut cache = BTreeMap::new();
        for projection in projections {
            cache.insert(projection.media_key.clone(), projection);
        }
        self.caches
            .write()
            .map_err(|_| "Match projection cache is poisoned".to_string())?
            .projections = cache;
        drop(_guard);
        Ok(hydrated)
    }

    fn cache_projection(&self, projection: PeopleProjection) -> Result<(), String> {
        let mut caches = self.cache_write_recover();
        // Callers hold the store transaction lock and supply canonical rows.
        // Recovery discards cache revisions too, so restore the current fence
        // together with the row instead of reporting a committed write failed.
        caches.identity_revision = projection.identity_revision;
        caches.catalog_revision = projection.catalog_revision;
        insert_bounded_projection(&mut caches.projections, projection);
        Ok(())
    }

    pub fn operator_mutation_fence(
        &self,
        face_id: &str,
        person_id: &str,
    ) -> Result<OperatorMutationFence, String> {
        let _guard = self.database_read_guard("Match operator-fence lock is poisoned")?;
        let face: FaceObservation =
            self.require_unlocked(FACE_TABLE, face_id, "FaceObservation")?;
        let person: Person = self.require_unlocked(PERSON_TABLE, person_id, "Person")?;
        let assignment = self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?;
        let execution = self.execution_state_unlocked()?;
        Ok(OperatorMutationFence {
            face_id: face.face_id,
            person_id: person.person_id,
            face_revision: face.face_revision,
            person_revision: person.revision,
            assignment_operation_id: assignment.map(|value| value.operation_id),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    /// Match caches are regenerable projections, never mutation authority.
    /// Recover a cache-only panic by discarding every possibly partial entry;
    /// callers then repopulate only the projection they own from canonical DB
    /// rows. Durable mutation verdicts must never depend on this lock's health.
    fn cache_write_recover(&self) -> std::sync::RwLockWriteGuard<'_, MatchCaches> {
        match self.caches.write() {
            Ok(caches) => caches,
            Err(poisoned) => {
                let mut caches = poisoned.into_inner();
                *caches = MatchCaches::default();
                self.caches.clear_poison();
                caches
            }
        }
    }

    pub fn refresh_autocomplete(&self) -> Result<u64, String> {
        #[cfg(test)]
        if self
            .autocomplete_refresh_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err("injected Match autocomplete refresh failure".to_string());
        }
        let _guard = self.database_read_guard("Match autocomplete refresh lock is poisoned")?;
        let persons = self.list_unlocked::<Person>(PERSON_TABLE)?;
        let execution = self.execution_state_unlocked()?;
        let catalog_revision = execution.catalog_revision;
        let mut entries = Vec::new();
        for person in persons {
            let mut terms = vec![person.name.clone()];
            terms.extend(person.aliases.clone());
            let search = terms.join("\n").to_lowercase();
            entries.push(AutocompleteEntry {
                person_id: person.person_id,
                display_name: person.name,
                search,
            });
        }
        entries.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
                .then_with(|| a.person_id.cmp(&b.person_id))
        });
        let mut caches = self.cache_write_recover();
        caches.autocomplete = AutocompleteIndex {
            valid: true,
            catalog_revision,
            entries,
        };
        caches.identity_revision = execution.identity_revision;
        caches.catalog_revision = catalog_revision;
        drop(caches);
        drop(_guard);
        Ok(catalog_revision)
    }

    pub fn autocomplete(
        &self,
        query: &str,
        expected_catalog_revision: u64,
        limit: usize,
    ) -> Result<Vec<AutocompleteResult>, String> {
        let query = query.trim().to_lowercase();
        let needs_canonical_rebuild = match self.caches.read() {
            Ok(caches) => !caches.autocomplete.valid,
            // `refresh_autocomplete` takes the write side and recovers poison;
            // autocomplete remains a projection over canonical Person rows.
            Err(_) => true,
        };
        if needs_canonical_rebuild {
            self.refresh_autocomplete()?;
        }
        let caches = self
            .caches
            .read()
            .map_err(|_| "Match autocomplete cache is poisoned".to_string())?;
        if !caches.autocomplete.valid
            || caches.autocomplete.catalog_revision != expected_catalog_revision
        {
            return Err("stale autocomplete catalog revision".to_string());
        }
        Ok(caches
            .autocomplete
            .entries
            .iter()
            .filter(|entry| query.is_empty() || entry.search.contains(&query))
            .take(limit.min(100))
            .map(|entry| AutocompleteResult {
                person_id: entry.person_id.clone(),
                display_name: entry.display_name.clone(),
                catalog_revision: caches.autocomplete.catalog_revision,
            })
            .collect())
    }

    pub fn nearest(
        &self,
        query_vector: &[f32],
        model_generation: &str,
        candidate_k: usize,
        rerank_k: usize,
    ) -> Result<NeighborQuery, String> {
        crate::match_benchmark::note_index_query();
        validate_vector(query_vector)?;
        let candidate_k = candidate_k.clamp(1, 1000);
        let rerank_k = rerank_k.clamp(1, candidate_k);
        let ef = strict_hnsw_ef_search(candidate_k);
        let sql = format!(
            "SELECT embedding_id, face_id, vector FROM {EMBEDDING_TABLE} WITH INDEX {EMBEDDING_INDEX} WHERE vector <|{candidate_k},{ef}|> $query AND active = true AND model_generation = $generation;"
        );
        let explain_sql = format!(
            "SELECT embedding_id FROM {EMBEDDING_TABLE} WITH INDEX {EMBEDDING_INDEX} WHERE vector <|{candidate_k},{ef}|> $query AND active = true AND model_generation = $generation EXPLAIN FULL;"
        );
        let _guard = self.database_read_guard("Match vector read lock is poisoned")?;
        let db = self.database();
        let query_owned = query_vector.to_vec();
        let generation = model_generation.to_string();
        let (rows, plan): (Vec<Value>, Value) = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("query", query_owned.clone()))
                .bind(("generation", generation.clone()))
                .await
                .map_err(|error| format!("query Match HNSW candidates: {error}"))?;
            let rows: Vec<Value> = response
                .take(0)
                .map_err(|error| format!("decode Match HNSW candidates: {error}"))?;
            let mut explain = db
                .query(explain_sql)
                .bind(("query", query_owned))
                .bind(("generation", generation))
                .await
                .map_err(|error| format!("explain Match HNSW query: {error}"))?;
            let plan_rows: Vec<Value> = explain
                .take(0)
                .map_err(|error| format!("decode Match HNSW plan: {error}"))?;
            Ok((rows, Value::Array(plan_rows)))
        })?;
        let mut neighbors = Vec::with_capacity(rows.len());
        for row in rows {
            let embedding_id = required_string(&row, "embedding_id")?;
            let face_id = required_string(&row, "face_id")?;
            let vector: Vec<f32> = serde_json::from_value(
                row.get("vector")
                    .cloned()
                    .ok_or("HNSW candidate omitted source vector")?,
            )
            .map_err(|error| format!("decode HNSW source vector: {error}"))?;
            let exact_cosine = exact_cosine(query_vector, &vector)?;
            neighbors.push(Neighbor {
                embedding_id,
                face_id,
                exact_cosine,
            });
        }
        let candidate_count = neighbors.len();
        neighbors.sort_by(|a, b| {
            b.exact_cosine
                .total_cmp(&a.exact_cosine)
                .then_with(|| a.embedding_id.cmp(&b.embedding_id))
        });
        neighbors.truncate(rerank_k);
        let plan_uses_hnsw = json_contains(&plan, EMBEDDING_INDEX)
            && (json_contains(&plan, "Iterate Index") || json_contains(&plan, "KnnScan"));
        self.caches
            .write()
            .map_err(|_| "Match query-plan cache is poisoned".to_string())?
            .last_query_plan = Some(QueryPlanEvidence {
            observed: true,
            index: EMBEDDING_INDEX.to_string(),
            uses_hnsw: plan_uses_hnsw,
            exact_rerank: true,
            model_generation: model_generation.to_string(),
            observed_at: now(),
        });
        Ok(NeighborQuery {
            neighbors,
            plan_uses_hnsw,
            plan,
            candidate_count,
        })
    }

    /// HNSW candidate search over the derived trusted-only table. Untrusted
    /// embeddings cannot consume candidate K before exact reranking.
    pub fn nearest_trusted(
        &self,
        query_vector: &[f32],
        model_generation: &str,
        candidate_k: usize,
        rerank_k: usize,
    ) -> Result<TrustedNeighborQuery, String> {
        crate::match_benchmark::note_index_query();
        validate_vector(query_vector)?;
        let candidate_k = candidate_k.clamp(1, 1000);
        let rerank_k = rerank_k.clamp(1, candidate_k);
        let ef = candidate_k.saturating_mul(4).clamp(40, 4000);
        let index = "match_trusted_search_hnsw_v1";
        let sql = format!(
            "SELECT membership_id, person_id, look_id, face_id, embedding_id, vector FROM {TRUSTED_SEARCH_TABLE} WITH INDEX {index} WHERE vector <|{candidate_k},{ef}|> $query AND model_generation = $generation;"
        );
        let explain_sql = format!(
            "SELECT membership_id FROM {TRUSTED_SEARCH_TABLE} WITH INDEX {index} WHERE vector <|{candidate_k},{ef}|> $query AND model_generation = $generation EXPLAIN FULL;"
        );
        let _guard = self.database_read_guard("Match trusted vector read lock is poisoned")?;
        let build_receipt: TrustedIndexBuildReceipt = self.require_unlocked(
            TRUSTED_INDEX_BUILD_TABLE,
            "global",
            "current trusted index build receipt",
        )?;
        if build_receipt.engine_version != STRICT_ANN_ENGINE_VERSION
            || build_receipt.build_seed != 0
            || build_receipt.build_order != STRICT_ANN_BUILD_ORDER
        {
            return Err("trusted search index build receipt is stale or incompatible".to_string());
        }
        let db = self.database();
        let query_owned = query_vector.to_vec();
        let generation = model_generation.to_string();
        let (rows, plan): (Vec<Value>, Value) = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("query", query_owned.clone()))
                .bind(("generation", generation.clone()))
                .await
                .map_err(|error| format!("query trusted Match HNSW candidates: {error}"))?;
            let rows: Vec<Value> = response
                .take(0)
                .map_err(|error| format!("decode trusted Match HNSW candidates: {error}"))?;
            let mut explain = db
                .query(explain_sql)
                .bind(("query", query_owned))
                .bind(("generation", generation))
                .await
                .map_err(|error| format!("explain trusted Match HNSW query: {error}"))?;
            let plan_rows: Vec<Value> = explain
                .take(0)
                .map_err(|error| format!("decode trusted Match HNSW plan: {error}"))?;
            Ok((rows, Value::Array(plan_rows)))
        })?;
        let mut neighbors = Vec::with_capacity(rows.len());
        for row in rows {
            let vector: Vec<f32> = serde_json::from_value(
                row.get("vector")
                    .cloned()
                    .ok_or("trusted HNSW candidate omitted source vector")?,
            )
            .map_err(|error| format!("decode trusted HNSW source vector: {error}"))?;
            neighbors.push(TrustedNeighbor {
                membership_id: required_string(&row, "membership_id")?,
                person_id: required_string(&row, "person_id")?,
                look_id: required_string(&row, "look_id")?,
                face_id: required_string(&row, "face_id")?,
                embedding_id: required_string(&row, "embedding_id")?,
                exact_cosine: exact_cosine(query_vector, &vector)?,
            });
        }
        let candidate_count = neighbors.len();
        neighbors.sort_by(|left, right| {
            right
                .exact_cosine
                .total_cmp(&left.exact_cosine)
                .then_with(|| left.person_id.cmp(&right.person_id))
                .then_with(|| left.look_id.cmp(&right.look_id))
                .then_with(|| left.embedding_id.cmp(&right.embedding_id))
        });
        neighbors.truncate(rerank_k);
        let plan_uses_hnsw = json_contains(&plan, index)
            && (json_contains(&plan, "Iterate Index") || json_contains(&plan, "KnnScan"));
        Ok(TrustedNeighborQuery {
            neighbors,
            plan_uses_hnsw,
            plan,
            candidate_count,
        })
    }

    pub fn trusted_gallery_members_digest(&self) -> Result<String, String> {
        let _guard = self.database_read_guard("Match gallery digest lock is poisoned")?;
        self.trusted_gallery_members_digest_unlocked()
    }

    pub fn has_active_strict_calibration(&self, model_generation: &str) -> Result<bool, String> {
        let _database_unit = self.begin_database_unit()?;
        Ok(self
            .list::<CalibrationActivation>(CALIBRATION_TABLE)?
            .into_iter()
            .any(|activation| {
                activation.active
                    && activation.model_generation == model_generation
                    && activation.verifier_verdict == "pass"
                    && activation.independent_review_verdict == "pass"
                    && activation.wp084_runtime_ready
                    && activation.wp087_release_ready
                    && activation.invalidation_reason.is_none()
            }))
    }

    fn trusted_gallery_members_digest_unlocked(&self) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let search = self.list_unlocked::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)?;
        let memberships = self.list_unlocked::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)?;
        let mut rows = search
            .iter()
            .map(|row| {
                let set_id = memberships
                    .iter()
                    .find(|membership| membership.membership_id == row.membership_id)
                    .map(|membership| membership.set_id.as_str())
                    .ok_or("trusted search row has no durable membership")?;
                Ok(format!(
                    "{}\0{}\0{}\0{}",
                    row.person_id, row.look_id, set_id, row.face_id
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        rows.sort();
        Ok(format!("{:x}", Sha256::digest(rows.join("\n").as_bytes())))
    }

    fn validate_gallery_envelope_unlocked(
        &self,
        model_generation: &str,
        people_max: usize,
        looks_per_person_max: usize,
        templates_per_look_max: usize,
        total_templates_max: usize,
    ) -> Result<(), String> {
        let search = self.list_unlocked::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)?;
        if search.len() > total_templates_max
            || search
                .iter()
                .any(|row| row.model_generation != model_generation)
        {
            return Err("live trusted gallery is outside the verified envelope".to_string());
        }
        let mut people = BTreeSet::new();
        let mut looks_by_person: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut templates_by_look: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for row in &search {
            people.insert(row.person_id.as_str());
            looks_by_person
                .entry(row.person_id.as_str())
                .or_default()
                .insert(row.look_id.as_str());
            *templates_by_look
                .entry((row.person_id.as_str(), row.look_id.as_str()))
                .or_default() += 1;
        }
        if people.len() > people_max
            || looks_by_person
                .values()
                .any(|looks| looks.len() > looks_per_person_max)
            || templates_by_look
                .values()
                .any(|count| *count > templates_per_look_max)
        {
            return Err("live trusted gallery exceeds the verified cardinality limits".to_string());
        }
        Ok(())
    }

    fn validate_gallery_envelope(
        &self,
        model_generation: &str,
        people_max: usize,
        looks_per_person_max: usize,
        templates_per_look_max: usize,
        total_templates_max: usize,
    ) -> Result<(), String> {
        let _guard = self.database_read_guard("Match gallery envelope lock is poisoned")?;
        self.validate_gallery_envelope_unlocked(
            model_generation,
            people_max,
            looks_per_person_max,
            templates_per_look_max,
            total_templates_max,
        )
    }

    /// The only production route that may persist a strict-automatic
    /// assignment. It reads the face's persisted active embedding, queries the
    /// trusted-only HNSW source, exact-reranks templates, aggregates by Look and
    /// then Person, applies cannot-link/quality/margin/envelope gates from the
    /// verified activation, and finally uses the original revision fence. Any
    /// concurrent gallery mutation bumps that fence and makes persistence fail.
    pub fn recognize_and_persist_strict(
        &self,
        face_id: &str,
        fence: &RevisionFence,
        stage_permit: &MatchStagePermit,
    ) -> Result<StrictRecognitionOutcome, String> {
        let _database_unit = stage_permit.begin_execution_scope()?;
        let face: FaceObservation = self.require(FACE_TABLE, face_id, "FaceObservation")?;
        if face.media_key != fence.media_key
            || face.media_fingerprint != fence.media_fingerprint
            || face.face_revision == 0
            || fence.model_generation.trim().is_empty()
        {
            return Err("strict recognition face/fence mismatch".to_string());
        }
        let activation = self
            .list::<CalibrationActivation>(CALIBRATION_TABLE)?
            .into_iter()
            .find(|activation| {
                activation.active && activation.model_generation == fence.model_generation
            })
            .ok_or("strict automatic calibration is not active")?;
        if activation.verifier_verdict != "pass"
            || activation.independent_review_verdict != "pass"
            || !activation.wp084_runtime_ready
            || !activation.wp087_release_ready
            || activation.invalidation_reason.is_some()
        {
            return Err("strict automatic calibration prerequisites are not current".to_string());
        }
        if activation.activation_integrity_digest
            != calibration_activation_integrity_digest(&activation)
        {
            return Err("strict automatic activation integrity check failed".to_string());
        }
        {
            let _guard = self.database_read_guard("Match face-disposition lock is poisoned")?;
            if self
                .get_one_unlocked::<Value>(FACE_DISPOSITION_TABLE, face_id)?
                .is_some()
            {
                return Ok(StrictRecognitionOutcome {
                    state: "suppressed".to_string(),
                    person_id: None,
                    winning_look_id: None,
                    winning_face_id: None,
                    similarity: None,
                    runner_up_margin: None,
                    calibration_generation: activation.calibration_generation,
                    envelope_hash: activation.envelope_hash,
                });
            }
        }
        let build_receipt: TrustedIndexBuildReceipt = self.require(
            TRUSTED_INDEX_BUILD_TABLE,
            "global",
            "trusted index build receipt",
        )?;
        if build_receipt.build_digest != activation.trusted_index_build_digest
            || build_receipt.engine_version != STRICT_ANN_ENGINE_VERSION
            || build_receipt.build_seed != 0
            || build_receipt.build_order != STRICT_ANN_BUILD_ORDER
        {
            return Err("strict automatic trusted-index build receipt drifted".to_string());
        }
        if activation.runtime_configuration_digest
            != strict_runtime_configuration_digest(activation.candidate_k, activation.rerank_k)
        {
            return Err("strict automatic runtime configuration drifted".to_string());
        }
        if !face.alignment_valid || !is_real_yaw_bucket(&face.pose_bucket) {
            return Err("strict automatic query failed alignment or pose gates".to_string());
        }
        let current_digest = self.trusted_gallery_members_digest()?;
        if current_digest != activation.gallery_members_digest {
            return Err("trusted gallery composition drifted from calibration".to_string());
        }
        self.validate_gallery_envelope(
            &activation.model_generation,
            activation.people_max,
            activation.looks_per_person_max,
            activation.templates_per_look_max,
            activation.total_templates_max,
        )?;
        let embedding_id = embedding_id(face_id, &fence.model_generation);
        let embedding: FaceEmbedding =
            self.require(EMBEDDING_TABLE, &embedding_id, "strict query embedding")?;
        let job: IndexJob = self.require(JOB_TABLE, &fence.job_id, "strict query job")?;
        let asset: JobAsset = self.require(
            JOB_ASSET_TABLE,
            &job_asset_id(&fence.job_id, &fence.media_key),
            "strict query job asset",
        )?;
        validate_asset_fence(&asset, fence)?;
        validate_embedding_job_provenance(&embedding, &face, &job, &asset, fence)
            .map_err(|error| format!("strict query embedding is stale: {error}"))?;
        validate_vector(&embedding.vector)?;
        let query = self.nearest_trusted(
            &embedding.vector,
            &fence.model_generation,
            activation.candidate_k,
            activation.rerank_k,
        )?;
        if !query.plan_uses_hnsw {
            return Err(
                "trusted candidate query did not execute the frozen HNSW index".to_string(),
            );
        }
        let blocked = self
            .list::<CannotLinkConstraint>(CONSTRAINT_TABLE)?
            .into_iter()
            .filter(|constraint| constraint.face_id == face_id)
            .map(|constraint| constraint.person_id)
            .collect::<BTreeSet<_>>();
        let mut per_look: BTreeMap<(String, String), TrustedNeighbor> = BTreeMap::new();
        for neighbor in query.neighbors {
            if blocked.contains(&neighbor.person_id) {
                continue;
            }
            let key = (neighbor.person_id.clone(), neighbor.look_id.clone());
            match per_look.get(&key) {
                Some(current) if current.exact_cosine >= neighbor.exact_cosine => {}
                _ => {
                    per_look.insert(key, neighbor);
                }
            }
        }
        let mut per_person: BTreeMap<String, TrustedNeighbor> = BTreeMap::new();
        for neighbor in per_look.into_values() {
            match per_person.get(&neighbor.person_id) {
                Some(current) if current.exact_cosine >= neighbor.exact_cosine => {}
                _ => {
                    per_person.insert(neighbor.person_id.clone(), neighbor);
                }
            }
        }
        let mut ranked = per_person.into_values().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .exact_cosine
                .total_cmp(&left.exact_cosine)
                .then_with(|| left.person_id.cmp(&right.person_id))
                .then_with(|| left.look_id.cmp(&right.look_id))
                .then_with(|| left.face_id.cmp(&right.face_id))
        });
        let Some(best) = ranked.first().cloned() else {
            return Ok(StrictRecognitionOutcome {
                state: "unidentified".to_string(),
                person_id: None,
                winning_look_id: None,
                winning_face_id: None,
                similarity: None,
                runner_up_margin: None,
                calibration_generation: activation.calibration_generation,
                envelope_hash: activation.envelope_hash,
            });
        };
        let margin = ranked
            .get(1)
            .map(|runner| best.exact_cosine - runner.exact_cosine)
            .unwrap_or(f32::INFINITY);
        let person: Person = self.require(PERSON_TABLE, &best.person_id, "strict Person")?;
        let outcome = if face.quality < activation.minimum_quality
            || best.exact_cosine < activation.suggestion_threshold
        {
            StrictRecognitionOutcome {
                state: "unidentified".to_string(),
                person_id: None,
                winning_look_id: Some(best.look_id),
                winning_face_id: Some(best.face_id),
                similarity: Some(best.exact_cosine),
                runner_up_margin: Some(margin),
                calibration_generation: activation.calibration_generation,
                envelope_hash: activation.envelope_hash,
            }
        } else if best.exact_cosine >= activation.automatic_threshold
            && margin >= activation.runner_up_margin
        {
            self.assign_face(
                face_id,
                &best.person_id,
                Some(&best.look_id),
                AssignmentState::CommittedStrictAutomatic,
                "strict_recognition_v1",
                Some(&fence.model_generation),
                Some(&activation.calibration_generation),
                Some(&activation.envelope_hash),
                None,
                Some(fence),
                Some(stage_permit),
                face.face_revision,
                person.revision,
            )?;
            StrictRecognitionOutcome {
                state: AssignmentState::CommittedStrictAutomatic
                    .as_str()
                    .to_string(),
                person_id: Some(best.person_id),
                winning_look_id: Some(best.look_id),
                winning_face_id: Some(best.face_id),
                similarity: Some(best.exact_cosine),
                runner_up_margin: Some(margin),
                calibration_generation: activation.calibration_generation,
                envelope_hash: activation.envelope_hash,
            }
        } else {
            self.record_suggestion(
                Suggestion {
                    suggestion_id: suggestion_id(face_id, &best.person_id),
                    face_id: face_id.to_string(),
                    candidate_person_id: best.person_id.clone(),
                    similarity: best.exact_cosine,
                    model_generation: fence.model_generation.clone(),
                    calibration_generation: Some(activation.calibration_generation.clone()),
                    envelope_hash: Some(activation.envelope_hash.clone()),
                    media_fingerprint: face.media_fingerprint.clone(),
                    face_revision: face.face_revision,
                    person_revision: person.revision,
                    job_id: fence.job_id.clone(),
                    created_at: now(),
                },
                fence,
                stage_permit,
            )?;
            StrictRecognitionOutcome {
                state: AssignmentState::Suggestion.as_str().to_string(),
                person_id: Some(best.person_id),
                winning_look_id: Some(best.look_id),
                winning_face_id: Some(best.face_id),
                similarity: Some(best.exact_cosine),
                runner_up_margin: Some(margin),
                calibration_generation: activation.calibration_generation,
                envelope_hash: activation.envelope_hash,
            }
        };
        Ok(outcome)
    }

    /// Preserve stable face/assignment identity across a move or rename only
    /// when the caller proves the canonical source fingerprint is unchanged.
    pub fn rekey_media(
        &self,
        old_media_key: &str,
        new_media_key: &str,
        proven_fingerprint: &str,
    ) -> Result<usize, String> {
        validate_media_key(old_media_key)?;
        validate_media_key(new_media_key)?;
        validate_text("proven media fingerprint", proven_fingerprint)?;
        let canonical_proven_fingerprint =
            canonical_correction_media_fingerprint(proven_fingerprint)?;
        if old_media_key == new_media_key {
            return Ok(0);
        }
        let _guard = self.mutation_write_guard("Match media rekey")?;
        let mut faces = self
            .list_unlocked::<FaceObservation>(FACE_TABLE)?
            .into_iter()
            .filter(|face| face.media_key == old_media_key)
            .collect::<Vec<_>>();
        if faces.is_empty() {
            return Err("no Match observations use the old media key".to_string());
        }
        if faces.iter().any(|face| {
            canonical_media_sha256(&face.media_fingerprint)
                != Some(canonical_proven_fingerprint.as_str())
        }) {
            return Err(
                "media move fingerprint proof does not match every observation".to_string(),
            );
        }
        let moved_face_ids = faces
            .iter()
            .map(|face| face.face_id.clone())
            .collect::<BTreeSet<_>>();
        let mut assignments = self
            .list_unlocked::<Assignment>(ASSIGNMENT_TABLE)?
            .into_iter()
            .filter(|assignment| moved_face_ids.contains(&assignment.face_id))
            .collect::<Vec<_>>();
        if assignments
            .iter()
            .any(|assignment| assignment.media_key != old_media_key)
        {
            return Err(
                "media move assignment key does not match its face observation".to_string(),
            );
        }
        let mut dispositions = self
            .list_unlocked::<corrections::FaceDisposition>(FACE_DISPOSITION_TABLE)?
            .into_iter()
            .filter(|disposition| moved_face_ids.contains(&disposition.face_id))
            .collect::<Vec<_>>();
        if dispositions
            .iter()
            .any(|disposition| disposition.media_key != old_media_key)
        {
            return Err(
                "media move disposition key does not match its face observation".to_string(),
            );
        }
        for assignment in &mut assignments {
            assignment.media_key = new_media_key.to_string();
            assignment.updated_at = now();
        }
        for disposition in &mut dispositions {
            disposition.media_key = new_media_key.to_string();
            disposition.updated_at = now();
        }
        for face in &mut faces {
            face.media_key = new_media_key.to_string();
            face.updated_at = now();
        }

        let old_projection =
            self.get_one_unlocked::<PeopleProjection>(PROJECTION_TABLE, old_media_key)?;
        if self
            .get_one_unlocked::<PeopleProjection>(PROJECTION_TABLE, new_media_key)?
            .is_some()
        {
            return Err("new media key already has a Match projection".to_string());
        }
        let mut moved_projection = old_projection.clone();
        if let Some(projection) = &mut moved_projection {
            if canonical_media_sha256(&projection.media_fingerprint)
                != Some(canonical_proven_fingerprint.as_str())
            {
                return Err("media move fingerprint proof does not match projection".to_string());
            }
            projection.media_key = new_media_key.to_string();
            projection.published_at = now();
        }

        let mut old_job_asset_ids = Vec::new();
        let mut moved_job_assets = Vec::new();
        for mut asset in self
            .list_unlocked::<JobAsset>(JOB_ASSET_TABLE)?
            .into_iter()
            .filter(|asset| asset.media_key == old_media_key)
        {
            if canonical_media_sha256(&asset.media_fingerprint)
                != Some(canonical_proven_fingerprint.as_str())
            {
                return Err("media move fingerprint proof does not match job asset".to_string());
            }
            let old_id = asset.asset_id.clone();
            asset.media_key = new_media_key.to_string();
            asset.asset_id = job_asset_id(&asset.job_id, new_media_key);
            asset.updated_at = now();
            if self
                .get_one_unlocked::<JobAsset>(JOB_ASSET_TABLE, &asset.asset_id)?
                .is_some()
            {
                return Err("new media key already exists in a Match job".to_string());
            }
            old_job_asset_ids.push(old_id);
            moved_job_assets.push(asset);
        }

        let mut cover_people = self
            .list_unlocked::<Person>(PERSON_TABLE)?
            .into_iter()
            .filter(|person| person.cover_media_key.as_deref() == Some(old_media_key))
            .collect::<Vec<_>>();
        for person in &mut cover_people {
            if !assignments
                .iter()
                .any(|assignment| assignment.person_id == person.person_id)
            {
                return Err(
                    "media move found a Person cover without a matching assignment".to_string(),
                );
            }
        }
        // Reject malformed video evidence before revision/cache invalidation.
        let (video_upserts, video_deletes) = self.rekey_video_rows_unlocked(
            old_media_key,
            new_media_key,
            &canonical_proven_fingerprint,
        )?;
        let mut execution = self.execution_state_unlocked()?;
        self.bump_revisions(&mut execution, false, true)?;
        for person in &mut cover_people {
            person.cover_media_key = Some(new_media_key.to_string());
            person.revision = person
                .revision
                .checked_add(1)
                .ok_or("Person revision overflow during media move")?;
            person.catalog_revision = execution.catalog_revision;
            person.updated_at = now();
        }
        let mut correction_mappings: Vec<corrections::CorrectionMediaOperation> = {
            let db = self.database();
            let old_media_key = old_media_key.to_string();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_correction_media_operation WITH INDEX match_correction_media_operation_media WHERE media_key = $media_key ORDER BY operation_id, mapping_id LIMIT 4097;",
                    )
                    .bind(("media_key", old_media_key))
                    .await
                    .map_err(|error| format!("query correction history for media rekey: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode correction history for media rekey: {error}"))
            })?
        };
        if correction_mappings.len() > 4096 {
            return Err("media rekey correction-history association exceeds 4096 rows".to_string());
        }
        let mut suggestion_source_provenance: Vec<corrections::SuggestionSourceProvenance> = {
            let db = self.database();
            let old_media_key = old_media_key.to_string();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_suggestion_source_provenance WITH INDEX match_suggestion_source_provenance_media WHERE media_key = $media_key ORDER BY operation_id, suggestion_id LIMIT 4097;",
                    )
                    .bind(("media_key", old_media_key))
                    .await
                    .map_err(|error| format!("query suggestion provenance for media rekey: {error}"))?;
                response.take(0).map_err(|error| {
                    format!("decode suggestion provenance for media rekey: {error}")
                })
            })?
        };
        if suggestion_source_provenance.len() > 4096 {
            return Err(
                "media rekey suggestion-provenance association exceeds 4096 rows".to_string(),
            );
        }
        for provenance in &mut suggestion_source_provenance {
            provenance.media_key = new_media_key.to_string();
        }
        // WP-085 introduced explicit media/operation mappings. Discover the
        // bounded legacy surface that predates those mappings so existing
        // direct reviews and no-change Not-sure receipts remain portable after
        // a proven move. Direct history uses the kind/FaceId index. Batch
        // Not-sure history has no single FaceId, so walk its kind index in
        // deterministic bounded pages and retain only envelopes for this key.
        // Unrelated batch history therefore cannot consume the 4096 relevant-
        // association limit or force a full-table scan.
        let legacy_candidates: Vec<MatchOperation> = {
            let db = self.database();
            let face_ids = moved_face_ids.iter().cloned().collect::<Vec<_>>();
            let kinds = vec![
                "assign_operator_confirmed".to_string(),
                "assign_committed_strict_automatic".to_string(),
                "different".to_string(),
                "not_sure".to_string(),
                "move_to_look".to_string(),
                "correction_not_sure".to_string(),
            ];
            let old_media_key = old_media_key.to_string();
            surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_operation WITH INDEX match_operation_kind_face WHERE kind IN $kinds AND face_id IN $face_ids ORDER BY operation_id ASC LIMIT 4097;",
                    )
                    .bind(("kinds", kinds))
                    .bind(("face_ids", face_ids))
                    .await
                    .map_err(|error| format!("query legacy direct Match history: {error}"))?;
                let mut candidates: Vec<MatchOperation> = response
                    .take(0)
                    .map_err(|error| format!("decode legacy direct Match history: {error}"))?;
                if candidates.len() > 4096 {
                    return Err(
                        "media rekey legacy direct-operation discovery exceeds 4096 rows"
                            .to_string(),
                    );
                }

                let mut after_operation_id = String::new();
                loop {
                    let mut page_response = db
                        .query(
                            "SELECT * OMIT id FROM match_operation WITH INDEX match_operation_kind_operation WHERE kind = 'correction_batch_not_sure' AND operation_id > $after_operation_id ORDER BY operation_id ASC LIMIT 256;",
                        )
                        .bind(("after_operation_id", after_operation_id.clone()))
                        .await
                        .map_err(|error| {
                            format!("query paged legacy batch Not-sure history: {error}")
                        })?;
                    let page: Vec<MatchOperation> = page_response.take(0).map_err(|error| {
                        format!("decode paged legacy batch Not-sure history: {error}")
                    })?;
                    if page.is_empty() {
                        break;
                    }
                    let next_after = page
                        .last()
                        .map(|operation| operation.operation_id.clone())
                        .ok_or("legacy batch Not-sure page unexpectedly omitted its cursor")?;
                    if next_after <= after_operation_id {
                        return Err("legacy batch Not-sure pagination did not advance".to_string());
                    }
                    for operation in &page {
                        let envelope: corrections::CorrectionDeltaEnvelope =
                            serde_json::from_str(&operation.after_json).map_err(|error| {
                                format!("decode legacy batch Not-sure envelope: {error}")
                            })?;
                        if envelope.kind == "batch_not_sure"
                            && envelope.media_keys.iter().any(|key| key == &old_media_key)
                        {
                            candidates.push(operation.clone());
                            if candidates.len() > 4096 {
                                return Err(
                                    "media rekey relevant legacy media-bearing operation discovery exceeds 4096 rows"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    let page_complete = page.len() < LEGACY_OPERATION_QUERY_PAGE_LIMIT;
                    after_operation_id = next_after;
                    if page_complete {
                        break;
                    }
                }
                Ok(candidates)
            })?
        };
        let mut mapped_ids = correction_mappings
            .iter()
            .map(|mapping| mapping.mapping_id.clone())
            .collect::<BTreeSet<_>>();
        for candidate in &legacy_candidates {
            let expected_mapping_id =
                corrections::correction_media_mapping_id(&candidate.operation_id, old_media_key);
            if mapped_ids.contains(&expected_mapping_id) {
                continue;
            }
            let mapping_kind = if let Some(kind) = candidate.kind.strip_prefix("correction_") {
                let envelope: corrections::CorrectionDeltaEnvelope =
                    serde_json::from_str(&candidate.after_json).map_err(|error| {
                        format!("decode legacy Not-sure correction envelope: {error}")
                    })?;
                if !matches!(kind, "not_sure" | "batch_not_sure")
                    || envelope.kind != kind
                    || !envelope.media_keys.iter().any(|key| key == old_media_key)
                {
                    continue;
                }
                kind.to_string()
            } else {
                candidate.kind.clone()
            };
            correction_mappings.push(corrections::CorrectionMediaOperation {
                mapping_id: expected_mapping_id.clone(),
                media_key: old_media_key.to_string(),
                media_fingerprint: canonical_proven_fingerprint.clone(),
                operation_id: candidate.operation_id.clone(),
                kind: mapping_kind,
                created_at: candidate.created_at.clone(),
            });
            mapped_ids.insert(expected_mapping_id);
        }
        if correction_mappings.len() > 4096 {
            return Err(
                "media rekey combined media-history association exceeds 4096 rows".to_string(),
            );
        }
        let mut rekeyed_correction_operations = Vec::new();
        let mut rekeyed_correction_mappings = Vec::new();
        let mut old_correction_mapping_ids = Vec::new();
        for mapping in correction_mappings {
            if mapping.mapping_id
                != corrections::correction_media_mapping_id(
                    &mapping.operation_id,
                    &mapping.media_key,
                )
            {
                return Err("media rekey found a non-canonical correction mapping".to_string());
            }
            if mapping.media_fingerprint != canonical_proven_fingerprint {
                return Err(
                    "media rekey correction mapping fingerprint contradicts proven media bytes"
                        .to_string(),
                );
            }
            let mut correction: MatchOperation = self.require_unlocked(
                OPERATION_TABLE,
                &mapping.operation_id,
                "media rekey correction operation",
            )?;
            let correction_history = correction.kind == format!("correction_{}", mapping.kind);
            let direct_history = correction.kind == mapping.kind
                && matches!(
                    mapping.kind.as_str(),
                    "assign_operator_confirmed"
                        | "assign_committed_strict_automatic"
                        | "different"
                        | "not_sure"
                        | "move_to_look"
                );
            let no_change_correction = correction_history
                && matches!(mapping.kind.as_str(), "not_sure" | "batch_not_sure");
            if correction.created_at != mapping.created_at
                || (!correction_history && !direct_history)
                || (correction_history && correction.reversible == no_change_correction)
            {
                return Err(
                    "media rekey found an invalid correction-history association".to_string(),
                );
            }
            let rewrite_history_operation = |operation: &mut MatchOperation,
                                             regenerate_before: bool|
             -> Result<(), String> {
                let mut envelope: corrections::CorrectionDeltaEnvelope =
                    serde_json::from_str(&operation.after_json).map_err(|error| {
                        format!("decode correction envelope during media rekey: {error}")
                    })?;
                if !envelope.media_keys.iter().any(|key| key == old_media_key) {
                    return Err(format!(
                        "media rekey operation {} omits its mapped media key",
                        operation.operation_id
                    ));
                }
                for media_key in &mut envelope.media_keys {
                    if media_key == old_media_key {
                        *media_key = new_media_key.to_string();
                    }
                }
                envelope.media_keys.sort();
                envelope.media_keys.dedup();
                for row in &mut envelope.rows {
                    let mut current = self.get_one_unlocked::<Value>(
                        correction_table_name_for_rekey(&row.table),
                        &row.stable_id,
                    )?;
                    let owns_live_row =
                        correction_row_owner_matches_for_rekey(&row.table, &current, &row.after)?;
                    if let Some(snapshot) = &mut current {
                        rekey_correction_snapshot_media_fields(
                            snapshot,
                            old_media_key,
                            new_media_key,
                        );
                    }
                    for snapshot in [&mut row.before, &mut row.after] {
                        if let Some(snapshot) = snapshot {
                            rekey_correction_snapshot_media_fields(
                                snapshot,
                                old_media_key,
                                new_media_key,
                            );
                        }
                    }
                    if owns_live_row {
                        row.after = match row.table {
                            corrections::CorrectionTable::Face => faces
                                .iter()
                                .find(|face| face.face_id == row.stable_id)
                                .map(serde_json::to_value)
                                .transpose()
                                .map_err(|error| error.to_string())?
                                .or(current),
                            corrections::CorrectionTable::Assignment => assignments
                                .iter()
                                .find(|assignment| assignment.assignment_id == row.stable_id)
                                .map(serde_json::to_value)
                                .transpose()
                                .map_err(|error| error.to_string())?
                                .or(current),
                            corrections::CorrectionTable::Person => cover_people
                                .iter()
                                .find(|person| person.person_id == row.stable_id)
                                .map(serde_json::to_value)
                                .transpose()
                                .map_err(|error| error.to_string())?
                                .or(current),
                            corrections::CorrectionTable::Disposition => dispositions
                                .iter()
                                .find(|disposition| disposition.face_id == row.stable_id)
                                .map(serde_json::to_value)
                                .transpose()
                                .map_err(|error| error.to_string())?
                                .or(current),
                            _ => current,
                        };
                    }
                }
                if regenerate_before {
                    operation.before_json = serde_json::to_string(
                        &envelope
                            .rows
                            .iter()
                            .map(|row| (&row.table, &row.stable_id, &row.before))
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|error| error.to_string())?;
                }
                operation.after_json =
                    serde_json::to_string(&envelope).map_err(|error| error.to_string())?;
                if operation.before_json.len() > exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES
                    || operation.after_json.len()
                        > exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES
                {
                    return Err(format!(
                        "media rekey operation {} exceeds the portable identity-bundle nested-string limit",
                        operation.operation_id
                    ));
                }
                Ok(())
            };
            if correction_history {
                rewrite_history_operation(&mut correction, true)?;

                if correction.reversible {
                    let undo_id = format!("undo-{}", mapping.operation_id);
                    if let Some(mut undo) =
                        self.get_one_unlocked::<MatchOperation>(OPERATION_TABLE, &undo_id)?
                    {
                        let undo_before: Value = serde_json::from_str(&undo.before_json)
                            .map_err(|error| format!("decode media rekey undo binding: {error}"))?;
                        if undo.kind != "undo_correction"
                            || undo.reversible
                            || undo_before.get("operation_id").and_then(Value::as_str)
                                != Some(mapping.operation_id.as_str())
                        {
                            return Err("media rekey found an invalid correction undo association"
                                .to_string());
                        }
                        rewrite_history_operation(&mut undo, false)?;
                        rekeyed_correction_operations.push(undo);
                    }
                }
            } else {
                rekey_direct_operation_media_fields(&mut correction, old_media_key, new_media_key)?;
            }

            let replacement = corrections::CorrectionMediaOperation {
                mapping_id: corrections::correction_media_mapping_id(
                    &mapping.operation_id,
                    new_media_key,
                ),
                media_key: new_media_key.to_string(),
                // A proven move changes only the key. Preserve the existing
                // operation's exact byte identity rather than deriving a new
                // fingerprint from the destination path.
                media_fingerprint: mapping.media_fingerprint.clone(),
                operation_id: mapping.operation_id.clone(),
                kind: mapping.kind.clone(),
                created_at: mapping.created_at.clone(),
            };
            if let Some(existing) = self.get_one_unlocked::<corrections::CorrectionMediaOperation>(
                corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                &replacement.mapping_id,
            )? {
                if existing != replacement {
                    return Err(
                        "new media key already has a conflicting correction mapping".to_string()
                    );
                }
            }
            old_correction_mapping_ids.push(mapping.mapping_id);
            rekeyed_correction_operations.push(correction);
            rekeyed_correction_mappings.push(replacement);
        }

        let operation = MatchOperation {
            operation_id: new_id("operation"),
            kind: "rekey_media_proven_move".to_string(),
            face_id: None,
            person_id: None,
            before_json: serde_json::to_string(&json!({
                "media_key": old_media_key,
                "media_fingerprint": proven_fingerprint,
            }))
            .map_err(|error| error.to_string())?,
            after_json: serde_json::to_string(&json!({
                "media_key": new_media_key,
                "media_fingerprint": proven_fingerprint,
                "rekeyed_cover_person_ids": cover_people
                    .iter()
                    .map(|person| person.person_id.as_str())
                    .collect::<Vec<_>>(),
            }))
            .map_err(|error| error.to_string())?,
            reversible: true,
            created_at: now(),
        };
        let mut owned_upserts = video_upserts;
        if let Some(mut context) = self.get_one_unlocked::<CanonicalMediaContext>(
            context::MEDIA_CONTEXT_TABLE,
            old_media_key,
        )? {
            context.validate()?;
            if context.media_fingerprint != canonical_proven_fingerprint {
                return Err("media rekey context fingerprint is stale".into());
            }
            let target = self.get_one_unlocked::<CanonicalMediaContext>(
                context::MEDIA_CONTEXT_TABLE,
                new_media_key,
            )?;
            if let Some(target) = &target {
                target.validate()?;
                if target.media_fingerprint != canonical_proven_fingerprint
                    || target.capture_unix_millis.is_some()
                    || target.time_window_millis.is_some()
                    || !target.album_ids.is_empty()
                {
                    return Err("media rekey destination has canonical context".into());
                }
            }
            context.revision = context
                .revision
                .max(target.as_ref().map_or(0, |r| r.revision))
                .checked_add(1)
                .filter(|r| *r <= i64::MAX as u64)
                .ok_or("media context revision exhausted")?;
            let mut tombstone = context.clone();
            tombstone.capture_unix_millis = None;
            tombstone.time_window_millis = None;
            tombstone.album_ids.clear();
            context.media_key = new_media_key.into();
            for row in [tombstone, context] {
                owned_upserts.push((
                    context::MEDIA_CONTEXT_TABLE.into(),
                    row.media_key.clone(),
                    serde_json::to_value(row).map_err(|e| e.to_string())?,
                ));
            }
        }
        for face in &faces {
            owned_upserts.push((
                FACE_TABLE.to_string(),
                face.face_id.clone(),
                serde_json::to_value(face).map_err(|error| error.to_string())?,
            ));
        }
        for assignment in &assignments {
            owned_upserts.push((
                ASSIGNMENT_TABLE.to_string(),
                assignment.assignment_id.clone(),
                serde_json::to_value(assignment).map_err(|error| error.to_string())?,
            ));
        }
        for disposition in &dispositions {
            owned_upserts.push((
                FACE_DISPOSITION_TABLE.to_string(),
                disposition.face_id.clone(),
                serde_json::to_value(disposition).map_err(|error| error.to_string())?,
            ));
        }
        for person in &cover_people {
            owned_upserts.push((
                PERSON_TABLE.to_string(),
                person.person_id.clone(),
                serde_json::to_value(person).map_err(|error| error.to_string())?,
            ));
        }
        if let Some(projection) = &moved_projection {
            owned_upserts.push((
                PROJECTION_TABLE.to_string(),
                new_media_key.to_string(),
                serde_json::to_value(projection).map_err(|error| error.to_string())?,
            ));
        }
        for asset in &moved_job_assets {
            owned_upserts.push((
                JOB_ASSET_TABLE.to_string(),
                asset.asset_id.clone(),
                serde_json::to_value(asset).map_err(|error| error.to_string())?,
            ));
        }
        for correction in &rekeyed_correction_operations {
            owned_upserts.push((
                OPERATION_TABLE.to_string(),
                correction.operation_id.clone(),
                serde_json::to_value(correction).map_err(|error| error.to_string())?,
            ));
        }
        for mapping in &rekeyed_correction_mappings {
            owned_upserts.push((
                corrections::CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                mapping.mapping_id.clone(),
                serde_json::to_value(mapping).map_err(|error| error.to_string())?,
            ));
        }
        for provenance in &suggestion_source_provenance {
            owned_upserts.push((
                corrections::SUGGESTION_SOURCE_PROVENANCE_TABLE.to_string(),
                provenance.provenance_id.clone(),
                serde_json::to_value(provenance).map_err(|error| error.to_string())?,
            ));
        }
        owned_upserts.push((
            OPERATION_TABLE.to_string(),
            operation.operation_id.clone(),
            serde_json::to_value(&operation).map_err(|error| error.to_string())?,
        ));
        owned_upserts.push((
            EXECUTION_TABLE.to_string(),
            "global".to_string(),
            serde_json::to_value(&execution).map_err(|error| error.to_string())?,
        ));
        let upserts = owned_upserts
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        let mut owned_deletes = old_job_asset_ids
            .into_iter()
            .map(|id| (JOB_ASSET_TABLE.to_string(), id))
            .collect::<Vec<_>>();
        owned_deletes.extend(video_deletes);
        owned_deletes.extend(old_correction_mapping_ids.into_iter().map(|id| {
            (
                corrections::CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                id,
            )
        }));
        if old_projection.is_some() {
            owned_deletes.push((PROJECTION_TABLE.to_string(), old_media_key.to_string()));
        }
        let deletes = owned_deletes
            .iter()
            .map(|(table, id)| (table.as_str(), id.as_str()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&upserts, &deletes)?;
        let mut caches = self.cache_write_recover();
        caches.projections.remove(old_media_key);
        if let Some(projection) = moved_projection {
            insert_bounded_projection(&mut caches.projections, projection);
        }
        drop(caches);
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(faces.len())
    }

    pub fn rebuild_regenerable(&self) -> Result<(), String> {
        let preview = self.preview_rebuild_match_analysis()?;
        self.rebuild_match_analysis(&preview).map(|_| ())
    }

    pub fn status(&self) -> Result<Value, String> {
        if self.database_recovery_pending() {
            return Ok(self.database_recovery_snapshot("database_recovery_pending"));
        }
        self.database_diagnostic_result(self.canonical_status())
    }

    fn canonical_status(&self) -> Result<Value, String> {
        let desired_mode = self.desired_mode()?;
        let execution_state = self.execution_state()?;
        let holds = self.holds()?;
        let resource_telemetry = self.governor.telemetry()?;
        let usage = resource_telemetry.current_usage;
        let (cached_projections, last_query_plan) = {
            let caches = self
                .caches
                .read()
                .map_err(|_| "Match projection cache is poisoned".to_string())?;
            (caches.projections.len(), caches.last_query_plan.clone())
        };
        let job_diagnostics = self.job_diagnostics()?;
        let active_generation = self
            .list::<ModelGeneration>(GENERATION_TABLE)?
            .into_iter()
            .find(|generation| generation.state == "active")
            .map(|generation| generation.generation);
        let active_calibration = self
            .list::<CalibrationActivation>(CALIBRATION_TABLE)?
            .into_iter()
            .find(|activation| activation.active)
            .map(|activation| {
                json!({
                    "calibration_generation": activation.calibration_generation,
                    "model_generation": activation.model_generation,
                    "envelope_hash": activation.envelope_hash,
                    "wp084_runtime_ready": activation.wp084_runtime_ready,
                    "wp087_release_ready": activation.wp087_release_ready
                })
            });
        Ok(json!({
            "schema_version": MATCH_SCHEMA_VERSION,
            "schema_generation": MATCH_SCHEMA_GENERATION,
            "vector": {
                "index": EMBEDDING_INDEX,
                "dimension": EMBEDDING_DIM,
                "type": "F32",
                "distance": "COSINE",
                "exact_rerank": true,
                "last_query_plan": last_query_plan.unwrap_or(QueryPlanEvidence {
                    observed: false,
                    index: EMBEDDING_INDEX.to_string(),
                    uses_hnsw: false,
                    exact_rerank: true,
                    model_generation: String::new(),
                    observed_at: String::new(),
                }),
            },
            "calibration_activation": active_calibration,
            "counts": {
                "people": self.count(PERSON_TABLE)?,
                "index_roots": self.count(ROOT_CONFIG_TABLE)?,
                "looks": self.count(LOOK_TABLE)?,
                "trusted_template_sets": self.count(TEMPLATE_SET_TABLE)?,
                "trusted_members": self.count(TRUSTED_MEMBER_TABLE)?,
                "trusted_search_embeddings": self.count(TRUSTED_SEARCH_TABLE)?,
                "calibration_records": self.count(CALIBRATION_TABLE)?,
                "calibration_spent_sets": self.count(CALIBRATION_SPENT_TABLE)?,
                "faces": self.count(FACE_TABLE)?,
                "embeddings": self.count(EMBEDDING_TABLE)?,
                "assignments": self.count(ASSIGNMENT_TABLE)?,
                "suggestions": self.count(SUGGESTION_TABLE)?,
                "constraints": self.count(CONSTRAINT_TABLE)?,
                "operations": self.count(OPERATION_TABLE)?,
                "jobs": self.count(JOB_TABLE)?,
            },
            "execution": {
                "desired_mode": desired_mode.as_str(),
                "database_owner": self.store.owner_diagnostics(),
                "pending_failure_records": self.pending_database_failure_snapshot(),
                "pending_failure_records_capacity": 16,
                "pending_failure_records_evicted": self.pending_database_failures_evicted.load(Ordering::Acquire),
                "pending_failure_records_authoritative": false,
                "identity_revision": execution_state.identity_revision,
                "catalog_revision": execution_state.catalog_revision,
                "transient_holds": holds,
                "resource_usage": usage,
                "resource_budget": self.governor.budget(),
                "resource_telemetry": resource_telemetry,
            },
            "projection_cache": {
                "capacity": PROJECTION_CACHE_CAPACITY,
                "cached": cached_projections,
                "miss_policy": "explicit_off_render_warm",
            },
            "jobs": job_diagnostics,
            "active_model_generation": active_generation,
            "privacy": {
                "embeddings_in_status": false,
                "face_crops_in_status": false,
                "database_handles_in_status": false,
            }
        }))
    }

    fn job_diagnostics(&self) -> Result<Value, String> {
        let _guard = self.database_read_guard("Match job diagnostics lock is poisoned")?;
        let db = self.database();
        let (lifecycle_counts, progress_rows, raw_failure_codes): (
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
        ) = surreal_store::run(async move {
            let mut response = db
                    .query(
                        "SELECT lifecycle, count() AS count FROM match_index_job GROUP BY lifecycle ORDER BY lifecycle ASC;\
                         SELECT math::sum(discovered) AS discovered, math::sum(completed) AS completed, math::sum(failed) AS failed FROM match_index_job GROUP ALL;\
                         SELECT failure_code, count() AS count FROM match_job_asset WHERE failure_code != NONE GROUP BY failure_code ORDER BY count DESC, failure_code ASC LIMIT 20;",
                    )
                    .await
                    .map_err(|error| format!("query Match job diagnostics: {error}"))?;
            let lifecycle_counts = response
                .take(0)
                .map_err(|error| format!("decode Match job lifecycle diagnostics: {error}"))?;
            let progress_rows = response
                .take(1)
                .map_err(|error| format!("decode Match job progress diagnostics: {error}"))?;
            let failure_codes = response
                .take(2)
                .map_err(|error| format!("decode Match job failure diagnostics: {error}"))?;
            Ok((lifecycle_counts, progress_rows, failure_codes))
        })?;
        let mut sanitized_failure_codes = BTreeMap::<String, u64>::new();
        for row in raw_failure_codes {
            let raw_code = row
                .get("failure_code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let code = redacted_failure_code(raw_code);
            let count = row.get("count").and_then(Value::as_u64).unwrap_or(0);
            *sanitized_failure_codes.entry(code.to_string()).or_default() += count;
        }
        let failure_codes = sanitized_failure_codes
            .into_iter()
            .map(|(failure_code, count)| json!({ "failure_code": failure_code, "count": count }))
            .collect::<Vec<_>>();
        Ok(json!({
            "lifecycle_counts": lifecycle_counts,
            "progress": progress_rows.first().cloned().unwrap_or_else(|| json!({
                "discovered": 0,
                "completed": 0,
                "failed": 0,
            })),
            "failure_codes": failure_codes,
            "failure_code_limit": 20,
            "includes_failure_messages": false,
            "includes_media_keys": false,
        }))
    }

    pub fn list<T: DeserializeOwned>(&self, table: &str) -> Result<Vec<T>, String> {
        let _guard = self.database_read_guard("Match read lock is poisoned")?;
        self.list_unlocked(table)
    }

    fn count(&self, table: &str) -> Result<u64, String> {
        let _guard = self.database_read_guard("Match count lock is poisoned")?;
        let db = self.database();
        let sql = format!("SELECT count() AS count FROM {table} GROUP ALL;");
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .await
                .map_err(|error| format!("count Match table: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match table count: {error}"))
        })?;
        Ok(rows
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    fn list_unlocked<T: DeserializeOwned>(&self, table: &str) -> Result<Vec<T>, String> {
        let db = self.database();
        let sql = format!("SELECT * OMIT id FROM {table};");
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .await
                .map_err(|error| format!("read Match table: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match table: {error}"))
        })?;
        rows.into_iter()
            .map(|row| serde_json::from_value(row).map_err(|error| error.to_string()))
            .collect()
    }

    fn get_one<T: DeserializeOwned>(&self, table: &str, id: &str) -> Result<Option<T>, String> {
        let _guard = self.database_read_guard("Match read lock is poisoned")?;
        self.get_one_unlocked(table, id)
    }

    fn get_one_unlocked<T: DeserializeOwned>(
        &self,
        table: &str,
        id: &str,
    ) -> Result<Option<T>, String> {
        let db = self.database();
        let sql = format!("SELECT * OMIT id FROM ONLY type::record('{table}', $id);");
        let id = id.to_string();
        let row: Option<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .bind(("id", id))
                .await
                .map_err(|error| format!("read Match record: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match record: {error}"))
        })?;
        row.map(|row| serde_json::from_value(row).map_err(|error| error.to_string()))
            .transpose()
    }

    fn require<T: DeserializeOwned>(
        &self,
        table: &str,
        id: &str,
        label: &str,
    ) -> Result<T, String> {
        self.get_one(table, id)?
            .ok_or_else(|| format!("{label} {id} does not exist"))
    }

    fn require_unlocked<T: DeserializeOwned>(
        &self,
        table: &str,
        id: &str,
        label: &str,
    ) -> Result<T, String> {
        self.get_one_unlocked(table, id)?
            .ok_or_else(|| format!("{label} {id} does not exist"))
    }

    fn upsert_json<T: Serialize>(&self, table: &str, id: &str, value: &T) -> Result<(), String> {
        let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
        self.transactional_upserts_deletes(&[(table, id, value)], &[])
    }

    fn commit_person_revision_change_unlocked(
        &self,
        person: &Person,
        execution: &PersistedExecutionState,
    ) -> Result<(), String> {
        let db = self.database();
        let person = person.clone();
        let execution = execution.clone();
        let person_id = person.person_id.clone();
        let person_revision = person.revision;
        surreal_store::run(async move {
            // Operator-confirmed rows are durable identity decisions, so only
            // their Person revision fence advances. Automatic rows cannot be
            // silently promoted or re-endorsed against a changed Person; they
            // and candidate suggestions must be regenerated instead.
            db.query(
                "BEGIN TRANSACTION;
                 UPDATE match_assignment SET person_revision = $person_revision
                    WHERE person_id = $person_id AND state = $operator_confirmed;
                 DELETE match_assignment
                    WHERE person_id = $person_id AND state = $strict_automatic;
                 DELETE match_suggestion WHERE candidate_person_id = $person_id;
                 UPSERT type::record('match_person', $person_id) CONTENT $person;
                 UPSERT match_execution:global CONTENT $execution;
                 COMMIT TRANSACTION;",
            )
            .bind(("person_id", person_id))
            .bind(("person_revision", person_revision))
            .bind((
                "operator_confirmed",
                AssignmentState::OperatorConfirmed.as_str(),
            ))
            .bind((
                "strict_automatic",
                AssignmentState::CommittedStrictAutomatic.as_str(),
            ))
            .bind(("person", person))
            .bind(("execution", execution))
            .await
            .map_err(|error| format!("reconcile Match Person revision transaction: {error}"))?
            .check()
            .map_err(|error| format!("reconcile Match Person revision transaction: {error}"))?;
            Ok::<(), String>(())
        })
    }

    fn transactional_upserts_deletes(
        &self,
        upserts: &[(&str, &str, Value)],
        deletes: &[(&str, &str)],
    ) -> Result<(), String> {
        let _guard = self.mutation_write_guard("Match write")?;
        self.transactional_upserts_deletes_unlocked(upserts, deletes)
    }

    fn transactional_upserts_deletes_unlocked(
        &self,
        upserts: &[(&str, &str, Value)],
        deletes: &[(&str, &str)],
    ) -> Result<(), String> {
        let mut sql = String::from("BEGIN TRANSACTION;\n");
        let mut statement_labels = vec!["begin transaction".to_string()];
        for (index, (table, _, _)) in upserts.iter().enumerate() {
            sql.push_str(&format!(
                "UPSERT type::record('{table}', $upsert_id_{index}) CONTENT $upsert_value_{index};\n"
            ));
            statement_labels.push(format!("upsert {table}:{}", upserts[index].1));
        }
        for (index, (table, _)) in deletes.iter().enumerate() {
            sql.push_str(&format!(
                "DELETE type::record('{table}', $delete_id_{index});\n"
            ));
            statement_labels.push(format!("delete {table}:{}", deletes[index].1));
        }
        sql.push_str("COMMIT TRANSACTION;");
        statement_labels.push("commit transaction".to_string());
        let db = self.database();
        let mut query = db.query(sql);
        #[cfg(test)]
        if let Some(delay) = self.next_checkpoint_ack_delay.lock().unwrap().take() {
            query = query.test_delay_reply_after_commit(delay);
        }
        for (index, (_, id, value)) in upserts.iter().enumerate() {
            query = query
                .bind((format!("upsert_id_{index}"), (*id).to_string()))
                .bind((format!("upsert_value_{index}"), value.clone()));
        }
        for (index, (_, id)) in deletes.iter().enumerate() {
            query = query.bind((format!("delete_id_{index}"), (*id).to_string()));
        }
        surreal_store::run(async move {
            let mut response = query
                .await
                .map_err(|error| format!("commit Match transaction: {error:?}"))?;
            let mut errors = response.take_errors().into_iter().collect::<Vec<_>>();
            errors.sort_by_key(|(statement_index, _)| *statement_index);
            if let Some((statement_index, error)) = errors.into_iter().next() {
                let label = statement_labels
                    .get(statement_index)
                    .map(String::as_str)
                    .unwrap_or("unknown statement");
                return Err(format!(
                    "commit Match transaction at statement {statement_index} ({label}): {error:?}"
                ));
            }
            Ok(())
        })
    }

    /// Disable every strict calibration before an envelope-affecting mutation.
    /// This deliberately commits first: if the later mutation fails, the only
    /// possible outcome is a conservative false negative, never a stale active
    /// calibration over a changed trusted gallery.
    fn invalidate_calibrations_unlocked(&self, reason: &str) -> Result<(), String> {
        validate_text("calibration invalidation reason", reason)?;
        let mut activations = self.list_unlocked::<CalibrationActivation>(CALIBRATION_TABLE)?;
        if activations.is_empty() {
            return Ok(());
        }
        let updated_at = now();
        let mut owned = Vec::with_capacity(activations.len());
        for activation in &mut activations {
            let invalidation_reason = if reason == "trusted_search_reconciled" {
                match activation.invalidation_reason.as_deref() {
                    Some(existing)
                        if existing.starts_with("trusted_search_reconciled:was_active") =>
                    {
                        existing.to_string()
                    }
                    _ if activation.active => "trusted_search_reconciled:was_active".to_string(),
                    _ => "trusted_search_reconciled:was_inactive".to_string(),
                }
            } else {
                reason.to_string()
            };
            activation.active = false;
            // Readiness gates describe independently verified evidence. Keep
            // them intact while the activation is fail-closed by `active` and
            // `invalidation_reason`; a verified identity rollback can then
            // restore a previously active calibration only after every live
            // gallery/build/envelope binding matches again.
            activation.invalidation_reason = Some(invalidation_reason);
            activation.updated_at = updated_at.clone();
            activation.activation_integrity_digest =
                calibration_activation_integrity_digest(activation);
            owned.push((
                CALIBRATION_TABLE.to_string(),
                activation.calibration_generation.clone(),
                serde_json::to_value(activation).map_err(|error| error.to_string())?,
            ));
        }
        let borrowed = owned
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&borrowed, &[])
    }

    fn execution_state(&self) -> Result<PersistedExecutionState, String> {
        self.require(EXECUTION_TABLE, "global", "Match execution state")
    }

    fn execution_state_unlocked(&self) -> Result<PersistedExecutionState, String> {
        self.require_unlocked(EXECUTION_TABLE, "global", "Match execution state")
    }

    fn validate_operator_fence_unlocked(
        &self,
        fence: &OperatorMutationFence,
        face: &FaceObservation,
        person: &Person,
    ) -> Result<(), String> {
        let execution = self.execution_state_unlocked()?;
        let assignment_operation_id = self
            .get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, &face.face_id)?
            .map(|value| value.operation_id);
        if fence.face_id != face.face_id
            || fence.person_id != person.person_id
            || fence.face_revision != face.face_revision
            || fence.person_revision != person.revision
            || fence.assignment_operation_id != assignment_operation_id
            || fence.identity_revision != execution.identity_revision
            || fence.catalog_revision != execution.catalog_revision
        {
            return Err("stale operator identity mutation fence".to_string());
        }
        Ok(())
    }

    fn bump_revisions(
        &self,
        state: &mut PersistedExecutionState,
        identity: bool,
        catalog: bool,
    ) -> Result<(), String> {
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("Match execution revision overflow")?;
        if identity {
            state.identity_revision = state
                .identity_revision
                .checked_add(1)
                .ok_or("Match identity revision overflow")?;
        }
        if catalog {
            state.catalog_revision = state
                .catalog_revision
                .checked_add(1)
                .ok_or("Match catalog revision overflow")?;
        }
        state.updated_at = now();
        let mut caches = self.cache_write_recover();
        caches.identity_revision = state.identity_revision;
        caches.catalog_revision = state.catalog_revision;
        caches.projections.clear();
        if catalog {
            caches.autocomplete.valid = false;
        }
        Ok(())
    }

    fn require_current_job_asset_unlocked(
        &self,
        job_id: &str,
        media_key: &str,
        require_active_generation: bool,
    ) -> Result<(IndexJob, JobAsset), String> {
        let job: IndexJob = self.require_unlocked(JOB_TABLE, job_id, "IndexJob")?;
        if !job.lifecycle()?.accepts_inflight_result() {
            return Err("terminal or non-runnable IndexJob rejected a late result".to_string());
        }
        let asset: JobAsset = self.require_unlocked(
            JOB_ASSET_TABLE,
            &job_asset_id(job_id, media_key),
            "job asset",
        )?;
        let state = self.execution_state_unlocked()?;
        let generation: ModelGeneration =
            self.require_unlocked(GENERATION_TABLE, &job.model_generation, "model generation")?;
        if job.schema_generation != asset.schema_generation
            || job.model_generation != asset.model_generation
            || job.identity_revision != asset.identity_revision
            || job.catalog_revision != asset.catalog_revision
            || job.identity_revision != state.identity_revision
            || job.catalog_revision != state.catalog_revision
            || !generation.validated
            || if require_active_generation {
                generation.state != "active"
            } else {
                !matches!(generation.state.as_str(), "usable" | "active")
            }
        {
            return Err("stale IndexJob authority revision".to_string());
        }
        Ok((job, asset))
    }

    fn require_valid_asset_fence_unlocked(
        &self,
        fence: &RevisionFence,
        require_active_generation: bool,
    ) -> Result<JobAsset, String> {
        let (job, asset) = self.require_current_job_asset_unlocked(
            &fence.job_id,
            &fence.media_key,
            require_active_generation,
        )?;
        validate_asset_fence(&asset, fence)?;
        if job.schema_generation != fence.schema_generation
            || job.model_generation != fence.model_generation
            || job.identity_revision != fence.identity_revision
            || job.catalog_revision != fence.catalog_revision
        {
            return Err("stale IndexJob revision fence".to_string());
        }
        Ok(asset)
    }

    fn recover_interrupted_jobs(&self) -> Result<(), String> {
        let interrupted = self
            .list::<IndexJob>(JOB_TABLE)?
            .into_iter()
            .filter(|job| matches!(job.lifecycle.as_str(), "running" | "pausing"))
            .collect::<Vec<_>>();
        for mut job in interrupted {
            job.lifecycle = if job.lifecycle() == Ok(JobLifecycle::Pausing)
                || self.desired_mode()? == DesiredMode::OperatorPaused
            {
                JobLifecycle::Paused
            } else {
                JobLifecycle::Retrying
            }
            .as_str()
            .to_string();
            job.updated_at = now();
            self.upsert_json(JOB_TABLE, &job.job_id, &job)?;
        }
        Ok(())
    }
}

fn require_persist_time_remaining(deadline: std::time::Instant) -> Result<(), String> {
    if std::time::Instant::now() >= deadline {
        Err("safe_unit_timeout: Match Persist deadline expired before transaction admission".into())
    } else {
        Ok(())
    }
}

fn validate_job_transition(current: JobLifecycle, next: JobLifecycle) -> Result<(), String> {
    if current == next {
        return Ok(());
    }
    if current.is_terminal_or_failed() && next != JobLifecycle::Retrying {
        return Err("terminal or failed Match job requires explicit retry".to_string());
    }
    let allowed = matches!(
        (current, next),
        (JobLifecycle::Queued, JobLifecycle::Running)
            | (JobLifecycle::Queued, JobLifecycle::Paused)
            | (JobLifecycle::Queued, JobLifecycle::Cancelled)
            | (JobLifecycle::Running, JobLifecycle::Pausing)
            | (JobLifecycle::Running, JobLifecycle::Completed)
            | (JobLifecycle::Running, JobLifecycle::Partial)
            | (JobLifecycle::Running, JobLifecycle::Failed)
            | (JobLifecycle::Running, JobLifecycle::Cancelled)
            | (JobLifecycle::Pausing, JobLifecycle::Paused)
            | (JobLifecycle::Pausing, JobLifecycle::Cancelled)
            | (JobLifecycle::Paused, JobLifecycle::Running)
            | (JobLifecycle::Paused, JobLifecycle::Cancelled)
            | (JobLifecycle::Blocked, JobLifecycle::Retrying)
            | (JobLifecycle::Retrying, JobLifecycle::Running)
            | (JobLifecycle::Retrying, JobLifecycle::Paused)
            | (JobLifecycle::Partial, JobLifecycle::Retrying)
            | (JobLifecycle::Cancelled, JobLifecycle::Retrying)
            | (JobLifecycle::Completed, JobLifecycle::Retrying)
            | (JobLifecycle::Failed, JobLifecycle::Retrying)
    );
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "invalid Match job transition {} -> {}",
            current.as_str(),
            next.as_str()
        ))
    }
}

fn validate_asset_fence(asset: &JobAsset, fence: &RevisionFence) -> Result<(), String> {
    if asset.job_id != fence.job_id
        || asset.media_key != fence.media_key
        || asset.media_fingerprint != fence.media_fingerprint
        || asset.schema_generation != fence.schema_generation
        || asset.model_generation != fence.model_generation
        || asset.identity_revision != fence.identity_revision
        || asset.catalog_revision != fence.catalog_revision
    {
        Err("stale job asset revision fence".to_string())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmbeddingRetryDisposition {
    Exact,
    RefreshCurrentJob,
}

/// Apply one retry contract to standalone and fused embedding publication.
/// Derived bytes are immutable. Provenance may move to a different current job,
/// but a retry under the same job must reproduce its original timestamp exactly.
fn canonical_embedding_retry_disposition(
    existing: &FaceEmbedding,
    incoming: &FaceEmbedding,
) -> Result<EmbeddingRetryDisposition, ()> {
    let same_derived_payload = existing.embedding_id == incoming.embedding_id
        && existing.face_id == incoming.face_id
        && existing.vector == incoming.vector
        && existing.model_generation == incoming.model_generation
        && existing.schema_generation == incoming.schema_generation
        && existing.media_fingerprint == incoming.media_fingerprint
        && existing.face_revision == incoming.face_revision
        && existing.active == incoming.active;
    if !same_derived_payload {
        return Err(());
    }
    if existing.job_id != incoming.job_id {
        return Ok(EmbeddingRetryDisposition::RefreshCurrentJob);
    }
    if existing.created_at == incoming.created_at {
        Ok(EmbeddingRetryDisposition::Exact)
    } else {
        Err(())
    }
}

fn validate_embedding_job_provenance(
    embedding: &FaceEmbedding,
    face: &FaceObservation,
    job: &IndexJob,
    asset: &JobAsset,
    fence: &RevisionFence,
) -> Result<(), String> {
    if embedding.embedding_id != embedding_id(&face.face_id, &fence.model_generation)
        || embedding.face_id != face.face_id
        || embedding.job_id != fence.job_id
        || embedding.job_id != job.job_id
        || asset.job_id != job.job_id
        || embedding.model_generation != fence.model_generation
        || embedding.model_generation != job.model_generation
        || embedding.model_generation != asset.model_generation
        || embedding.schema_generation != MATCH_SCHEMA_GENERATION
        || embedding.schema_generation != face.schema_generation
        || embedding.schema_generation != fence.schema_generation
        || embedding.schema_generation != job.schema_generation
        || embedding.schema_generation != asset.schema_generation
        || embedding.media_fingerprint != face.media_fingerprint
        || embedding.media_fingerprint != fence.media_fingerprint
        || embedding.media_fingerprint != asset.media_fingerprint
        || embedding.face_revision != face.face_revision
        || !embedding.active
    {
        return Err("embedding provenance contradicts its canonical job/face fence".to_string());
    }
    let embedding_created_at = chrono::DateTime::parse_from_rfc3339(&embedding.created_at)
        .map_err(|_| "embedding provenance timestamp is not RFC3339".to_string())?;
    let job_created_at = chrono::DateTime::parse_from_rfc3339(&job.created_at)
        .map_err(|_| "embedding job timestamp is not RFC3339".to_string())?;
    if embedding_created_at < job_created_at || embedding_created_at > chrono::Utc::now() {
        return Err(
            "embedding provenance timestamp is outside its canonical job lifetime".to_string(),
        );
    }
    Ok(())
}

fn validate_face(face: &FaceObservation) -> Result<(), String> {
    validate_text("face id", &face.face_id)?;
    validate_media_key(&face.media_key)?;
    validate_text("media fingerprint", &face.media_fingerprint)?;
    validate_text("pose bucket", &face.pose_bucket)?;
    if face.bounds_normalized.len() != 4
        || face
            .bounds_normalized
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
    {
        return Err("face bounds must contain four finite normalized values".to_string());
    }
    if face.quality.is_nan() || !face.quality.is_finite() {
        return Err("face quality must be finite".to_string());
    }
    if face.landmarks_normalized.iter().any(|point| {
        point.len() != 2
            || point
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
    }) {
        return Err("face landmarks must contain finite normalized x/y pairs".to_string());
    }
    Ok(())
}

fn is_real_yaw_bucket(value: &str) -> bool {
    matches!(value, "frontal" | "quarter" | "profile")
}

fn same_face_payload(left: &FaceObservation, right: &FaceObservation) -> bool {
    // A retry is idempotent only when it is the exact observation already
    // persisted. Geometry provenance and timestamps are part of the durable
    // evidence; accepting a changed value here would silently rewrite history.
    left == right
}

/// Restored observations retain their durable timestamps while excluded vectors
/// are regenerated by a later job. This is not a same-job replay exception and
/// never authorizes changing geometry, revisions, or operator-owned evidence.
fn can_reobserve_face_for_new_job(
    existing: &FaceObservation,
    incoming: &FaceObservation,
    job: &IndexJob,
    prior_embedding: Option<&FaceEmbedding>,
) -> bool {
    if existing.operator_owned
        || incoming.operator_owned
        || prior_embedding.is_some_and(|embedding| embedding.job_id == job.job_id)
    {
        return false;
    }
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    let Some(job_created) = parse(&job.created_at) else {
        return false;
    };
    let current_time = chrono::Utc::now();
    if [&existing.created_at, &existing.updated_at]
        .into_iter()
        .any(|value| !parse(value).is_some_and(|time| time < job_created))
        || [&incoming.created_at, &incoming.updated_at]
            .into_iter()
            .any(|value| {
                !parse(value).is_some_and(|time| time >= job_created && time <= current_time)
            })
    {
        return false;
    }
    let mut reproduced = incoming.clone();
    reproduced.created_at.clone_from(&existing.created_at);
    reproduced.updated_at.clone_from(&existing.updated_at);
    same_face_payload(existing, &reproduced)
}

/// SurrealDB upserts are last-write-wins inside one transaction. Reject an
/// internally duplicated fused result before opening the mutation guard so a
/// malformed model batch cannot silently collapse multiple detections into one
/// Face or embedding while still advancing the durable asset cursor.
fn validate_fused_publication_uniqueness(
    faces: &[FaceObservation],
    embeddings: &[FaceEmbedding],
) -> Result<(), String> {
    let mut face_ids = BTreeSet::new();
    let mut source_slots = BTreeSet::new();
    for face in faces {
        if !face_ids.insert(face.face_id.as_str()) {
            return Err("fused Match inference contains a duplicate canonical face_id".to_string());
        }
        if !source_slots.insert((
            face.media_key.as_str(),
            face.media_fingerprint.as_str(),
            face.schema_generation.as_str(),
            face.source_index,
        )) {
            return Err(
                "fused Match inference contains a duplicate media-identity/source-index slot"
                    .to_string(),
            );
        }
    }
    let mut embedding_ids = BTreeSet::new();
    for embedding in embeddings {
        if !embedding_ids.insert(embedding.embedding_id.as_str()) {
            return Err("fused Match inference contains a duplicate embedding_id".to_string());
        }
    }
    Ok(())
}

fn validate_vector(vector: &[f32]) -> Result<(), String> {
    if vector.len() != EMBEDDING_DIM {
        return Err(format!(
            "Match vector dimension mismatch: expected {EMBEDDING_DIM}, got {}",
            vector.len()
        ));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err("Match vector contains NaN or infinity".to_string());
    }
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    if !norm.is_finite() || norm <= f64::EPSILON {
        return Err("Match vector has zero or invalid norm".to_string());
    }
    Ok(())
}

fn exact_cosine(left: &[f32], right: &[f32]) -> Result<f32, String> {
    validate_vector(left)?;
    validate_vector(right)?;
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (&a, &b) in left.iter().zip(right) {
        dot += f64::from(a) * f64::from(b);
        left_norm += f64::from(a) * f64::from(a);
        right_norm += f64::from(b) * f64::from(b);
    }
    let score = dot / (left_norm.sqrt() * right_norm.sqrt());
    if score.is_finite() {
        Ok(score.clamp(-1.0, 1.0) as f32)
    } else {
        Err("exact cosine produced a non-finite value".to_string())
    }
}

fn trusted_index_build_digest(rows: &[TrustedSearchEmbedding]) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    #[derive(Serialize)]
    struct IndexedRow<'a> {
        membership_id: &'a str,
        person_id: &'a str,
        look_id: &'a str,
        face_id: &'a str,
        embedding_id: &'a str,
        vector: &'a [f32],
        model_generation: &'a str,
    }
    let mut rows = rows.to_vec();
    rows.sort_by(|left, right| left.membership_id.cmp(&right.membership_id));
    let indexed_rows = rows
        .iter()
        .map(|row| IndexedRow {
            membership_id: &row.membership_id,
            person_id: &row.person_id,
            look_id: &row.look_id,
            face_id: &row.face_id,
            embedding_id: &row.embedding_id,
            vector: &row.vector,
            model_generation: &row.model_generation,
        })
        .collect::<Vec<_>>();
    let mut digest = Sha256::new();
    digest.update(format!(
        "engine={STRICT_ANN_ENGINE_VERSION}\nseed=0\norder={STRICT_ANN_BUILD_ORDER}\nm={STRICT_HNSW_M}\nefc={STRICT_HNSW_EF_CONSTRUCTION}\n"
    ));
    digest.update(serde_json::to_vec(&indexed_rows).map_err(|error| error.to_string())?);
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_text(label: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if value.len() > MAX_TEXT_BYTES {
        return Err(format!("{label} exceeds {MAX_TEXT_BYTES} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} contains control characters"));
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(format!("{label} must be a lowercase SHA-256 digest"))
    }
}

fn validate_failure_code(value: &str) -> Result<(), String> {
    if FAILURE_CODES.contains(&value) {
        Ok(())
    } else {
        Err("failure code is not a canonical Match code".to_string())
    }
}

fn redacted_failure_code(value: &str) -> &str {
    if FAILURE_CODES.contains(&value) {
        value
    } else {
        "unknown"
    }
}

fn grouped_media_source_path(value: Value) -> Option<(String, String)> {
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .find_map(|value| grouped_media_source_path(value.clone()));
    }
    let media_key = value.get("media_key")?;
    let media_key = media_key.as_str().or_else(|| {
        media_key
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str)
    })?;
    let source_path = value.get("source_path")?;
    let source_path = source_path.as_str().or_else(|| {
        source_path
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str)
    })?;
    Some((media_key.to_string(), source_path.to_string()))
}

fn rekey_correction_snapshot_media_fields(
    value: &mut Value,
    old_media_key: &str,
    new_media_key: &str,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                rekey_correction_snapshot_media_fields(item, old_media_key, new_media_key);
            }
        }
        Value::Object(fields) => {
            for (field, nested) in fields {
                if matches!(field.as_str(), "media_key" | "cover_media_key")
                    && nested.as_str() == Some(old_media_key)
                {
                    *nested = Value::String(new_media_key.to_string());
                } else {
                    rekey_correction_snapshot_media_fields(nested, old_media_key, new_media_key);
                }
            }
        }
        _ => {}
    }
}

fn rekey_direct_operation_media_fields(
    operation: &mut MatchOperation,
    old_media_key: &str,
    new_media_key: &str,
) -> Result<(), String> {
    let rewrite_assignment = |assignment: &mut Assignment| {
        if assignment.media_key == old_media_key {
            assignment.media_key = new_media_key.to_string();
        }
    };
    match operation.kind.as_str() {
        "assign_operator_confirmed" | "assign_committed_strict_automatic" => {
            let mut before: Option<Assignment> = serde_json::from_str(&operation.before_json)
                .map_err(|error| format!("decode direct assignment rekey before-state: {error}"))?;
            if let Some(before) = &mut before {
                rewrite_assignment(before);
            }
            let mut after: Assignment = serde_json::from_str(&operation.after_json)
                .map_err(|error| format!("decode direct assignment rekey after-state: {error}"))?;
            rewrite_assignment(&mut after);
            operation.before_json =
                serde_json::to_string(&before).map_err(|error| error.to_string())?;
            operation.after_json =
                serde_json::to_string(&after).map_err(|error| error.to_string())?;
        }
        "different" => {
            let mut before: Option<Assignment> = serde_json::from_str(&operation.before_json)
                .map_err(|error| format!("decode direct Different rekey before-state: {error}"))?;
            if let Some(before) = &mut before {
                rewrite_assignment(before);
            }
            operation.before_json =
                serde_json::to_string(&before).map_err(|error| error.to_string())?;
        }
        "not_sure" => {
            let (mut assignment, constraint): (Option<Assignment>, Option<CannotLinkConstraint>) =
                serde_json::from_str(&operation.before_json).map_err(|error| {
                    format!("decode direct Not-sure rekey before-state: {error}")
                })?;
            if let Some(assignment) = &mut assignment {
                rewrite_assignment(assignment);
            }
            operation.before_json = serde_json::to_string(&(assignment, constraint))
                .map_err(|error| error.to_string())?;
        }
        "move_to_look" => {
            let mut before: Assignment = serde_json::from_str(&operation.before_json)
                .map_err(|error| format!("decode direct Look-move rekey before-state: {error}"))?;
            let mut after: Assignment = serde_json::from_str(&operation.after_json)
                .map_err(|error| format!("decode direct Look-move rekey after-state: {error}"))?;
            rewrite_assignment(&mut before);
            rewrite_assignment(&mut after);
            operation.before_json =
                serde_json::to_string(&before).map_err(|error| error.to_string())?;
            operation.after_json =
                serde_json::to_string(&after).map_err(|error| error.to_string())?;
        }
        _ => {
            return Err(format!(
                "media rekey does not support mapped direct operation kind {}",
                operation.kind
            ));
        }
    }
    for (field, value) in [
        ("before_json", &operation.before_json),
        ("after_json", &operation.after_json),
    ] {
        if value.len() > exchange::IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES {
            return Err(format!(
                "media rekey {field} exceeds the portable identity-bundle nested-string limit"
            ));
        }
    }
    Ok(())
}

fn correction_table_name_for_rekey(table: &corrections::CorrectionTable) -> &'static str {
    match table {
        corrections::CorrectionTable::Person => PERSON_TABLE,
        corrections::CorrectionTable::Look => LOOK_TABLE,
        corrections::CorrectionTable::TemplateSet => TEMPLATE_SET_TABLE,
        corrections::CorrectionTable::Face => FACE_TABLE,
        corrections::CorrectionTable::Embedding => EMBEDDING_TABLE,
        corrections::CorrectionTable::Assignment => ASSIGNMENT_TABLE,
        corrections::CorrectionTable::Constraint => CONSTRAINT_TABLE,
        corrections::CorrectionTable::TrustedMember => TRUSTED_MEMBER_TABLE,
        corrections::CorrectionTable::TrustedSearch => TRUSTED_SEARCH_TABLE,
        corrections::CorrectionTable::Disposition => FACE_DISPOSITION_TABLE,
        corrections::CorrectionTable::Suggestion => SUGGESTION_TABLE,
        corrections::CorrectionTable::VideoObservation => video::VIDEO_OBSERVATION_TABLE,
    }
}

fn correction_row_owner_matches_for_rekey(
    table: &corrections::CorrectionTable,
    current: &Option<Value>,
    recorded: &Option<Value>,
) -> Result<bool, String> {
    let (Some(current), Some(recorded)) = (current, recorded) else {
        return Ok(current.is_none() && recorded.is_none());
    };
    let recorded_omits_vector = recorded.get("vector").is_none()
        && recorded.get("vector_omitted").and_then(Value::as_bool) == Some(true);
    if recorded_omits_vector {
        macro_rules! projected_typed_equal {
            ($kind:ty) => {{
                let current_typed: $kind = serde_json::from_value(current.clone())
                    .map_err(|error| format!("decode current rekey owner row: {error}"))?;
                let canonical_current = serde_json::to_value(&current_typed)
                    .map_err(|error| format!("canonicalize current rekey owner row: {error}"))?;
                if !same_rekey_json_shape(current, &canonical_current) {
                    return Err(
                        "media rekey current owner row contains unsupported fields or shape"
                            .to_string(),
                    );
                }
                let mut projected = canonical_current;
                let projected = projected
                    .as_object_mut()
                    .ok_or("media rekey vector-bearing owner row is not a typed object")?;
                if projected.remove("vector").is_none() {
                    return Err(
                        "media rekey current owner row omits its regenerated vector".to_string()
                    );
                }
                projected.insert("vector_omitted".to_string(), Value::Bool(true));
                let projected = Value::Object(projected.clone());
                if !same_rekey_json_shape(recorded, &projected) {
                    return Err(
                        "media rekey portable owner row contains unsupported fields or shape"
                            .to_string(),
                    );
                }
                projected == *recorded
            }};
        }
        return Ok(match table {
            corrections::CorrectionTable::Embedding => {
                projected_typed_equal!(FaceEmbedding)
            }
            corrections::CorrectionTable::TrustedSearch => {
                projected_typed_equal!(TrustedSearchEmbedding)
            }
            _ => {
                return Err(
                    "media rekey portable vector omission appears on a non-vector row".to_string(),
                )
            }
        });
    }
    macro_rules! typed_equal {
        ($kind:ty) => {{
            let current_typed: $kind = serde_json::from_value(current.clone())
                .map_err(|error| format!("decode current rekey owner row: {error}"))?;
            let recorded_typed: $kind = serde_json::from_value(recorded.clone())
                .map_err(|error| format!("decode recorded rekey owner row: {error}"))?;
            let canonical_current = serde_json::to_value(&current_typed)
                .map_err(|error| format!("canonicalize current rekey owner row: {error}"))?;
            let canonical_recorded = serde_json::to_value(&recorded_typed)
                .map_err(|error| format!("canonicalize recorded rekey owner row: {error}"))?;
            if !same_rekey_json_shape(current, &canonical_current)
                || !same_rekey_json_shape(recorded, &canonical_recorded)
            {
                return Err(
                    "media rekey owner row contains unsupported fields or shape".to_string()
                );
            }
            current_typed == recorded_typed
        }};
    }
    Ok(match table {
        corrections::CorrectionTable::Person => typed_equal!(Person),
        corrections::CorrectionTable::Look => typed_equal!(Look),
        corrections::CorrectionTable::TemplateSet => typed_equal!(TrustedTemplateSet),
        corrections::CorrectionTable::Face => typed_equal!(FaceObservation),
        corrections::CorrectionTable::Embedding => typed_equal!(FaceEmbedding),
        corrections::CorrectionTable::Assignment => typed_equal!(Assignment),
        corrections::CorrectionTable::Constraint => typed_equal!(CannotLinkConstraint),
        corrections::CorrectionTable::TrustedMember => typed_equal!(TrustedTemplateMembership),
        corrections::CorrectionTable::TrustedSearch => typed_equal!(TrustedSearchEmbedding),
        corrections::CorrectionTable::Disposition => typed_equal!(corrections::FaceDisposition),
        corrections::CorrectionTable::Suggestion => typed_equal!(Suggestion),
        corrections::CorrectionTable::VideoObservation => typed_equal!(StoredVideoObservation),
    })
}

fn same_rekey_json_shape(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    right
                        .get(key)
                        .is_some_and(|right_value| same_rekey_json_shape(left_value, right_value))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_rekey_json_shape(left, right))
        }
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => true,
        _ => false,
    }
}

/// Exact lexical spellings only; this does not establish filesystem alias identity.
pub(crate) fn match_source_path_candidates(path: &Path) -> Result<Vec<String>, String> {
    let raw = path
        .to_str()
        .ok_or("match_source_resolution_unsupported: non-UTF8 source")?;
    validate_text("Match selected source", raw)?;
    let slash = raw.replace('\\', "/");
    let ordinary = if let Some(rest) = slash.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = slash.strip_prefix("//?/") {
        rest.to_string()
    } else {
        slash
    };
    let drive = ordinary
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && ordinary.as_bytes().get(1) == Some(&b':')
        && ordinary.as_bytes().get(2) == Some(&b'/');
    let unc = ordinary.starts_with("//") && !ordinary.starts_with("///");
    let components = if drive {
        &ordinary[3..]
    } else if unc {
        &ordinary[2..]
    } else {
        return Err(
            "match_source_resolution_unsupported: require absolute drive or UNC source".into(),
        );
    };
    let parts: Vec<_> = components.split('/').collect();
    if parts.len() < if unc { 3 } else { 1 }
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || part.ends_with(['.', ' '])
                || part.chars().any(|ch| {
                    ch.is_control() || matches!(ch, ':' | '?' | '*' | '"' | '<' | '>' | '|')
                })
        })
    {
        return Err(
            "match_source_resolution_unsupported: unsafe or ambiguous source components".into(),
        );
    }
    let mut candidates = Vec::new();
    if drive {
        for letter in [
            ordinary[..1].to_ascii_uppercase(),
            ordinary[..1].to_ascii_lowercase(),
        ] {
            let value = format!("{letter}{}", &ordinary[1..]);
            candidates.push(value.clone());
            candidates.push(value.replace('/', "\\"));
            candidates.push(format!(r"\\?\{}", value.replace('/', "\\")));
            candidates.push(format!("//?/{value}"));
        }
    } else {
        candidates.push(ordinary.clone());
        candidates.push(ordinary.replace('/', "\\"));
        candidates.push(format!(r"\\?\UNC\{}", ordinary[2..].replace('/', "\\")));
        candidates.push(format!("//?/UNC/{}", &ordinary[2..]));
    }
    candidates.sort();
    candidates.dedup();
    debug_assert!(candidates.len() <= 8);
    Ok(candidates)
}

fn validate_media_key(value: &str) -> Result<(), String> {
    validate_text("media key", value)?;
    if value.starts_with('/')
        || value.starts_with("//")
        || value.contains('\\')
        || value.contains(':')
        || value != value.to_lowercase()
        || value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(
            "media key must be canonical lowercase workspace-relative slash form".to_string(),
        );
    }
    Ok(())
}

fn required_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Match row omitted string field {field}"))
}

fn json_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(items) => items.iter().any(|item| json_contains(item, needle)),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key.contains(needle) || json_contains(value, needle)),
        _ => false,
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

fn suggestion_id(face_id: &str, person_id: &str) -> String {
    stable_pair_id("suggestion", face_id, person_id)
}

pub(crate) fn embedding_id(face_id: &str, model_generation: &str) -> String {
    stable_pair_id("embedding", face_id, model_generation)
}

/// Portable operation/media provenance stores one unambiguous byte identity:
/// raw lowercase SHA-256 hex. A display prefix is accepted from canonical Face
/// evidence, but malformed or uppercase digests fail closed instead of being
/// converted into invented provenance.
pub(crate) fn canonical_media_sha256(value: &str) -> Option<&str> {
    let digest = value.strip_prefix("sha256:").unwrap_or(value);
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(digest)
}

fn canonical_correction_media_fingerprint(value: &str) -> Result<String, String> {
    canonical_media_sha256(value)
        .map(str::to_string)
        .ok_or_else(|| {
            "correction media provenance requires a canonical lowercase SHA-256 fingerprint"
                .to_string()
        })
}

fn legacy_direct_media_backfill_error(detail: impl std::fmt::Display) -> String {
    format!("Match schema v17 to v18 direct-media backfill: {detail}")
}

#[derive(Default)]
struct LegacyDirectMediaEvidence {
    by_face: BTreeMap<String, BTreeSet<(String, String)>>,
    by_media: BTreeMap<String, BTreeSet<String>>,
}

impl LegacyDirectMediaEvidence {
    fn bind_face(&mut self, face: &FaceObservation, source: &str) -> Result<(), String> {
        validate_media_key(&face.media_key).map_err(legacy_direct_media_backfill_error)?;
        let fingerprint = canonical_correction_media_fingerprint(&face.media_fingerprint)
            .map_err(legacy_direct_media_backfill_error)?;
        self.by_face
            .entry(face.face_id.clone())
            .or_default()
            .insert((face.media_key.clone(), fingerprint.clone()));
        self.by_media
            .entry(face.media_key.clone())
            .or_default()
            .insert(fingerprint);
        if face.face_id.trim().is_empty() {
            return Err(legacy_direct_media_backfill_error(format!(
                "{source} contains an empty face_id"
            )));
        }
        Ok(())
    }

    fn bind_mapping(
        &mut self,
        mapping: &corrections::CorrectionMediaOperation,
        source: &str,
    ) -> Result<(), String> {
        validate_media_key(&mapping.media_key).map_err(legacy_direct_media_backfill_error)?;
        let fingerprint = canonical_correction_media_fingerprint(&mapping.media_fingerprint)
            .map_err(legacy_direct_media_backfill_error)?;
        if mapping.operation_id.trim().is_empty() || mapping.mapping_id.trim().is_empty() {
            return Err(legacy_direct_media_backfill_error(format!(
                "{source} contains an empty mapping identity"
            )));
        }
        self.by_media
            .entry(mapping.media_key.clone())
            .or_default()
            .insert(fingerprint);
        Ok(())
    }

    fn bind_suggestion_provenance(
        &mut self,
        provenance: &corrections::SuggestionSourceProvenance,
        relevant_face_ids: &BTreeSet<String>,
    ) -> Result<(), String> {
        if !relevant_face_ids.contains(&provenance.face_id) {
            return Err(legacy_direct_media_backfill_error(format!(
                "historical suggestion provenance {} resolved an unrelated face_id",
                provenance.provenance_id
            )));
        }
        validate_media_key(&provenance.media_key).map_err(legacy_direct_media_backfill_error)?;
        let fingerprint = canonical_correction_media_fingerprint(&provenance.media_fingerprint)
            .map_err(legacy_direct_media_backfill_error)?;
        self.by_face
            .entry(provenance.face_id.clone())
            .or_default()
            .insert((provenance.media_key.clone(), fingerprint.clone()));
        self.by_media
            .entry(provenance.media_key.clone())
            .or_default()
            .insert(fingerprint);
        Ok(())
    }

    fn bind_correction_snapshots(
        &mut self,
        operation: &MatchOperation,
        relevant_face_ids: &BTreeSet<String>,
    ) -> Result<(), String> {
        if !operation.kind.starts_with("correction_") {
            return Ok(());
        }
        let envelope: corrections::CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).map_err(|error| {
                legacy_direct_media_backfill_error(format!(
                    "decode historical correction operation {}: {error}",
                    operation.operation_id
                ))
            })?;
        for row in envelope
            .rows
            .iter()
            .filter(|row| row.table == corrections::CorrectionTable::Face)
        {
            if !relevant_face_ids.contains(&row.stable_id) {
                continue;
            }
            for snapshot in [&row.before, &row.after].into_iter().flatten() {
                let face: FaceObservation =
                    serde_json::from_value(snapshot.clone()).map_err(|error| {
                        legacy_direct_media_backfill_error(format!(
                            "decode historical Face snapshot {} in operation {}: {error}",
                            row.stable_id, operation.operation_id
                        ))
                    })?;
                if face.face_id != row.stable_id {
                    return Err(legacy_direct_media_backfill_error(format!(
                        "historical Face snapshot {} in operation {} has mismatched identity",
                        row.stable_id, operation.operation_id
                    )));
                }
                self.bind_face(&face, "historical correction Face snapshot")?;
            }
        }
        Ok(())
    }
}

fn require_legacy_assignment_media_key(
    assignment: &Assignment,
    operation: &MatchOperation,
    label: &str,
) -> Result<String, String> {
    let face_id = operation.face_id.as_deref().ok_or_else(|| {
        legacy_direct_media_backfill_error(format!(
            "{} operation {} omitted face_id",
            operation.kind, operation.operation_id
        ))
    })?;
    if assignment.face_id != face_id || assignment.assignment_id != face_id {
        return Err(legacy_direct_media_backfill_error(format!(
            "{} operation {} has a mismatched {label} Assignment",
            operation.kind, operation.operation_id
        )));
    }
    validate_media_key(&assignment.media_key).map_err(legacy_direct_media_backfill_error)?;
    Ok(assignment.media_key.clone())
}

/// Recover the one media key encoded by a pre-WP085 direct operation. The
/// operation envelope is parsed according to its exact direct kind; current
/// Face evidence may complete kinds whose envelope intentionally contains no
/// Assignment, but conflicting sources are never reconciled heuristically.
fn legacy_direct_operation_media_key(
    operation: &MatchOperation,
    face: Option<&FaceObservation>,
    historical_faces: Option<&BTreeSet<(String, String)>>,
    existing_mapping: Option<&corrections::CorrectionMediaOperation>,
) -> Result<String, String> {
    if !LEGACY_DIRECT_MEDIA_KINDS.contains(&operation.kind.as_str()) {
        return Err(legacy_direct_media_backfill_error(format!(
            "unsupported direct operation kind {}",
            operation.kind
        )));
    }
    let expected_face_id = operation.face_id.as_deref().ok_or_else(|| {
        legacy_direct_media_backfill_error(format!(
            "{} operation {} omitted face_id",
            operation.kind, operation.operation_id
        ))
    })?;
    let mut media_keys = BTreeSet::new();
    match operation.kind.as_str() {
        "assign_operator_confirmed" | "assign_committed_strict_automatic" => {
            let before: Option<Assignment> =
                serde_json::from_str(&operation.before_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode {} operation {} before-state: {error}",
                        operation.kind, operation.operation_id
                    ))
                })?;
            if let Some(before) = &before {
                media_keys.insert(require_legacy_assignment_media_key(
                    before,
                    operation,
                    "before-state",
                )?);
            }
            let after: Assignment =
                serde_json::from_str(&operation.after_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode {} operation {} after-state: {error}",
                        operation.kind, operation.operation_id
                    ))
                })?;
            if after.operation_id != operation.operation_id {
                return Err(legacy_direct_media_backfill_error(format!(
                    "{} operation {} has a mismatched after-state owner",
                    operation.kind, operation.operation_id
                )));
            }
            media_keys.insert(require_legacy_assignment_media_key(
                &after,
                operation,
                "after-state",
            )?);
        }
        "different" => {
            let before: Option<Assignment> =
                serde_json::from_str(&operation.before_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode Different operation {} before-state: {error}",
                        operation.operation_id
                    ))
                })?;
            if let Some(before) = &before {
                media_keys.insert(require_legacy_assignment_media_key(
                    before,
                    operation,
                    "before-state",
                )?);
            }
            let after: CannotLinkConstraint =
                serde_json::from_str(&operation.after_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode Different operation {} after-state: {error}",
                        operation.operation_id
                    ))
                })?;
            if after.face_id != expected_face_id || after.operation_id != operation.operation_id {
                return Err(legacy_direct_media_backfill_error(format!(
                    "Different operation {} has mismatched constraint evidence",
                    operation.operation_id
                )));
            }
        }
        "not_sure" => {
            let (before_assignment, before_constraint): (
                Option<Assignment>,
                Option<CannotLinkConstraint>,
            ) = serde_json::from_str(&operation.before_json).map_err(|error| {
                legacy_direct_media_backfill_error(format!(
                    "decode Not-sure operation {} before-state: {error}",
                    operation.operation_id
                ))
            })?;
            if let Some(before) = &before_assignment {
                media_keys.insert(require_legacy_assignment_media_key(
                    before,
                    operation,
                    "before-state",
                )?);
            }
            if before_constraint
                .as_ref()
                .is_some_and(|constraint| constraint.face_id != expected_face_id)
                || serde_json::from_str::<Option<Value>>(&operation.after_json)
                    .map_err(|error| {
                        legacy_direct_media_backfill_error(format!(
                            "decode Not-sure operation {} after-state: {error}",
                            operation.operation_id
                        ))
                    })?
                    .is_some()
            {
                return Err(legacy_direct_media_backfill_error(format!(
                    "Not-sure operation {} has mismatched no-change evidence",
                    operation.operation_id
                )));
            }
        }
        "move_to_look" => {
            let before: Assignment =
                serde_json::from_str(&operation.before_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode Look-move operation {} before-state: {error}",
                        operation.operation_id
                    ))
                })?;
            let after: Assignment =
                serde_json::from_str(&operation.after_json).map_err(|error| {
                    legacy_direct_media_backfill_error(format!(
                        "decode Look-move operation {} after-state: {error}",
                        operation.operation_id
                    ))
                })?;
            if after.operation_id != operation.operation_id {
                return Err(legacy_direct_media_backfill_error(format!(
                    "Look-move operation {} has a mismatched after-state owner",
                    operation.operation_id
                )));
            }
            media_keys.insert(require_legacy_assignment_media_key(
                &before,
                operation,
                "before-state",
            )?);
            media_keys.insert(require_legacy_assignment_media_key(
                &after,
                operation,
                "after-state",
            )?);
        }
        _ => unreachable!("direct operation kind checked above"),
    }
    if let Some(face) = face {
        if face.face_id != expected_face_id {
            return Err(legacy_direct_media_backfill_error(format!(
                "operation {} resolved the wrong Face evidence",
                operation.operation_id
            )));
        }
        validate_media_key(&face.media_key).map_err(legacy_direct_media_backfill_error)?;
        media_keys.insert(face.media_key.clone());
    }
    if let Some(historical_faces) = historical_faces {
        for (media_key, _) in historical_faces {
            validate_media_key(media_key).map_err(legacy_direct_media_backfill_error)?;
            media_keys.insert(media_key.clone());
        }
    }
    if let Some(mapping) = existing_mapping {
        if mapping.operation_id != operation.operation_id
            || mapping.kind != operation.kind
            || mapping.created_at != operation.created_at
        {
            return Err(legacy_direct_media_backfill_error(format!(
                "operation {} has mismatched persisted mapping evidence",
                operation.operation_id
            )));
        }
        validate_media_key(&mapping.media_key).map_err(legacy_direct_media_backfill_error)?;
        media_keys.insert(mapping.media_key.clone());
    }
    if media_keys.len() != 1 {
        return Err(legacy_direct_media_backfill_error(format!(
            "operation {} has {} canonical media-key candidates; exact current/historical evidence is required",
            operation.operation_id,
            media_keys.len()
        )));
    }
    Ok(media_keys.into_iter().next().expect("singleton checked"))
}

fn derive_legacy_direct_media_backfills(
    operations: &[MatchOperation],
    faces: &BTreeMap<String, FaceObservation>,
    historical_evidence: &LegacyDirectMediaEvidence,
    existing_mappings_by_operation: &BTreeMap<String, corrections::CorrectionMediaOperation>,
) -> Result<Vec<corrections::CorrectionMediaOperation>, String> {
    let mut backfills = Vec::new();
    for operation in operations {
        let face_id = operation.face_id.as_deref().ok_or_else(|| {
            legacy_direct_media_backfill_error(format!(
                "{} operation {} omitted face_id",
                operation.kind, operation.operation_id
            ))
        })?;
        let face = faces.get(face_id);
        let historical_faces = historical_evidence.by_face.get(face_id);
        let existing = existing_mappings_by_operation.get(&operation.operation_id);
        let media_key =
            legacy_direct_operation_media_key(operation, face, historical_faces, existing)?;
        let mapping_id =
            corrections::correction_media_mapping_id(&operation.operation_id, &media_key);
        let mut fingerprint_candidates = historical_evidence
            .by_media
            .get(&media_key)
            .cloned()
            .unwrap_or_default();
        if let Some(face) = face.filter(|face| face.media_key == media_key) {
            fingerprint_candidates.insert(
                canonical_correction_media_fingerprint(&face.media_fingerprint)
                    .map_err(legacy_direct_media_backfill_error)?,
            );
        }
        if let Some(historical_faces) = historical_faces {
            for (historical_media_key, fingerprint) in historical_faces {
                if historical_media_key == &media_key {
                    fingerprint_candidates.insert(fingerprint.clone());
                }
            }
        }
        if let Some(existing) = existing {
            let existing_fingerprint =
                canonical_correction_media_fingerprint(&existing.media_fingerprint)
                    .map_err(legacy_direct_media_backfill_error)?;
            fingerprint_candidates.insert(existing_fingerprint.clone());
            if existing.mapping_id != mapping_id
                || existing.operation_id != operation.operation_id
                || existing.media_key != media_key
                || existing.kind != operation.kind
                || existing.created_at != operation.created_at
                || fingerprint_candidates
                    .iter()
                    .any(|fingerprint| fingerprint != &existing_fingerprint)
            {
                return Err(legacy_direct_media_backfill_error(format!(
                    "operation {} has conflicting persisted media provenance",
                    operation.operation_id
                )));
            }
            continue;
        }
        if fingerprint_candidates.len() != 1 {
            return Err(legacy_direct_media_backfill_error(format!(
                "operation {} has {} canonical fingerprint candidates; exact agreeing current/historical evidence is required",
                operation.operation_id,
                fingerprint_candidates.len()
            )));
        }
        let media_fingerprint = fingerprint_candidates
            .into_iter()
            .next()
            .expect("singleton checked");
        backfills.push(corrections::CorrectionMediaOperation {
            mapping_id,
            media_key,
            media_fingerprint,
            operation_id: operation.operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        });
    }
    Ok(backfills)
}

pub(crate) fn derived_face_id(
    media_key: &str,
    media_fingerprint: &str,
    source_index: u32,
    schema_generation: &str,
) -> String {
    stable_pair_id(
        "face",
        media_key,
        &format!("{media_fingerprint}\0{source_index}\0{schema_generation}"),
    )
}

fn cannot_link_id(face_id: &str, person_id: &str) -> String {
    stable_pair_id("cannot_link", face_id, person_id)
}

fn trusted_member_id(set_id: &str, face_id: &str) -> String {
    stable_pair_id("trusted", set_id, face_id)
}

fn job_asset_id(job_id: &str, media_key: &str) -> String {
    stable_pair_id("asset", job_id, media_key)
}

fn stable_pair_id(prefix: &str, left: &str, right: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(prefix.as_bytes());
    hash.update([0]);
    hash.update(left.as_bytes());
    hash.update([0]);
    hash.update(right.as_bytes());
    format!("{prefix}-{:x}", hash.finalize())
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const MATCH_SCHEMA_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE TABLE IF NOT EXISTS match_schema_state SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS schema_version ON TABLE match_schema_state TYPE int;
DEFINE FIELD IF NOT EXISTS engine_version ON TABLE match_schema_state TYPE string;
DEFINE FIELD IF NOT EXISTS schema_generation ON TABLE match_schema_state TYPE string;
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE match_schema_state TYPE string;
DEFINE TABLE OVERWRITE match_identity_import_journal SCHEMAFULL;
DEFINE FIELD OVERWRITE journal_id ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE version ON TABLE match_identity_import_journal TYPE int;
DEFINE FIELD OVERWRITE plan_token ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE pre_state_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_path ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_file_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_content_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE relocations_json ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE phase ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_identity_import_journal TYPE string;
DEFINE INDEX OVERWRITE match_identity_import_journal_id ON TABLE match_identity_import_journal FIELDS journal_id UNIQUE;
DEFINE TABLE OVERWRITE match_recovery_face_closure SCHEMAFULL;
DEFINE FIELD OVERWRITE closure_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE FIELD OVERWRITE run_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE INDEX OVERWRITE match_recovery_face_closure_id ON TABLE match_recovery_face_closure FIELDS closure_id UNIQUE;
DEFINE INDEX OVERWRITE match_recovery_face_closure_run ON TABLE match_recovery_face_closure FIELDS run_id, closure_id;
DEFINE INDEX OVERWRITE match_recovery_face_closure_run_face ON TABLE match_recovery_face_closure FIELDS run_id, face_id UNIQUE;
DEFINE TABLE OVERWRITE match_person SCHEMAFULL;
DEFINE FIELD OVERWRITE person_id ON TABLE match_person TYPE string;
DEFINE FIELD OVERWRITE name ON TABLE match_person TYPE string;
DEFINE FIELD OVERWRITE aliases ON TABLE match_person TYPE array<string>;
DEFINE FIELD OVERWRITE cover_media_key ON TABLE match_person TYPE option<string>;
DEFINE FIELD OVERWRITE hidden ON TABLE match_person TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE favorite ON TABLE match_person TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE revision ON TABLE match_person TYPE int;
DEFINE FIELD OVERWRITE catalog_revision ON TABLE match_person TYPE int;
DEFINE FIELD OVERWRITE created_at ON TABLE match_person TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_person TYPE string;
DEFINE INDEX OVERWRITE match_person_id ON TABLE match_person FIELDS person_id UNIQUE;

DEFINE TABLE OVERWRITE match_index_root SCHEMAFULL;
DEFINE FIELD OVERWRITE root_id ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE path ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE exclusions ON TABLE match_index_root TYPE array<string>;
DEFINE FIELD OVERWRITE enabled ON TABLE match_index_root TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_index_root TYPE string;
DEFINE INDEX OVERWRITE match_index_root_id ON TABLE match_index_root FIELDS root_id UNIQUE;

DEFINE TABLE OVERWRITE match_look SCHEMAFULL;
DEFINE FIELD OVERWRITE look_id ON TABLE match_look TYPE string;
DEFINE FIELD OVERWRITE person_id ON TABLE match_look TYPE string;
DEFINE FIELD OVERWRITE name ON TABLE match_look TYPE string;
DEFINE FIELD OVERWRITE revision ON TABLE match_look TYPE int;
DEFINE FIELD OVERWRITE created_at ON TABLE match_look TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_look TYPE string;
DEFINE INDEX OVERWRITE match_look_id ON TABLE match_look FIELDS look_id UNIQUE;
DEFINE INDEX OVERWRITE match_look_person ON TABLE match_look FIELDS person_id;

DEFINE TABLE OVERWRITE match_trusted_template_set SCHEMAFULL;
DEFINE FIELD OVERWRITE set_id ON TABLE match_trusted_template_set TYPE string;
DEFINE FIELD OVERWRITE look_id ON TABLE match_trusted_template_set TYPE string;
DEFINE FIELD OVERWRITE name ON TABLE match_trusted_template_set TYPE string;
DEFINE FIELD OVERWRITE revision ON TABLE match_trusted_template_set TYPE int;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_template_set TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_trusted_template_set TYPE string;
DEFINE INDEX OVERWRITE match_template_set_id ON TABLE match_trusted_template_set FIELDS set_id UNIQUE;
DEFINE INDEX OVERWRITE match_template_set_look ON TABLE match_trusted_template_set FIELDS look_id;

DEFINE TABLE OVERWRITE match_trusted_member SCHEMAFULL;
DEFINE FIELD OVERWRITE membership_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE set_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE look_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE authorized ON TABLE match_trusted_member TYPE bool;
DEFINE FIELD OVERWRITE alignment_valid ON TABLE match_trusted_member TYPE bool;
DEFINE FIELD OVERWRITE quality_passed ON TABLE match_trusted_member TYPE bool;
DEFINE FIELD OVERWRITE pose_passed ON TABLE match_trusted_member TYPE bool;
DEFINE FIELD OVERWRITE diversity_passed ON TABLE match_trusted_member TYPE bool;
DEFINE FIELD OVERWRITE provenance ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE embedding_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_trusted_member TYPE int;
DEFINE FIELD OVERWRITE quality_score ON TABLE match_trusted_member TYPE float;
DEFINE FIELD OVERWRITE quality_threshold ON TABLE match_trusted_member TYPE float;
DEFINE FIELD OVERWRITE pose_bucket ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE policy_version ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_trusted_member TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_member TYPE string;
DEFINE INDEX OVERWRITE match_trusted_member_id ON TABLE match_trusted_member FIELDS membership_id UNIQUE;
DEFINE INDEX OVERWRITE match_trusted_member_set ON TABLE match_trusted_member FIELDS set_id;
DEFINE INDEX OVERWRITE match_trusted_member_face ON TABLE match_trusted_member FIELDS face_id;

DEFINE TABLE OVERWRITE match_face_observation SCHEMAFULL;
DEFINE FIELD OVERWRITE face_id ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE source_index ON TABLE match_face_observation TYPE int;
DEFINE FIELD OVERWRITE source_width ON TABLE match_face_observation TYPE option<int>;
DEFINE FIELD OVERWRITE source_height ON TABLE match_face_observation TYPE option<int>;
DEFINE FIELD OVERWRITE exif_orientation ON TABLE match_face_observation TYPE option<int>;
DEFINE FIELD OVERWRITE bounds_normalized ON TABLE match_face_observation TYPE array<float> ASSERT array::len($value) = 4;
DEFINE FIELD OVERWRITE landmarks_normalized ON TABLE match_face_observation TYPE array<array<float>>;
DEFINE FIELD OVERWRITE alignment_valid ON TABLE match_face_observation TYPE bool;
DEFINE FIELD OVERWRITE quality ON TABLE match_face_observation TYPE float;
DEFINE FIELD OVERWRITE pose_bucket ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE operator_owned ON TABLE match_face_observation TYPE bool;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_face_observation TYPE int;
DEFINE FIELD OVERWRITE created_at ON TABLE match_face_observation TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_face_observation TYPE string;
DEFINE INDEX OVERWRITE match_face_id ON TABLE match_face_observation FIELDS face_id UNIQUE;
DEFINE INDEX OVERWRITE match_face_media ON TABLE match_face_observation FIELDS media_key;
DEFINE INDEX OVERWRITE match_face_derived_identity ON TABLE match_face_observation FIELDS media_key, media_fingerprint, source_index, schema_generation UNIQUE;

DEFINE TABLE OVERWRITE match_face_embedding SCHEMAFULL;
DEFINE FIELD OVERWRITE embedding_id ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE vector ON TABLE match_face_embedding TYPE array<float> ASSERT array::len($value) = 512;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_face_embedding TYPE int;
DEFINE FIELD OVERWRITE job_id ON TABLE match_face_embedding TYPE string;
DEFINE FIELD OVERWRITE active ON TABLE match_face_embedding TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_face_embedding TYPE string;
DEFINE INDEX OVERWRITE match_embedding_id ON TABLE match_face_embedding FIELDS embedding_id UNIQUE;
DEFINE INDEX OVERWRITE match_embedding_face_generation ON TABLE match_face_embedding FIELDS face_id, model_generation UNIQUE;
DEFINE INDEX IF NOT EXISTS match_face_embedding_hnsw_v1 ON TABLE match_face_embedding FIELDS vector HNSW DIMENSION 512 DIST COSINE TYPE F32 EFC 150 M 12;

DEFINE TABLE OVERWRITE match_trusted_search_embedding SCHEMAFULL;
DEFINE FIELD OVERWRITE membership_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE person_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE look_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE embedding_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE vector ON TABLE match_trusted_search_embedding TYPE array<float> ASSERT array::len($value) = 512;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_search_embedding TYPE string;
DEFINE INDEX OVERWRITE match_trusted_search_membership ON TABLE match_trusted_search_embedding FIELDS membership_id UNIQUE;
DEFINE INDEX OVERWRITE match_trusted_search_face_person ON TABLE match_trusted_search_embedding FIELDS face_id, person_id;
DEFINE INDEX IF NOT EXISTS match_trusted_search_hnsw_v1 ON TABLE match_trusted_search_embedding FIELDS vector HNSW DIMENSION 512 DIST COSINE TYPE F32 EFC 150 M 12;

DEFINE TABLE OVERWRITE match_trusted_index_build SCHEMAFULL;
DEFINE FIELD OVERWRITE receipt_id ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE build_digest ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE source_row_count ON TABLE match_trusted_index_build TYPE int;
DEFINE FIELD OVERWRITE engine_version ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE build_seed ON TABLE match_trusted_index_build TYPE int;
DEFINE FIELD OVERWRITE build_order ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_index_build TYPE string;
DEFINE INDEX OVERWRITE match_trusted_index_build_receipt ON TABLE match_trusted_index_build FIELDS receipt_id UNIQUE;

DEFINE TABLE OVERWRITE match_calibration_activation SCHEMAFULL;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE runtime_configuration_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE activation_integrity_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE trusted_index_build_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE gallery_members_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE verifier_artifact_id ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE contract_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE raw_records_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE review_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE automatic_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE suggestion_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE runner_up_margin ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE minimum_quality ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE candidate_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE rerank_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE people_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE looks_per_person_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE templates_per_look_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE total_templates_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE verifier_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE independent_review_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE wp084_runtime_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE wp087_release_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE active ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE invalidation_reason ON TABLE match_calibration_activation TYPE option<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_calibration_activation TYPE string;
DEFINE INDEX OVERWRITE match_calibration_generation ON TABLE match_calibration_activation FIELDS calibration_generation UNIQUE;

DEFINE TABLE OVERWRITE match_calibration_spent_set SCHEMAFULL;
DEFINE FIELD OVERWRITE activation_run_id ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE fixture_manifest_sha256 ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE person_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE acquisition_cluster_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_spent_set TYPE string;
DEFINE INDEX OVERWRITE match_calibration_spent_run ON TABLE match_calibration_spent_set FIELDS activation_run_id UNIQUE;

DEFINE TABLE OVERWRITE match_assignment SCHEMAFULL;
DEFINE FIELD OVERWRITE assignment_id ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE person_id ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE look_id ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE placement ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE state ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE provenance ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE locked ON TABLE match_assignment TYPE bool;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_assignment TYPE int;
DEFINE FIELD OVERWRITE person_revision ON TABLE match_assignment TYPE int;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_assignment TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_assignment TYPE string;
DEFINE INDEX OVERWRITE match_assignment_face ON TABLE match_assignment FIELDS face_id UNIQUE;
DEFINE INDEX OVERWRITE match_assignment_person ON TABLE match_assignment FIELDS person_id, media_key;
DEFINE INDEX OVERWRITE match_assignment_person_inventory ON TABLE match_assignment FIELDS person_id, assignment_id;

DEFINE TABLE OVERWRITE match_suggestion SCHEMAFULL;
DEFINE FIELD OVERWRITE suggestion_id ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE candidate_person_id ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE similarity ON TABLE match_suggestion TYPE float;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_suggestion TYPE option<string>;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_suggestion TYPE option<string>;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE face_revision ON TABLE match_suggestion TYPE int;
DEFINE FIELD OVERWRITE person_revision ON TABLE match_suggestion TYPE int;
DEFINE FIELD OVERWRITE job_id ON TABLE match_suggestion TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_suggestion TYPE string;
DEFINE INDEX OVERWRITE match_suggestion_id ON TABLE match_suggestion FIELDS suggestion_id UNIQUE;
DEFINE INDEX OVERWRITE match_suggestion_face_person ON TABLE match_suggestion FIELDS face_id, candidate_person_id UNIQUE;
DEFINE INDEX OVERWRITE match_suggestion_person_inventory ON TABLE match_suggestion FIELDS candidate_person_id, suggestion_id;
DEFINE INDEX OVERWRITE match_suggestion_person_face_inventory ON TABLE match_suggestion FIELDS candidate_person_id, face_id;

DEFINE TABLE OVERWRITE match_constraint SCHEMAFULL;
DEFINE FIELD OVERWRITE constraint_id ON TABLE match_constraint TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_constraint TYPE string;
DEFINE FIELD OVERWRITE person_id ON TABLE match_constraint TYPE string;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_constraint TYPE string;
DEFINE FIELD OVERWRITE operator_owned ON TABLE match_constraint TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_constraint TYPE string;
DEFINE INDEX OVERWRITE match_constraint_id ON TABLE match_constraint FIELDS constraint_id UNIQUE;
DEFINE INDEX OVERWRITE match_constraint_face_person ON TABLE match_constraint FIELDS face_id, person_id UNIQUE;

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

DEFINE TABLE OVERWRITE match_operation SCHEMAFULL;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_operation TYPE string;
DEFINE FIELD OVERWRITE kind ON TABLE match_operation TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_operation TYPE option<string>;
DEFINE FIELD OVERWRITE person_id ON TABLE match_operation TYPE option<string>;
DEFINE FIELD OVERWRITE before_json ON TABLE match_operation TYPE string;
DEFINE FIELD OVERWRITE after_json ON TABLE match_operation TYPE string;
DEFINE FIELD OVERWRITE reversible ON TABLE match_operation TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_operation TYPE string;
DEFINE INDEX OVERWRITE match_operation_id ON TABLE match_operation FIELDS operation_id UNIQUE;
DEFINE INDEX OVERWRITE match_operation_kind_face ON TABLE match_operation FIELDS kind, face_id, operation_id;
DEFINE INDEX OVERWRITE match_operation_kind_operation ON TABLE match_operation FIELDS kind, operation_id;

DEFINE TABLE OVERWRITE match_index_job SCHEMAFULL;
DEFINE FIELD OVERWRITE job_id ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE root_key ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE lifecycle ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE identity_revision ON TABLE match_index_job TYPE int;
DEFINE FIELD OVERWRITE catalog_revision ON TABLE match_index_job TYPE int;
DEFINE FIELD OVERWRITE discovered ON TABLE match_index_job TYPE int;
DEFINE FIELD OVERWRITE completed ON TABLE match_index_job TYPE int;
DEFINE FIELD OVERWRITE failed ON TABLE match_index_job TYPE int;
DEFINE FIELD OVERWRITE skipped ON TABLE match_index_job TYPE int DEFAULT 0;
DEFINE FIELD OVERWRITE failure_code ON TABLE match_index_job TYPE option<string>;
DEFINE FIELD OVERWRITE failure_message ON TABLE match_index_job TYPE option<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_index_job TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_index_job TYPE string;
DEFINE INDEX OVERWRITE match_job_id ON TABLE match_index_job FIELDS job_id UNIQUE;

DEFINE TABLE OVERWRITE match_job_asset SCHEMAFULL;
DEFINE FIELD OVERWRITE asset_id ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE job_id ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE source_path ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE next_stage ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE completed_stages ON TABLE match_job_asset TYPE array<string>;
DEFINE FIELD OVERWRITE failure_code ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE failure_message ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE skipped_code ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE skipped_message ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_job_asset TYPE string;
DEFINE FIELD OVERWRITE identity_revision ON TABLE match_job_asset TYPE int;
DEFINE FIELD OVERWRITE catalog_revision ON TABLE match_job_asset TYPE int;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_job_asset TYPE string;
DEFINE INDEX OVERWRITE match_job_asset_id ON TABLE match_job_asset FIELDS asset_id UNIQUE;
DEFINE INDEX OVERWRITE match_job_asset_job ON TABLE match_job_asset FIELDS job_id;
DEFINE INDEX OVERWRITE match_job_asset_media ON TABLE match_job_asset FIELDS media_key, updated_at, asset_id;
DEFINE INDEX OVERWRITE match_job_asset_source ON TABLE match_job_asset FIELDS source_path, media_key;

DEFINE TABLE OVERWRITE match_execution SCHEMAFULL;
DEFINE FIELD OVERWRITE desired_mode ON TABLE match_execution TYPE string;
DEFINE FIELD OVERWRITE revision ON TABLE match_execution TYPE int;
DEFINE FIELD OVERWRITE identity_revision ON TABLE match_execution TYPE int;
DEFINE FIELD OVERWRITE catalog_revision ON TABLE match_execution TYPE int;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_execution TYPE string;

DEFINE TABLE OVERWRITE match_model_generation SCHEMAFULL;
DEFINE FIELD OVERWRITE generation ON TABLE match_model_generation TYPE string;
DEFINE FIELD OVERWRITE state ON TABLE match_model_generation TYPE string;
DEFINE FIELD OVERWRITE validated ON TABLE match_model_generation TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_model_generation TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_model_generation TYPE string;
DEFINE INDEX OVERWRITE match_generation_id ON TABLE match_model_generation FIELDS generation UNIQUE;

DEFINE TABLE OVERWRITE match_people_projection SCHEMAFULL;
DEFINE FIELD OVERWRITE media_key ON TABLE match_people_projection TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_people_projection TYPE string;
DEFINE FIELD OVERWRITE schema_generation ON TABLE match_people_projection TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_people_projection TYPE string;
DEFINE FIELD OVERWRITE identity_revision ON TABLE match_people_projection TYPE int;
DEFINE FIELD OVERWRITE catalog_revision ON TABLE match_people_projection TYPE int;
DEFINE FIELD OVERWRITE person_ids ON TABLE match_people_projection TYPE array<string>;
DEFINE FIELD OVERWRITE published_at ON TABLE match_people_projection TYPE string;
DEFINE INDEX OVERWRITE match_projection_media ON TABLE match_people_projection FIELDS media_key UNIQUE;

UPSERT match_schema_state:global SET schema_version = $schema_version, engine_version = $engine_version, schema_generation = $schema_generation, updated_at = $updated_at;
UPSERT match_execution:global SET desired_mode = desired_mode ?? 'running', revision = revision ?? 1, identity_revision = identity_revision ?? 1, catalog_revision = catalog_revision ?? 1, updated_at = updated_at ?? $updated_at;
COMMIT TRANSACTION;
"#;

const MATCH_SCHEMA_V2_TO_V3_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_assignment TYPE option<string>;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_suggestion TYPE option<string>;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_suggestion TYPE option<string>;
DEFINE TABLE OVERWRITE match_trusted_search_embedding SCHEMAFULL;
DEFINE FIELD OVERWRITE membership_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE person_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE look_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE embedding_id ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE vector ON TABLE match_trusted_search_embedding TYPE array<float> ASSERT array::len($value) = 512;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_trusted_search_embedding TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_search_embedding TYPE string;
DEFINE INDEX OVERWRITE match_trusted_search_membership ON TABLE match_trusted_search_embedding FIELDS membership_id UNIQUE;
DEFINE INDEX IF NOT EXISTS match_trusted_search_hnsw_v1 ON TABLE match_trusted_search_embedding FIELDS vector HNSW DIMENSION 512 DIST COSINE TYPE F32 EFC 150 M 12;
DEFINE TABLE OVERWRITE match_calibration_activation SCHEMAFULL;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE gallery_members_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE verifier_artifact_id ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE contract_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE raw_records_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE review_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE automatic_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE suggestion_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE runner_up_margin ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE minimum_quality ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE candidate_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE rerank_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE people_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE looks_per_person_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE templates_per_look_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE total_templates_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE verifier_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE independent_review_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE wp084_runtime_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE wp087_release_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE active ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE invalidation_reason ON TABLE match_calibration_activation TYPE option<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_calibration_activation TYPE string;
DEFINE INDEX OVERWRITE match_calibration_generation ON TABLE match_calibration_activation FIELDS calibration_generation UNIQUE;
DEFINE TABLE OVERWRITE match_calibration_spent_set SCHEMAFULL;
DEFINE FIELD OVERWRITE activation_run_id ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE fixture_manifest_sha256 ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE person_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE acquisition_cluster_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_spent_set TYPE string;
DEFINE INDEX OVERWRITE match_calibration_spent_run ON TABLE match_calibration_spent_set FIELDS activation_run_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// V3 activations were caller-asserted and therefore are not migration-grade
// evidence. Discard only those unsafe activation rows, preserve all identity
// truth and trusted authorization, then install the content-addressed schema.
const MATCH_SCHEMA_V3_TO_V4_SQL: &str = r#"
BEGIN TRANSACTION;
DELETE match_calibration_activation;
DEFINE TABLE OVERWRITE match_calibration_activation SCHEMAFULL;
DEFINE FIELD OVERWRITE calibration_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE model_generation ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE envelope_hash ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE gallery_members_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE verifier_artifact_id ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE contract_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE raw_records_sha256 ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE review_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE automatic_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE suggestion_threshold ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE runner_up_margin ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE minimum_quality ON TABLE match_calibration_activation TYPE float;
DEFINE FIELD OVERWRITE candidate_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE rerank_k ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE people_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE looks_per_person_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE templates_per_look_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE total_templates_max ON TABLE match_calibration_activation TYPE int;
DEFINE FIELD OVERWRITE verifier_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE independent_review_verdict ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE wp084_runtime_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE wp087_release_ready ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE active ON TABLE match_calibration_activation TYPE bool;
DEFINE FIELD OVERWRITE invalidation_reason ON TABLE match_calibration_activation TYPE option<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_calibration_activation TYPE string;
DEFINE INDEX OVERWRITE match_calibration_generation ON TABLE match_calibration_activation FIELDS calibration_generation UNIQUE;
DEFINE TABLE OVERWRITE match_calibration_spent_set SCHEMAFULL;
DEFINE FIELD OVERWRITE activation_run_id ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE fixture_manifest_sha256 ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE evidence_digest ON TABLE match_calibration_spent_set TYPE string;
DEFINE FIELD OVERWRITE person_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE acquisition_cluster_hashes ON TABLE match_calibration_spent_set TYPE array<string>;
DEFINE FIELD OVERWRITE created_at ON TABLE match_calibration_spent_set TYPE string;
DEFINE INDEX OVERWRITE match_calibration_spent_run ON TABLE match_calibration_spent_set FIELDS activation_run_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// The v5 activation binds the verifier's complete, app-owned runtime pipeline
// digest. Older rows cannot prove that binding and are invalidated fail-closed.
const MATCH_SCHEMA_V4_TO_V5_SQL: &str = r#"
BEGIN TRANSACTION;
DELETE match_calibration_activation;
DEFINE FIELD OVERWRITE runtime_configuration_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE activation_integrity_digest ON TABLE match_calibration_activation TYPE string;
DEFINE FIELD OVERWRITE trusted_index_build_digest ON TABLE match_calibration_activation TYPE string;
DEFINE TABLE OVERWRITE match_trusted_index_build SCHEMAFULL;
DEFINE FIELD OVERWRITE receipt_id ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE build_digest ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE source_row_count ON TABLE match_trusted_index_build TYPE int;
DEFINE FIELD OVERWRITE engine_version ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE build_seed ON TABLE match_trusted_index_build TYPE int;
DEFINE FIELD OVERWRITE build_order ON TABLE match_trusted_index_build TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_trusted_index_build TYPE string;
DEFINE INDEX OVERWRITE match_trusted_index_build_receipt ON TABLE match_trusted_index_build FIELDS receipt_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 adds operator-owned catalog presentation state and explicit opt-in
// indexing-root configuration without changing derived identity generations.
const MATCH_SCHEMA_V5_TO_V6_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE cover_media_key ON TABLE match_person TYPE option<string>;
DEFINE FIELD OVERWRITE hidden ON TABLE match_person TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE favorite ON TABLE match_person TYPE bool DEFAULT false;
UPDATE match_person SET hidden = hidden ?? false, favorite = favorite ?? false;
DEFINE TABLE OVERWRITE match_index_root SCHEMAFULL;
DEFINE FIELD OVERWRITE root_id ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE path ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE exclusions ON TABLE match_index_root TYPE array<string>;
DEFINE FIELD OVERWRITE enabled ON TABLE match_index_root TYPE bool;
DEFINE FIELD OVERWRITE created_at ON TABLE match_index_root TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_index_root TYPE string;
DEFINE INDEX OVERWRITE match_index_root_id ON TABLE match_index_root FIELDS root_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 distinguishes unsupported/skipped media from processing failures so
// job progress and retry affordances remain actionable after restart.
const MATCH_SCHEMA_V6_TO_V7_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE skipped ON TABLE match_index_job TYPE int DEFAULT 0;
UPDATE match_index_job SET skipped = skipped ?? 0;
DEFINE FIELD OVERWRITE skipped_code ON TABLE match_job_asset TYPE option<string>;
DEFINE FIELD OVERWRITE skipped_message ON TABLE match_job_asset TYPE option<string>;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 stores the exact source path separately from the stable media key.
// The key remains safe for identity joins while the operator-only gallery can
// open a path without corrupting case or root information.
const MATCH_SCHEMA_V7_TO_V8_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE source_path ON TABLE match_job_asset TYPE option<string>;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 persists failures that occur before an asset exists (model load,
// root validation, walk, hashing, and worker spawn) on the durable job row.
const MATCH_SCHEMA_V8_TO_V9_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE failure_code ON TABLE match_index_job TYPE option<string>;
DEFINE FIELD OVERWRITE failure_message ON TABLE match_index_job TYPE option<string>;
UPDATE match_job_asset SET next_stage = 'detect', completed_stages = ['discover'] WHERE next_stage IN ['align', 'embed'];
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 denormalizes the stable media key onto assignments so Person gallery
// paging can use a composite index without materializing every face ID first.
const MATCH_SCHEMA_V9_TO_V10_PREPARE_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE media_key ON TABLE match_assignment TYPE option<string>;
UPDATE match_assignment SET media_key = type::record('match_face_observation', face_id).media_key WHERE media_key = NONE;
COMMIT TRANSACTION;
"#;

const MATCH_SCHEMA_V9_TO_V10_FINALIZE_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE media_key ON TABLE match_assignment TYPE string;
DEFINE INDEX OVERWRITE match_assignment_person ON TABLE match_assignment FIELDS person_id, media_key;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-083 resolves only the bounded gallery/cover media-key page against job
// assets. The leading media-key index prevents that second-stage lookup from
// scanning unrelated historical jobs.
const MATCH_SCHEMA_V10_TO_V11_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE INDEX OVERWRITE match_job_asset_media ON TABLE match_job_asset FIELDS media_key, updated_at, asset_id;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-084 binds manual corrections to the exact EXIF-oriented dimensions the
// operator used. Existing derived observations remain valid with absent
// geometry provenance; newly persisted observations always populate it.
const MATCH_SCHEMA_V12_TO_V13_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE FIELD OVERWRITE source_width ON TABLE match_face_observation TYPE option<int>;
DEFINE FIELD OVERWRITE source_height ON TABLE match_face_observation TYPE option<int>;
DEFINE FIELD OVERWRITE exif_orientation ON TABLE match_face_observation TYPE option<int>;
DEFINE INDEX OVERWRITE match_assignment_person_inventory ON TABLE match_assignment FIELDS person_id, assignment_id;
DEFINE TABLE OVERWRITE match_correction_media_operation SCHEMAFULL;
DEFINE FIELD OVERWRITE mapping_id ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE media_key ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE operation_id ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE kind ON TABLE match_correction_media_operation TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_correction_media_operation TYPE string;
DEFINE INDEX OVERWRITE match_correction_media_operation_id ON TABLE match_correction_media_operation FIELDS mapping_id UNIQUE;
DEFINE INDEX OVERWRITE match_correction_media_operation_media ON TABLE match_correction_media_operation FIELDS media_key, created_at, operation_id;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-084 Person-edit previews traverse suggestions in stable, bounded pages.
// Candidate-leading indexes keep those exact scans proportional to the edited
// Person's inventory instead of repeatedly scanning unrelated suggestions.
const MATCH_SCHEMA_V13_TO_V14_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE INDEX OVERWRITE match_suggestion_person_inventory ON TABLE match_suggestion FIELDS candidate_person_id, suggestion_id;
DEFINE INDEX OVERWRITE match_suggestion_person_face_inventory ON TABLE match_suggestion FIELDS candidate_person_id, face_id;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-085 legacy media-history backfill must never scan the complete operation
// table. Direct histories are located by kind/FaceId while batch Not-sure
// envelopes are walked in deterministic operation-id pages.
const MATCH_SCHEMA_V14_TO_V15_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE INDEX OVERWRITE match_operation_kind_face ON TABLE match_operation FIELDS kind, face_id, operation_id;
DEFINE INDEX OVERWRITE match_operation_kind_operation ON TABLE match_operation FIELDS kind, operation_id;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-085 records an app-owned recovery bundle in the same transaction as the
// first portable import mutation. A surviving singleton is recovered before
// normal trusted-search reconciliation during the next MatchStore open.
const MATCH_SCHEMA_V15_TO_V16_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE TABLE OVERWRITE match_identity_import_journal SCHEMAFULL;
DEFINE FIELD OVERWRITE journal_id ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE version ON TABLE match_identity_import_journal TYPE int;
DEFINE FIELD OVERWRITE plan_token ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE pre_state_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_path ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_file_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE recovery_content_sha256 ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE relocations_json ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE phase ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE created_at ON TABLE match_identity_import_journal TYPE string;
DEFINE FIELD OVERWRITE updated_at ON TABLE match_identity_import_journal TYPE string;
DEFINE INDEX OVERWRITE match_identity_import_journal_id ON TABLE match_identity_import_journal FIELDS journal_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// Face-leading probes bound each correlated Person-preview existence lookup.
const MATCH_SCHEMA_V18_TO_V19_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE INDEX OVERWRITE match_trusted_member_face ON TABLE match_trusted_member FIELDS face_id;
DEFINE INDEX OVERWRITE match_trusted_search_face_person ON TABLE match_trusted_search_embedding FIELDS face_id, person_id;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-085 rebuild closure is staged in bounded database pages instead of
// retaining every known/durable FaceId in process memory.
const MATCH_SCHEMA_V16_TO_V17_SQL: &str = r#"
BEGIN TRANSACTION;
DEFINE TABLE OVERWRITE match_recovery_face_closure SCHEMAFULL;
DEFINE FIELD OVERWRITE closure_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE FIELD OVERWRITE run_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE FIELD OVERWRITE face_id ON TABLE match_recovery_face_closure TYPE string;
DEFINE INDEX OVERWRITE match_recovery_face_closure_id ON TABLE match_recovery_face_closure FIELDS closure_id UNIQUE;
DEFINE INDEX OVERWRITE match_recovery_face_closure_run ON TABLE match_recovery_face_closure FIELDS run_id, closure_id;
DEFINE INDEX OVERWRITE match_recovery_face_closure_run_face ON TABLE match_recovery_face_closure FIELDS run_id, face_id UNIQUE;
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

// WP-085 persists independently anchored suggestion-source metadata so portable
// correction history cannot authenticate provenance solely from its own delta.
const MATCH_SCHEMA_V17_TO_V18_PREPARE_SQL: &str = r#"
BEGIN TRANSACTION;
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
COMMIT TRANSACTION;
"#;

const MATCH_SCHEMA_V17_TO_V18_BACKFILL_PAGE_SQL: &str = r#"
BEGIN TRANSACTION;
FOR $mapping IN $direct_media_backfills {
    UPSERT type::record('match_correction_media_operation', $mapping.mapping_id) CONTENT $mapping;
};
COMMIT TRANSACTION;
"#;

const MATCH_SCHEMA_V17_TO_V18_SQL: &str = r#"
BEGIN TRANSACTION;
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
FOR $mapping IN $direct_media_backfills {
    UPSERT type::record('match_correction_media_operation', $mapping.mapping_id) CONTENT $mapping;
};
UPDATE match_schema_state:global SET schema_version = $schema_version, updated_at = $updated_at;
COMMIT TRANSACTION;
"#;

const MATCH_SCHEMA_MARKER_BOOTSTRAP_SQL: &str = r#"
DEFINE TABLE IF NOT EXISTS match_schema_state SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS schema_version ON TABLE match_schema_state TYPE int;
DEFINE FIELD IF NOT EXISTS engine_version ON TABLE match_schema_state TYPE string;
DEFINE FIELD IF NOT EXISTS schema_generation ON TABLE match_schema_state TYPE string;
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE match_schema_state TYPE string;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wp086_external_holds_interleave_without_changing_operator_mode() {
        let root = workspace("wp086-external-holds");
        let holds = Arc::new(MatchExternalHolds::default());
        let store = MatchStore::open(&root)
            .unwrap()
            .with_external_holds(Arc::clone(&holds));
        store.set_desired_mode(DesiredMode::Running).unwrap();
        let initial = holds.snapshot();
        holds.set_playback(true);
        holds.set_fullscreen(true);
        let both = holds.snapshot();
        assert_ne!(initial, both);
        assert_eq!(
            holds.set_fullscreen(true),
            both,
            "same-state publications must coalesce"
        );
        assert!(!store.can_admit(JobLifecycle::Running).unwrap());
        assert!(!store.can_attempt_automatic(JobLifecycle::Running).unwrap());
        holds.set_playback(false);
        assert_eq!(store.holds().unwrap(), vec!["immersive_fullscreen"]);
        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        holds.set_fullscreen(false);
        assert!(!store.can_attempt_automatic(JobLifecycle::Running).unwrap());
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        store.set_desired_mode(DesiredMode::Running).unwrap();
        store.add_hold(HoldReason::ResourcePressure).unwrap();
        holds.set_playback(true);
        holds.set_fullscreen(true);
        holds.set_fullscreen(false);
        assert!(holds.playback());
        holds.set_playback(false);
        assert!(store.can_attempt_automatic(JobLifecycle::Running).unwrap());
        assert!(!store.can_admit(JobLifecycle::Running).unwrap());
        store.remove_hold(HoldReason::ResourcePressure).unwrap();
        assert!(store.can_admit(JobLifecycle::Running).unwrap());
        close(&root, store);
    }
    use crate::media_io::{MediaIoCoordinator, RootKind};
    use std::path::PathBuf;

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial-wp081-{name}-{}",
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

    #[test]
    fn correction_media_fingerprint_is_raw_lowercase_sha256_only() {
        let digest = "a".repeat(64);
        assert_eq!(
            canonical_correction_media_fingerprint(&format!("sha256:{digest}")).unwrap(),
            digest
        );
        assert!(canonical_correction_media_fingerprint(&"A".repeat(64)).is_err());
        assert!(canonical_correction_media_fingerprint("sha256:not-a-digest").is_err());
    }

    #[test]
    fn fused_face_retry_rejects_geometry_orientation_and_timestamp_mutation() {
        let mut persisted =
            derived_face("retry/fused.jpg", &format!("sha256:{}", "a".repeat(64)), 0);
        persisted.source_width = Some(1920);
        persisted.source_height = Some(1080);
        persisted.exif_orientation = Some(1);
        persisted.created_at = "2026-08-26T10:00:00Z".to_string();
        persisted.updated_at = persisted.created_at.clone();
        assert!(same_face_payload(&persisted, &persisted));

        let mut changed_width = persisted.clone();
        changed_width.source_width = Some(1919);
        assert!(!same_face_payload(&persisted, &changed_width));

        let mut changed_height = persisted.clone();
        changed_height.source_height = Some(1079);
        assert!(!same_face_payload(&persisted, &changed_height));

        let mut changed_orientation = persisted.clone();
        changed_orientation.exif_orientation = Some(6);
        assert!(!same_face_payload(&persisted, &changed_orientation));

        let mut changed_created_at = persisted.clone();
        changed_created_at.created_at = "2026-08-26T10:00:01Z".to_string();
        assert!(!same_face_payload(&persisted, &changed_created_at));

        let mut changed_updated_at = persisted.clone();
        changed_updated_at.updated_at = "2026-08-26T10:00:01Z".to_string();
        assert!(!same_face_payload(&persisted, &changed_updated_at));
    }

    #[test]
    fn fused_publication_rejects_each_internal_duplicate_identity_class() {
        let first = derived_face(
            "fused/duplicate-a.jpg",
            &format!("sha256:{}", "a".repeat(64)),
            0,
        );
        let mut duplicate_face_id = derived_face(
            "fused/duplicate-b.jpg",
            &format!("sha256:{}", "b".repeat(64)),
            1,
        );
        duplicate_face_id.face_id = first.face_id.clone();
        let first_embedding = FaceEmbedding {
            embedding_id: embedding_id(&first.face_id, "fused-duplicate-model"),
            face_id: first.face_id.clone(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: "fused-duplicate-model".to_string(),
            schema_generation: first.schema_generation.clone(),
            media_fingerprint: first.media_fingerprint.clone(),
            face_revision: first.face_revision,
            job_id: "fused-duplicate-job".to_string(),
            active: true,
            created_at: now(),
        };
        let second_embedding = FaceEmbedding {
            embedding_id: embedding_id(&duplicate_face_id.face_id, "fused-duplicate-model"),
            face_id: duplicate_face_id.face_id.clone(),
            media_fingerprint: duplicate_face_id.media_fingerprint.clone(),
            ..first_embedding.clone()
        };
        assert!(validate_fused_publication_uniqueness(
            &[first.clone(), duplicate_face_id],
            &[first_embedding.clone(), second_embedding]
        )
        .unwrap_err()
        .contains("duplicate canonical face_id"));

        let mut duplicate_source_slot = first.clone();
        duplicate_source_slot.face_id = "distinct-caller-face-id".to_string();
        assert!(validate_fused_publication_uniqueness(
            &[first.clone(), duplicate_source_slot.clone()],
            &[]
        )
        .unwrap_err()
        .contains("duplicate media-identity/source-index slot"));

        let mut duplicate_embedding_id = FaceEmbedding {
            embedding_id: embedding_id(&duplicate_source_slot.face_id, "fused-duplicate-model"),
            face_id: duplicate_source_slot.face_id,
            media_fingerprint: duplicate_source_slot.media_fingerprint,
            ..first_embedding.clone()
        };
        duplicate_embedding_id.embedding_id = first_embedding.embedding_id.clone();
        assert!(validate_fused_publication_uniqueness(
            &[first],
            &[first_embedding, duplicate_embedding_id]
        )
        .unwrap_err()
        .contains("duplicate embedding_id"));
    }

    #[test]
    fn legacy_direct_media_backfill_covers_every_direct_kind_and_fails_without_hash_evidence() {
        let timestamp = "2026-08-26T10:00:00Z".to_string();
        let face_id = "legacy-direct-face";
        let media_key = "legacy/direct.jpg";
        let digest = "b".repeat(64);
        let mut observation = face(face_id, media_key, &format!("sha256:{digest}"), true);
        observation.created_at = timestamp.clone();
        observation.updated_at = timestamp.clone();
        let assignment_for = |operation_id: &str| Assignment {
            assignment_id: face_id.to_string(),
            face_id: face_id.to_string(),
            person_id: "legacy-person".to_string(),
            media_key: media_key.to_string(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "legacy-direct-backfill-test".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: 1,
            person_revision: 1,
            operation_id: operation_id.to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let operation = |operation_id: &str,
                         kind: &str,
                         before_json: String,
                         after_json: String,
                         reversible: bool| MatchOperation {
            operation_id: operation_id.to_string(),
            kind: kind.to_string(),
            face_id: Some(face_id.to_string()),
            person_id: Some("legacy-person".to_string()),
            before_json,
            after_json,
            reversible,
            created_at: timestamp.clone(),
        };
        let assign_operator = assignment_for("legacy-assign-operator");
        let assign_strict = assignment_for("legacy-assign-strict");
        let different_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(face_id, "legacy-person"),
            face_id: face_id.to_string(),
            person_id: "legacy-person".to_string(),
            operation_id: "legacy-different".to_string(),
            operator_owned: true,
            created_at: timestamp.clone(),
        };
        let move_before = assignment_for("legacy-prior-owner");
        let mut move_after = move_before.clone();
        move_after.operation_id = "legacy-move".to_string();
        move_after.look_id = Some("legacy-look".to_string());
        move_after.placement = "look".to_string();
        let operations = vec![
            operation(
                "legacy-assign-operator",
                "assign_operator_confirmed",
                "null".to_string(),
                serde_json::to_string(&assign_operator).unwrap(),
                true,
            ),
            operation(
                "legacy-assign-strict",
                "assign_committed_strict_automatic",
                "null".to_string(),
                serde_json::to_string(&assign_strict).unwrap(),
                true,
            ),
            operation(
                "legacy-different",
                "different",
                "null".to_string(),
                serde_json::to_string(&different_constraint).unwrap(),
                true,
            ),
            operation(
                "legacy-not-sure",
                "not_sure",
                serde_json::to_string(&(
                    Option::<Assignment>::None,
                    Option::<CannotLinkConstraint>::None,
                ))
                .unwrap(),
                "null".to_string(),
                false,
            ),
            operation(
                "legacy-move",
                "move_to_look",
                serde_json::to_string(&move_before).unwrap(),
                serde_json::to_string(&move_after).unwrap(),
                true,
            ),
        ];
        let faces = BTreeMap::from([(face_id.to_string(), observation.clone())]);
        let backfills = derive_legacy_direct_media_backfills(
            &operations,
            &faces,
            &LegacyDirectMediaEvidence::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(backfills.len(), LEGACY_DIRECT_MEDIA_KINDS.len());
        assert!(backfills.iter().all(|mapping| {
            mapping.media_key == media_key && mapping.media_fingerprint == digest
        }));

        let missing_evidence = derive_legacy_direct_media_backfills(
            &operations[2..3],
            &BTreeMap::new(),
            &LegacyDirectMediaEvidence::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(missing_evidence.contains("exact current/historical evidence is required"));

        let mut historical = LegacyDirectMediaEvidence::default();
        historical
            .bind_face(&observation, "test historical Face snapshot")
            .unwrap();
        let historical_only = derive_legacy_direct_media_backfills(
            &operations[2..3],
            &BTreeMap::new(),
            &historical,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(historical_only[0].media_key, media_key);
        assert_eq!(historical_only[0].media_fingerprint, digest);

        let mut ambiguous = historical;
        let mut conflicting_snapshot = observation;
        conflicting_snapshot.media_fingerprint = format!("sha256:{}", "e".repeat(64));
        ambiguous
            .bind_face(
                &conflicting_snapshot,
                "conflicting historical Face snapshot",
            )
            .unwrap();
        let ambiguity = derive_legacy_direct_media_backfills(
            &operations[2..3],
            &BTreeMap::new(),
            &ambiguous,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(ambiguity.contains("2 canonical fingerprint candidates"));
    }

    #[test]
    fn legacy_direct_media_backfill_has_bounded_pages_not_a_lifetime_row_ceiling() {
        let timestamp = now();
        let face_id = "legacy-many-direct-face";
        let media_key = "legacy/many-direct.jpg";
        let digest = "d".repeat(64);
        let observation = face(face_id, media_key, &format!("sha256:{digest}"), true);
        let faces = BTreeMap::from([(face_id.to_string(), observation)]);
        let operations = (0..(4096 + 1))
            .map(|index| {
                let operation_id = format!("legacy-many-direct-{index:05}");
                let assignment = Assignment {
                    assignment_id: face_id.to_string(),
                    face_id: face_id.to_string(),
                    person_id: "legacy-many-person".to_string(),
                    media_key: media_key.to_string(),
                    look_id: None,
                    placement: "unsorted".to_string(),
                    state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                    provenance: "legacy-page-test".to_string(),
                    locked: true,
                    model_generation: None,
                    calibration_generation: None,
                    envelope_hash: None,
                    face_revision: 1,
                    person_revision: 1,
                    operation_id: operation_id.clone(),
                    created_at: timestamp.clone(),
                    updated_at: timestamp.clone(),
                };
                MatchOperation {
                    operation_id,
                    kind: "assign_operator_confirmed".to_string(),
                    face_id: Some(face_id.to_string()),
                    person_id: Some("legacy-many-person".to_string()),
                    before_json: "null".to_string(),
                    after_json: serde_json::to_string(&assignment).unwrap(),
                    reversible: true,
                    created_at: timestamp.clone(),
                }
            })
            .collect::<Vec<_>>();
        let backfills = derive_legacy_direct_media_backfills(
            &operations,
            &faces,
            &LegacyDirectMediaEvidence::default(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(backfills.len(), 4097);
        assert_eq!(backfills.first().unwrap().media_fingerprint, digest);
        assert_eq!(backfills.last().unwrap().media_fingerprint, digest);
    }

    #[test]
    fn fresh_schema_defines_correction_media_operation_query_surface() {
        assert!(MATCH_SCHEMA_SQL
            .contains("DEFINE TABLE OVERWRITE match_correction_media_operation SCHEMAFULL;"));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_correction_media_operation TYPE string;"
        ));
        assert!(MATCH_SCHEMA_V12_TO_V13_SQL.contains(
            "DEFINE FIELD OVERWRITE media_fingerprint ON TABLE match_correction_media_operation TYPE string;"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_correction_media_operation_media ON TABLE match_correction_media_operation"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_assignment_person_inventory ON TABLE match_assignment"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_suggestion_person_inventory ON TABLE match_suggestion FIELDS candidate_person_id, suggestion_id"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_suggestion_person_face_inventory ON TABLE match_suggestion FIELDS candidate_person_id, face_id"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_operation_kind_face ON TABLE match_operation FIELDS kind, face_id, operation_id"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_operation_kind_operation ON TABLE match_operation FIELDS kind, operation_id"
        ));
        assert!(MATCH_SCHEMA_V14_TO_V15_SQL.contains(
            "DEFINE INDEX OVERWRITE match_operation_kind_face ON TABLE match_operation FIELDS kind, face_id, operation_id"
        ));
        assert!(MATCH_SCHEMA_V14_TO_V15_SQL.contains(
            "DEFINE INDEX OVERWRITE match_operation_kind_operation ON TABLE match_operation FIELDS kind, operation_id"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_recovery_face_closure_run_face ON TABLE match_recovery_face_closure FIELDS run_id, face_id UNIQUE"
        ));
        assert!(MATCH_SCHEMA_V16_TO_V17_SQL.contains(
            "DEFINE INDEX OVERWRITE match_recovery_face_closure_run_face ON TABLE match_recovery_face_closure FIELDS run_id, face_id UNIQUE"
        ));
        assert!(MATCH_SCHEMA_SQL.contains(
            "DEFINE INDEX OVERWRITE match_suggestion_source_provenance_operation ON TABLE match_suggestion_source_provenance FIELDS operation_id, suggestion_id UNIQUE"
        ));
        assert!(MATCH_SCHEMA_V17_TO_V18_SQL.contains(
            "DEFINE INDEX OVERWRITE match_suggestion_source_provenance_operation ON TABLE match_suggestion_source_provenance FIELDS operation_id, suggestion_id UNIQUE"
        ));
        let root = workspace("fresh-correction-media-operation-schema");
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(store.count("match_correction_media_operation").unwrap(), 0);
        assert_eq!(store.count("match_identity_import_journal").unwrap(), 0);
        assert_eq!(store.count("match_recovery_face_closure").unwrap(), 0);
        assert_eq!(
            store.count("match_suggestion_source_provenance").unwrap(),
            0
        );
        close(&root, store);
    }

    #[test]
    fn schema_v18_upgrade_backfills_persisted_v17_direct_media_mapping() {
        let root = workspace("wp085-v18-direct-media-backfill");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Legacy Direct Person", Vec::new())
            .unwrap();
        let digest = "c".repeat(64);
        let observation = store
            .create_face(face(
                "legacy-v17-face",
                "legacy/v17-direct.jpg",
                &digest,
                true,
            ))
            .unwrap();
        let operator_fence = store
            .operator_mutation_fence(&observation.face_id, &person.person_id)
            .unwrap();
        let assignment = store
            .review_same(&observation.face_id, &person.person_id, &operator_fence)
            .unwrap();
        let mapping_id = corrections::correction_media_mapping_id(
            &assignment.operation_id,
            &observation.media_key,
        );
        let delete_fence = store.correction_fence(&observation.face_id).unwrap();
        let delete_receipt = store
            .delete_face_analysis(&observation.face_id, &delete_fence)
            .unwrap();
        assert_eq!(delete_receipt.kind, "delete_face_analysis");
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, &observation.face_id)
            .unwrap()
            .is_none());
        let db = store.store.db();
        let mapping_id_for_delete = mapping_id.clone();
        surreal_store::run(async move {
            db.query(
                "BEGIN TRANSACTION; \
                 DELETE type::record('match_correction_media_operation', $mapping_id); \
                 UPDATE match_schema_state:global SET schema_version = 17; \
                 COMMIT TRANSACTION;",
            )
            .bind(("mapping_id", mapping_id_for_delete))
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let mapping: corrections::CorrectionMediaOperation = reopened
            .require(
                corrections::CORRECTION_MEDIA_OPERATION_TABLE,
                &mapping_id,
                "v18 direct-media backfill",
            )
            .unwrap();
        assert_eq!(mapping.operation_id, assignment.operation_id);
        assert_eq!(mapping.media_key, observation.media_key);
        assert_eq!(mapping.media_fingerprint, digest);
        let schema_state = reopened
            .get_one::<Value>("match_schema_state", "global")
            .unwrap()
            .unwrap();
        assert_eq!(
            schema_state.get("schema_version").and_then(Value::as_u64),
            Some(MATCH_SCHEMA_VERSION)
        );
        close(&root, reopened);
    }

    #[test]
    fn canonical_embedding_retry_contract_is_shared_by_fused_and_standalone_writes() {
        let existing = FaceEmbedding {
            embedding_id: "embedding-retry-contract".to_string(),
            face_id: "face-retry-contract".to_string(),
            vector: vec![0.25; EMBEDDING_DIM],
            model_generation: "model-retry-contract".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: "sha256:retry-contract".to_string(),
            face_revision: 7,
            job_id: "job-retry-a".to_string(),
            active: true,
            created_at: "2026-08-26T10:00:00Z".to_string(),
        };
        assert_eq!(
            canonical_embedding_retry_disposition(&existing, &existing),
            Ok(EmbeddingRetryDisposition::Exact)
        );

        let mut changed_timestamp = existing.clone();
        changed_timestamp.created_at = "2026-08-26T10:00:01Z".to_string();
        assert_eq!(
            canonical_embedding_retry_disposition(&existing, &changed_timestamp),
            Err(())
        );

        let mut refreshed_job = changed_timestamp;
        refreshed_job.job_id = "job-retry-b".to_string();
        assert_eq!(
            canonical_embedding_retry_disposition(&existing, &refreshed_job),
            Ok(EmbeddingRetryDisposition::RefreshCurrentJob)
        );
    }

    #[test]
    fn canonical_embedding_retry_refreshes_current_job_provenance() {
        let root = workspace("embedding-job-provenance-refresh");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("model-refresh", true)
            .unwrap();
        store.activate_model_generation("model-refresh").unwrap();

        let first_job = create_running_job(&store, "root-refresh-a", "model-refresh");
        let first_asset = store
            .enqueue_asset(&first_job.job_id, "media/refresh.jpg", "sha256:refresh")
            .unwrap();
        let first_fence = fence(&first_job, &first_asset);
        let detect = stage_permit(&store, &first_fence, JobStage::Detect);
        let observed = store
            .create_derived_face(
                derived_face("media/refresh.jpg", "sha256:refresh", 0),
                &first_fence,
                &detect,
            )
            .unwrap();
        let first_permit = stage_permit(&store, &first_fence, JobStage::Embed);
        let first = embedding(&observed, "model-refresh", &first_job);
        store
            .put_embedding(first.clone(), &first_fence, &first_permit)
            .unwrap();

        let second_job = create_running_job(&store, "root-refresh-b", "model-refresh");
        let second_asset = store
            .enqueue_asset(&second_job.job_id, "media/refresh.jpg", "sha256:refresh")
            .unwrap();
        let second_fence = fence(&second_job, &second_asset);
        let second_permit = stage_permit(&store, &second_fence, JobStage::Embed);
        let mut refreshed = first.clone();
        refreshed.job_id = second_job.job_id.clone();
        refreshed.created_at = now();
        store
            .put_embedding(refreshed.clone(), &second_fence, &second_permit)
            .unwrap();
        let stored: FaceEmbedding = store
            .require(
                EMBEDDING_TABLE,
                &refreshed.embedding_id,
                "refreshed embedding",
            )
            .unwrap();
        assert_eq!(stored.job_id, second_job.job_id);
        assert_eq!(stored.created_at, refreshed.created_at);

        let retry_permit = raw_stage_permit(&store, &second_fence, JobStage::Embed);
        let mut forged_retry = refreshed;
        forged_retry.created_at.push_str("-forged");
        assert!(store
            .put_embedding(forged_retry, &second_fence, &retry_permit)
            .unwrap_err()
            .contains("timestamp is not RFC3339"));
        close(&root, store);
    }

    #[test]
    fn embedding_provenance_validator_rejects_wrong_job_and_time() {
        let timestamp = now();
        let job = IndexJob {
            job_id: "job-provenance".to_string(),
            root_key: "root-provenance".to_string(),
            lifecycle: JobLifecycle::Running.as_str().to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: "model-provenance".to_string(),
            identity_revision: 1,
            catalog_revision: 1,
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
            asset_id: job_asset_id(&job.job_id, "media/provenance.jpg"),
            job_id: job.job_id.clone(),
            media_key: "media/provenance.jpg".to_string(),
            source_path: None,
            media_fingerprint: "sha256:provenance".to_string(),
            next_stage: JobStage::Embed.as_str().to_string(),
            completed_stages: vec![
                "discover".to_string(),
                "detect".to_string(),
                "align".to_string(),
            ],
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: job.schema_generation.clone(),
            model_generation: job.model_generation.clone(),
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
            updated_at: timestamp.clone(),
        };
        let fence = fence(&job, &asset);
        let observed = derived_face(&asset.media_key, &asset.media_fingerprint, 0);
        let mut value = embedding(&observed, &job.model_generation, &job);
        assert!(validate_embedding_job_provenance(&value, &observed, &job, &asset, &fence).is_ok());
        value.job_id = "other-job".to_string();
        assert!(
            validate_embedding_job_provenance(&value, &observed, &job, &asset, &fence)
                .unwrap_err()
                .contains("canonical job/face fence")
        );
        value.job_id = job.job_id.clone();
        value.created_at = "not-a-time".to_string();
        assert!(
            validate_embedding_job_provenance(&value, &observed, &job, &asset, &fence)
                .unwrap_err()
                .contains("not RFC3339")
        );
        value.created_at = "2999-01-01T00:00:00Z".to_string();
        assert!(
            validate_embedding_job_provenance(&value, &observed, &job, &asset, &fence)
                .unwrap_err()
                .contains("outside its canonical job lifetime")
        );
    }

    fn face(id: &str, key: &str, fingerprint: &str, operator_owned: bool) -> FaceObservation {
        FaceObservation {
            face_id: id.to_string(),
            media_key: key.to_string(),
            media_fingerprint: fingerprint.to_string(),
            source_index: 0,
            source_width: None,
            source_height: None,
            exif_orientation: None,
            bounds_normalized: vec![0.1, 0.2, 0.3, 0.4],
            landmarks_normalized: vec![vec![0.2, 0.3], vec![0.4, 0.3]],
            alignment_valid: true,
            quality: 0.95,
            pose_bucket: "frontal".to_string(),
            operator_owned,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: now(),
            updated_at: now(),
        }
    }

    fn derived_face(key: &str, fingerprint: &str, source_index: u32) -> FaceObservation {
        let mut observation = face("placeholder", key, fingerprint, false);
        observation.source_index = source_index;
        observation.face_id =
            derived_face_id(key, fingerprint, source_index, MATCH_SCHEMA_GENERATION);
        observation
    }

    pub(super) fn embedding(
        face: &FaceObservation,
        generation: &str,
        job: &IndexJob,
    ) -> FaceEmbedding {
        FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, generation),
            face_id: face.face_id.clone(),
            vector: vector(0, 0.1),
            model_generation: generation.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: job.job_id.clone(),
            active: true,
            created_at: now(),
        }
    }

    fn vector(axis: usize, secondary: f32) -> Vec<f32> {
        let mut value = vec![0.0; EMBEDDING_DIM];
        value[axis] = 1.0;
        value[(axis + 1) % EMBEDDING_DIM] = secondary;
        value
    }

    fn fence(job: &IndexJob, asset: &JobAsset) -> RevisionFence {
        RevisionFence {
            job_id: job.job_id.clone(),
            media_key: asset.media_key.clone(),
            media_fingerprint: asset.media_fingerprint.clone(),
            schema_generation: job.schema_generation.clone(),
            model_generation: job.model_generation.clone(),
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
        }
    }

    fn stage_request(stage: JobStage) -> ResourceRequest {
        ResourceRequest {
            worker_memory_bytes: 0,
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 1024 * 1024,
            cpu_inference: u64::from(matches!(
                stage,
                JobStage::Detect | JobStage::Align | JobStage::Embed | JobStage::Suggest
            )),
            decoded_bytes: if matches!(stage, JobStage::Detect | JobStage::Align) {
                8 * 1024 * 1024
            } else {
                0
            },
            gpu_vram_bytes: 0,
            surreal_writes: 1,
            vector_index_builds: 0,
        }
    }

    fn raw_stage_permit(
        store: &MatchStore,
        revision_fence: &RevisionFence,
        stage: JobStage,
    ) -> MatchStagePermit {
        store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("match-test", 1, RootKind::Local),
                revision_fence,
                stage,
                stage_request(stage),
            )
            .unwrap()
    }

    fn create_running_job(store: &MatchStore, root_key: &str, model_generation: &str) -> IndexJob {
        let job = store.create_job(root_key, model_generation).unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap()
    }

    fn advance_to(
        store: &MatchStore,
        revision_fence: &RevisionFence,
        asset_id: &str,
        target: JobStage,
    ) {
        loop {
            let asset: JobAsset = store
                .require(JOB_ASSET_TABLE, asset_id, "JobAsset")
                .unwrap();
            let next = asset.next_stage().unwrap();
            if next == target {
                return;
            }
            assert_ne!(next, JobStage::Complete, "cannot advance beyond Complete");
            let permit = raw_stage_permit(store, revision_fence, next);
            store
                .commit_asset_stage(asset_id, next, revision_fence, &permit)
                .unwrap();
        }
    }

    fn stage_permit(
        store: &MatchStore,
        revision_fence: &RevisionFence,
        stage: JobStage,
    ) -> MatchStagePermit {
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        advance_to(store, revision_fence, &asset_id, stage);
        raw_stage_permit(store, revision_fence, stage)
    }

    pub(super) fn activate_calibration(
        store: &MatchStore,
        model_generation: &str,
        calibration_generation: &str,
        envelope_hash: &str,
    ) {
        let gallery_members_digest = store.trusted_gallery_members_digest().unwrap();
        let trusted_index_build: TrustedIndexBuildReceipt = store
            .require(
                TRUSTED_INDEX_BUILD_TABLE,
                "global",
                "trusted index build receipt",
            )
            .unwrap();
        let timestamp = now();
        let mut activation = CalibrationActivation {
            calibration_generation: calibration_generation.to_string(),
            model_generation: model_generation.to_string(),
            envelope_hash: envelope_hash.to_string(),
            runtime_configuration_digest: strict_runtime_configuration_digest(100, 50),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: trusted_index_build.build_digest,
            gallery_members_digest,
            verifier_artifact_id: "VAL-WP-082-TEST".to_string(),
            contract_sha256: "a".repeat(64),
            raw_records_sha256: "b".repeat(64),
            evidence_digest: "c".repeat(64),
            review_digest: "d".repeat(64),
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
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        activation.activation_integrity_digest =
            calibration_activation_integrity_digest(&activation);
        store
            .upsert_json(CALIBRATION_TABLE, calibration_generation, &activation)
            .unwrap();
    }

    #[test]
    fn activation_integrity_binds_thresholds_and_readiness_state() {
        let timestamp = now();
        let mut activation = CalibrationActivation {
            calibration_generation: "calibration-integrity".to_string(),
            model_generation: "model-integrity".to_string(),
            envelope_hash: "a".repeat(64),
            runtime_configuration_digest: strict_runtime_configuration_digest(100, 50),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: "9".repeat(64),
            gallery_members_digest: "b".repeat(64),
            verifier_artifact_id: "VAL-WP-082-TEST".to_string(),
            contract_sha256: "c".repeat(64),
            raw_records_sha256: "d".repeat(64),
            evidence_digest: "e".repeat(64),
            review_digest: "f".repeat(64),
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
            wp084_runtime_ready: false,
            wp087_release_ready: false,
            active: false,
            invalidation_reason: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let original = calibration_activation_integrity_digest(&activation);
        activation.automatic_threshold = 0.10;
        assert_ne!(
            original,
            calibration_activation_integrity_digest(&activation)
        );
        activation.automatic_threshold = 0.90;
        activation.wp084_runtime_ready = true;
        assert_ne!(
            original,
            calibration_activation_integrity_digest(&activation)
        );
    }

    #[test]
    fn trusted_index_build_receipt_is_order_independent_and_content_bound() {
        let row = |membership_id: &str, value: f32| TrustedSearchEmbedding {
            membership_id: membership_id.to_string(),
            person_id: format!("person-{membership_id}"),
            look_id: format!("look-{membership_id}"),
            face_id: format!("face-{membership_id}"),
            embedding_id: format!("embedding-{membership_id}"),
            vector: vec![value; EMBEDDING_DIM],
            model_generation: "model-build".to_string(),
            created_at: "frozen".to_string(),
        };
        let forward = vec![row("a", 0.1), row("b", 0.2)];
        let reverse = vec![forward[1].clone(), forward[0].clone()];
        assert_eq!(
            trusted_index_build_digest(&forward).unwrap(),
            trusted_index_build_digest(&reverse).unwrap()
        );
        let mut changed = forward.clone();
        changed[0].vector[0] = 0.9;
        assert_ne!(
            trusted_index_build_digest(&forward).unwrap(),
            trusted_index_build_digest(&changed).unwrap()
        );
    }

    fn assert_calibration_inactive(store: &MatchStore, generation: &str, reason: &str) {
        let activation: CalibrationActivation = store
            .require(CALIBRATION_TABLE, generation, "calibration")
            .unwrap();
        assert!(!activation.active);
        assert_eq!(activation.invalidation_reason.as_deref(), Some(reason));
    }

    #[test]
    fn catalog_composition_mutations_invalidate_strict_calibration() {
        let root = workspace("calibration-catalog-invalidation");
        let store = MatchStore::open(&root).unwrap();

        activate_calibration(&store, "model-1", "cal-person", &"e".repeat(64));
        let person = store.create_person("Alice", Vec::new()).unwrap();
        assert_calibration_inactive(&store, "cal-person", "person_created");

        activate_calibration(&store, "model-1", "cal-look", &"e".repeat(64));
        let look = store.create_look(&person.person_id, "Default").unwrap();
        assert_calibration_inactive(&store, "cal-look", "look_created");

        activate_calibration(&store, "model-1", "cal-set", &"e".repeat(64));
        store.create_template_set(&look.look_id, "Trusted").unwrap();
        assert_calibration_inactive(&store, "cal-set", "template_set_created");

        close(&root, store);
    }

    fn poison_match_caches(store: &MatchStore) {
        let caches = Arc::clone(&store.caches);
        assert!(std::thread::spawn(move || {
            let _guard = caches.write().unwrap();
            panic!("poison Match cache for post-commit receipt regression");
        })
        .join()
        .is_err());
    }

    #[test]
    fn create_person_returns_applied_when_post_commit_autocomplete_refresh_fails() {
        let root = workspace("create-person-post-commit-cache-failure");
        let store = MatchStore::open(&root).unwrap();
        poison_match_caches(&store);
        store
            .autocomplete_refresh_failures
            .store(1, Ordering::SeqCst);

        // The poisoned projection is recovered before the canonical write; the
        // injected failure is then consumed only by the eager post-commit
        // refresh. The durable mutation must still return its applied value.
        let person = store
            .create_person("Cache Recovery", vec!["Recovered".to_string()])
            .unwrap();
        let canonical: Person = store
            .require(PERSON_TABLE, &person.person_id, "created Person")
            .unwrap();
        assert_eq!(canonical, person);
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 1);
        assert!(!store.caches.read().unwrap().autocomplete.valid);

        // The first ordinary consumer lazily rebuilds from canonical rows. No
        // duplicate create retry is needed to repair the projection.
        let matches = store
            .autocomplete("recovered", person.catalog_revision, 10)
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].person_id, person.person_id);
        assert!(store.caches.read().unwrap().autocomplete.valid);
        close(&root, store);
    }

    #[test]
    fn person_preferences_return_applied_when_post_commit_autocomplete_refresh_fails() {
        let root = workspace("person-preferences-post-commit-cache-failure");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Preference Recovery", Vec::new())
            .unwrap();
        poison_match_caches(&store);
        store
            .autocomplete_refresh_failures
            .store(1, Ordering::SeqCst);

        let updated = store
            .update_person_preferences(&person.person_id, person.revision, None, true, true)
            .unwrap();
        assert_eq!(updated.revision, person.revision + 1);
        assert!(updated.hidden);
        assert!(updated.favorite);
        let canonical: Person = store
            .require(PERSON_TABLE, &person.person_id, "updated Person")
            .unwrap();
        assert_eq!(canonical, updated);
        assert!(!store.caches.read().unwrap().autocomplete.valid);

        let matches = store
            .autocomplete("preference", updated.catalog_revision, 10)
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].person_id, person.person_id);
        assert!(store.caches.read().unwrap().autocomplete.valid);
        assert_eq!(
            store
                .require::<Person>(PERSON_TABLE, &person.person_id, "updated Person")
                .unwrap()
                .revision,
            person.revision + 1
        );
        close(&root, store);
    }

    #[test]
    fn projection_cache_is_deterministically_bounded() {
        let mut projections = BTreeMap::new();
        for index in 0..(PROJECTION_CACHE_CAPACITY + 2) {
            let media_key = format!("cache/{index:05}.jpg");
            insert_bounded_projection(
                &mut projections,
                PeopleProjection {
                    media_key,
                    media_fingerprint: format!("sha256:{index}"),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "cache-model".to_string(),
                    identity_revision: 1,
                    catalog_revision: 1,
                    person_ids: Vec::new(),
                    published_at: format!("{index:05}"),
                },
            );
        }
        assert_eq!(projections.len(), PROJECTION_CACHE_CAPACITY);
        assert!(!projections.contains_key("cache/00000.jpg"));
        assert!(!projections.contains_key("cache/00001.jpg"));
        assert!(
            projections.contains_key(&format!("cache/{:05}.jpg", PROJECTION_CACHE_CAPACITY + 1))
        );
    }

    #[test]
    fn failure_codes_are_closed_and_status_safe() {
        for code in FAILURE_CODES {
            assert!(validate_failure_code(code).is_ok());
            assert_eq!(redacted_failure_code(code), *code);
        }
        let long = "x".repeat(128);
        for unsafe_code in [
            "media/person.jpg",
            r"C:\faces\person.jpg",
            "Ada Lovelace",
            "decode failed for private/person.jpg",
            " decode ",
            "decode:error",
            long.as_str(),
        ] {
            assert!(validate_failure_code(unsafe_code).is_err());
            assert_eq!(redacted_failure_code(unsafe_code), "unknown");
        }
    }

    #[test]
    fn calibration_activation_requires_a_verifier_claim_and_stays_inactive_for_wp082() {
        let root = workspace("calibration-activation");
        let store = MatchStore::open(&root).unwrap();
        store.register_model_generation("model-cal", true).unwrap();
        let gallery_digest = store.trusted_gallery_members_digest().unwrap();
        let claim = crate::identity::VerifiedCalibrationClaim::for_test(
            "model-cal",
            "cal-1",
            &gallery_digest,
        );
        let verification = CalibrationVerification::for_test(claim);
        store
            .register_calibration_verification(&verification)
            .unwrap();
        let activation: CalibrationActivation = store
            .require(CALIBRATION_TABLE, "cal-1", "calibration")
            .unwrap();
        assert!(!activation.active);
        assert!(!activation.wp084_runtime_ready);
        assert!(!activation.wp087_release_ready);
        assert_eq!(store.count(CALIBRATION_SPENT_TABLE).unwrap(), 1);
        assert!(store
            .register_calibration_verification(&verification)
            .is_err());
        close(&root, store);
    }

    #[test]
    fn rejected_activation_envelope_still_spends_authenticated_observations() {
        let root = workspace("calibration-rejected-envelope-spend");
        let store = MatchStore::open(&root).unwrap();
        store.register_model_generation("model-cal", true).unwrap();
        let gallery_digest = store.trusted_gallery_members_digest().unwrap();
        let claim =
            crate::identity::VerifiedCalibrationClaim::for_test_with_oversized_candidate_set(
                "model-cal",
                "cal-rejected-envelope",
                &gallery_digest,
            );
        let verification = CalibrationVerification::for_test(claim);

        assert!(store
            .register_calibration_verification(&verification)
            .unwrap_err()
            .contains("invalid runtime envelope"));
        assert_eq!(store.count(CALIBRATION_SPENT_TABLE).unwrap(), 1);
        assert!(store
            .register_calibration_verification(&verification)
            .unwrap_err()
            .contains("already spent"));
        assert_eq!(store.count(CALIBRATION_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn domain_truth_review_trust_rebuild_and_proven_move_round_trip() {
        let root = workspace("domain");
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::Running);
        let person = store
            .create_person("Ada", vec!["A".to_string(), "ada".to_string()])
            .unwrap();
        let look_a = store.create_look(&person.person_id, "Studio").unwrap();
        let look_b = store.create_look(&person.person_id, "Street").unwrap();
        let set = store
            .create_template_set(&look_a.look_id, "Trusted Studio")
            .unwrap();
        let catalog_revision = store.refresh_autocomplete().unwrap();
        let matches = store.autocomplete("ada", catalog_revision, 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].person_id, person.person_id);
        assert!(store.autocomplete("ada", catalog_revision + 1, 10).is_err());
        store.register_model_generation("model-a", true).unwrap();
        store.activate_model_generation("model-a").unwrap();
        let strict_envelope_hash = "a".repeat(64);
        activate_calibration(&store, "model-a", "cal-a", &strict_envelope_hash);
        let kept = store
            .create_face(face(
                "face-kept",
                "media/old.jpg",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                true,
            ))
            .unwrap();
        let job = create_running_job(&store, "root-domain", "model-a");
        let kept_asset = store
            .enqueue_asset(&job.job_id, &kept.media_key, &kept.media_fingerprint)
            .unwrap();
        let kept_fence = fence(&job, &kept_asset);
        let embed_permit = stage_permit(&store, &kept_fence, JobStage::Embed);
        store
            .put_embedding(
                embedding(&kept, "model-a", &job),
                &kept_fence,
                &embed_permit,
            )
            .unwrap();

        let suggestion = Suggestion {
            suggestion_id: suggestion_id(&kept.face_id, &person.person_id),
            face_id: kept.face_id.clone(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.91,
            model_generation: "model-a".to_string(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: kept.media_fingerprint.clone(),
            face_revision: kept.face_revision,
            person_revision: person.revision,
            job_id: job.job_id.clone(),
            created_at: now(),
        };
        let suggest_permit = stage_permit(&store, &kept_fence, JobStage::Suggest);
        store
            .record_suggestion(suggestion, &kept_fence, &suggest_permit)
            .unwrap();
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, &kept.face_id)
            .unwrap()
            .is_none());
        let operator_fence = store
            .operator_mutation_fence(&kept.face_id, &person.person_id)
            .unwrap();
        let confirmed = store
            .review_same(&kept.face_id, &person.person_id, &operator_fence)
            .unwrap();
        assert_eq!(confirmed.placement, "unsorted");
        assert_eq!(confirmed.state, "operator_confirmed");
        assert!(store
            .list::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)
            .unwrap()
            .is_empty());
        let operator_fence = store
            .operator_mutation_fence(&kept.face_id, &person.person_id)
            .unwrap();
        let confirmed = store
            .move_to_look(&kept.face_id, &look_a.look_id, &operator_fence)
            .unwrap();
        assert_eq!(confirmed.look_id.as_deref(), Some(look_a.look_id.as_str()));
        let operator_fence = store
            .operator_mutation_fence(&kept.face_id, &person.person_id)
            .unwrap();
        store
            .trusted_search_reconcile_failures
            .store(1, Ordering::SeqCst);
        let authorized = store
            .authorize_trusted_reference(
                &set.set_id,
                &kept.face_id,
                true,
                TrustedEligibility {
                    provenance: "operator-authorized fixture".to_string(),
                    model_generation: "model-a".to_string(),
                    embedding_id: embedding_id(&kept.face_id, "model-a"),
                    policy_version: TRUSTED_POLICY_VERSION.to_string(),
                },
                &operator_fence,
            )
            .unwrap();
        assert_eq!(
            authorized.membership_id,
            trusted_member_id(&set.set_id, &kept.face_id)
        );
        assert_eq!(
            store
                .require::<TrustedTemplateMembership>(
                    TRUSTED_MEMBER_TABLE,
                    &authorized.membership_id,
                    "committed trusted authorization",
                )
                .unwrap(),
            authorized,
            "a failed eager derived-index rebuild must not turn a committed authorization into an error"
        );
        assert!(store
            .nearest_trusted(&vector(0, 0.0), "model-a", 8, 4)
            .unwrap_err()
            .contains("current trusted index build receipt"));
        assert_eq!(store.reconcile_trusted_search().unwrap(), 1);
        // Trusted-gallery mutation deliberately invalidates prior calibration.
        // Restore this downstream-ready test fixture only after the gallery is final.
        activate_calibration(&store, "model-a", "cal-a", &strict_envelope_hash);
        let trusted = store
            .nearest_trusted(&vector(0, 0.0), "model-a", 8, 4)
            .unwrap();
        assert!(trusted.plan_uses_hnsw, "plan: {}", trusted.plan);
        assert_eq!(trusted.neighbors.len(), 1);
        assert_eq!(trusted.neighbors[0].person_id, person.person_id);
        assert_eq!(trusted.neighbors[0].look_id, look_a.look_id);

        let derived_job = create_running_job(&store, "root-domain", "model-a");
        let derived_asset = store
            .enqueue_asset(
                &derived_job.job_id,
                "media/derived.jpg",
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            )
            .unwrap();
        let confirmed_derived_asset = store
            .enqueue_asset(
                &derived_job.job_id,
                "media/confirmed-derived.jpg",
                "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            )
            .unwrap();
        let derived_fence = fence(&derived_job, &derived_asset);
        let derived_detect = stage_permit(&store, &derived_fence, JobStage::Detect);
        let derived = store
            .create_derived_face(
                derived_face(
                    "media/derived.jpg",
                    "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                    0,
                ),
                &derived_fence,
                &derived_detect,
            )
            .unwrap();
        let confirmed_derived_fence = fence(&derived_job, &confirmed_derived_asset);
        let confirmed_derived_detect =
            stage_permit(&store, &confirmed_derived_fence, JobStage::Detect);
        let confirmed_derived = store
            .create_derived_face(
                derived_face(
                    "media/confirmed-derived.jpg",
                    "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    0,
                ),
                &confirmed_derived_fence,
                &confirmed_derived_detect,
            )
            .unwrap();
        let derived_embed = stage_permit(&store, &derived_fence, JobStage::Embed);
        store
            .put_embedding(
                embedding(&derived, "model-a", &derived_job),
                &derived_fence,
                &derived_embed,
            )
            .unwrap();
        let strict_permit = stage_permit(&store, &derived_fence, JobStage::Suggest);
        let strict = store
            .recognize_and_persist_strict(&derived.face_id, &derived_fence, &strict_permit)
            .unwrap();
        assert_eq!(strict.state, "committed_strict_automatic");
        assert_eq!(strict.person_id.as_deref(), Some(person.person_id.as_str()));
        assert_eq!(
            strict.winning_look_id.as_deref(),
            Some(look_a.look_id.as_str())
        );
        let strict_assignment: Assignment = store
            .require(
                ASSIGNMENT_TABLE,
                &derived.face_id,
                "fresh strict assignment",
            )
            .unwrap();
        let strict_embedding: FaceEmbedding = store
            .require(
                EMBEDDING_TABLE,
                &embedding_id(&derived.face_id, "model-a"),
                "fresh strict embedding",
            )
            .unwrap();
        assert!(strict_assignment.created_at >= strict_embedding.created_at);
        let operator_fence = store
            .operator_mutation_fence(&confirmed_derived.face_id, &person.person_id)
            .unwrap();
        store
            .assign_face(
                &confirmed_derived.face_id,
                &person.person_id,
                None,
                AssignmentState::OperatorConfirmed,
                "operator-confirmed-derived-fixture",
                None,
                None,
                None,
                Some(&operator_fence),
                None,
                None,
                confirmed_derived.face_revision,
                person.revision,
            )
            .unwrap();

        let context_request = MediaContextRequest {
            media_key: "media/old.jpg".into(),
            media_fingerprint: "a".repeat(64),
            expected_revision: 0,
            capture_unix_millis: Some(100),
            time_window_millis: Some(5),
            album_ids: vec!["album-1".into()],
        };
        store.replace_media_context(&context_request).unwrap();
        let collision = CanonicalMediaContext {
            media_key: "collision.jpg".into(),
            media_fingerprint: "a".repeat(64),
            revision: 1,
            capture_unix_millis: None,
            time_window_millis: None,
            album_ids: vec!["occupied".into()],
        };
        store
            .upsert_json(
                context::MEDIA_CONTEXT_TABLE,
                &collision.media_key,
                &collision,
            )
            .unwrap();
        assert!(store
            .rekey_media(
                "media/old.jpg",
                "collision.jpg",
                &context_request.media_fingerprint
            )
            .unwrap_err()
            .contains("canonical context"));
        let mut wrong_tombstone = collision.clone();
        wrong_tombstone.album_ids.clear();
        wrong_tombstone.media_fingerprint = "b".repeat(64);
        store
            .upsert_json(
                context::MEDIA_CONTEXT_TABLE,
                &wrong_tombstone.media_key,
                &wrong_tombstone,
            )
            .unwrap();
        assert!(store
            .rekey_media(
                "media/old.jpg",
                "collision.jpg",
                &context_request.media_fingerprint
            )
            .unwrap_err()
            .contains("canonical context"));
        assert_eq!(
            store
                .media_context("media/old.jpg")
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            store
                .rekey_media(
                    "media/old.jpg",
                    "renamed/kept.jpg",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap(),
            1
        );
        let moved: FaceObservation = store.require(FACE_TABLE, &kept.face_id, "face").unwrap();
        assert_eq!(moved.media_key, "renamed/kept.jpg");
        let mut moved_context = store.media_context("renamed/kept.jpg").unwrap().unwrap();
        assert_eq!(moved_context.revision, 2);
        assert_eq!(moved_context.album_ids, vec!["album-1"]);
        let mut old_context = store.media_context("media/old.jpg").unwrap().unwrap();
        assert_eq!(old_context.revision, 2);
        assert!(old_context.album_ids.is_empty() && old_context.capture_unix_millis.is_none());
        assert!(store.replace_media_context(&context_request).is_err());
        store
            .rekey_media(
                "renamed/kept.jpg",
                "media/old.jpg",
                &context_request.media_fingerprint,
            )
            .unwrap();
        let returned = store.media_context("media/old.jpg").unwrap().unwrap();
        assert_eq!(returned.revision, 3);
        assert_eq!(returned.album_ids, vec!["album-1"]);
        assert!(store
            .media_context("renamed/kept.jpg")
            .unwrap()
            .unwrap()
            .album_ids
            .is_empty());
        assert!(store.replace_media_context(&context_request).is_err());
        store
            .rekey_media(
                "media/old.jpg",
                "renamed/kept.jpg",
                &context_request.media_fingerprint,
            )
            .unwrap();
        moved_context = store.media_context("renamed/kept.jpg").unwrap().unwrap();
        old_context = store.media_context("media/old.jpg").unwrap().unwrap();
        assert_eq!(moved_context.revision, 4);
        assert_eq!(old_context.revision, 4);
        assert!(store
            .rekey_media("renamed/kept.jpg", "bad.jpg", "sha256:wrong")
            .is_err());

        store.rebuild_regenerable().unwrap();
        assert_eq!(
            store.media_context("renamed/kept.jpg").unwrap(),
            Some(moved_context)
        );
        assert_eq!(
            store.media_context("media/old.jpg").unwrap(),
            Some(old_context)
        );
        assert_eq!(store.list::<Person>(PERSON_TABLE).unwrap().len(), 1);
        assert_eq!(store.list::<Look>(LOOK_TABLE).unwrap().len(), 2);
        assert_eq!(store.list::<Assignment>(ASSIGNMENT_TABLE).unwrap().len(), 2);
        assert_eq!(
            store
                .list::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, &derived.face_id)
            .unwrap()
            .is_none());
        assert!(store
            .list::<MatchOperation>(OPERATION_TABLE)
            .unwrap()
            .iter()
            .any(|operation| operation.face_id.as_deref() == Some(derived.face_id.as_str())));
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, &confirmed_derived.face_id)
            .unwrap()
            .is_some());
        assert!(store
            .list::<FaceEmbedding>(EMBEDDING_TABLE)
            .unwrap()
            .is_empty());
        assert_eq!(kept_asset.media_key, "media/old.jpg");
        let operator_fence = store
            .operator_mutation_fence(&kept.face_id, &person.person_id)
            .unwrap();
        assert!(store
            .move_to_look(&kept.face_id, &look_b.look_id, &operator_fence)
            .is_ok());
        assert!(store
            .nearest_trusted(&vector(0, 0.0), "model-a", 8, 4)
            .unwrap_err()
            .contains("current trusted index build receipt"));
        assert_eq!(store.reconcile_trusted_search().unwrap(), 0);
        assert!(store
            .nearest_trusted(&vector(0, 0.0), "model-a", 8, 4)
            .unwrap()
            .neighbors
            .is_empty());
        assert!(store
            .list::<TrustedTemplateMembership>(TRUSTED_MEMBER_TABLE)
            .unwrap()
            .is_empty());
        assert!(store
            .list::<CalibrationActivation>(CALIBRATION_TABLE)
            .unwrap()
            .iter()
            .all(|activation| !activation.active));
        assert!(store
            .list::<MatchOperation>(OPERATION_TABLE)
            .unwrap()
            .iter()
            .any(|operation| operation.kind == "authorize_trusted_reference"));
        let expected_moved = store.media_context("renamed/kept.jpg").unwrap();
        let expected_old = store.media_context("media/old.jpg").unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(
            store.media_context("renamed/kept.jpg").unwrap(),
            expected_moved
        );
        assert_eq!(store.media_context("media/old.jpg").unwrap(), expected_old);
        close(&root, store);
    }

    #[test]
    fn reviews_jobs_pause_restart_and_revision_fences_are_exact() {
        let root = workspace("jobs");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Grace", Vec::new()).unwrap();
        let observation = store
            .create_face(face(
                "face-job",
                "job.jpg",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                true,
            ))
            .unwrap();
        store.register_model_generation("model-job", true).unwrap();
        store.activate_model_generation("model-job").unwrap();
        activate_calibration(&store, "model-job", "cal-job", "envelope-job");
        let not_sure_fence = store
            .operator_mutation_fence(&observation.face_id, &person.person_id)
            .unwrap();
        store
            .review_not_sure(&observation.face_id, &person.person_id, &not_sure_fence)
            .unwrap();
        assert!(store
            .list::<Assignment>(ASSIGNMENT_TABLE)
            .unwrap()
            .is_empty());
        assert!(store
            .list::<CannotLinkConstraint>(CONSTRAINT_TABLE)
            .unwrap()
            .is_empty());
        let strict_job = create_running_job(&store, "root-a", "model-job");
        let strict_asset = store
            .enqueue_asset(
                &strict_job.job_id,
                "job.jpg",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap();
        let strict_fence = fence(&strict_job, &strict_asset);
        let strict_permit = stage_permit(&store, &strict_fence, JobStage::Suggest);
        store
            .assign_face(
                &observation.face_id,
                &person.person_id,
                None,
                AssignmentState::CommittedStrictAutomatic,
                "strict",
                Some("model-job"),
                Some("cal-job"),
                Some("envelope-job"),
                None,
                Some(&strict_fence),
                Some(&strict_permit),
                observation.face_revision,
                person.revision,
            )
            .unwrap();
        let operator_fence = store
            .operator_mutation_fence(&observation.face_id, &person.person_id)
            .unwrap();
        store
            .this_is_not(&observation.face_id, &person.person_id, &operator_fence)
            .unwrap();
        assert!(store
            .list::<Assignment>(ASSIGNMENT_TABLE)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .list::<CannotLinkConstraint>(CONSTRAINT_TABLE)
                .unwrap()
                .len(),
            1
        );

        let job = create_running_job(&store, "root-a", "model-job");
        let asset = store
            .enqueue_asset(
                &job.job_id,
                "job.jpg",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap();
        assert_eq!(
            store
                .enqueue_asset(
                    &job.job_id,
                    "job.jpg",
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .unwrap()
                .asset_id,
            asset.asset_id
        );
        let current_fence = fence(&job, &asset);
        let mut stale = current_fence.clone();
        stale.catalog_revision += 1;
        let stale_attempt = stage_permit(&store, &current_fence, JobStage::Discover);
        assert!(store
            .commit_asset_stage(&asset.asset_id, JobStage::Discover, &stale, &stale_attempt,)
            .is_err());
        let first_failure = stage_permit(&store, &current_fence, JobStage::Discover);
        store
            .record_asset_failure(
                &asset.asset_id,
                "decode",
                "fixture failure",
                &current_fence,
                &first_failure,
            )
            .unwrap();
        let repeated_failure = stage_permit(&store, &current_fence, JobStage::Discover);
        store
            .record_asset_failure(
                &asset.asset_id,
                "decode",
                "fixture failure",
                &current_fence,
                &repeated_failure,
            )
            .unwrap();
        let failure_status = store.status().unwrap();
        assert_eq!(failure_status["jobs"]["progress"]["failed"], 1);
        assert_eq!(
            failure_status["jobs"]["failure_codes"][0]["failure_code"],
            "decode"
        );
        assert_eq!(failure_status["jobs"]["includes_failure_messages"], false);
        assert_eq!(
            store
                .require::<IndexJob>(JOB_TABLE, &job.job_id, "job")
                .unwrap()
                .failed,
            1
        );
        let projection = PeopleProjection {
            media_key: current_fence.media_key.clone(),
            media_fingerprint: current_fence.media_fingerprint.clone(),
            schema_generation: current_fence.schema_generation.clone(),
            model_generation: current_fence.model_generation.clone(),
            identity_revision: current_fence.identity_revision,
            catalog_revision: current_fence.catalog_revision,
            person_ids: vec![person.person_id.clone()],
            published_at: now(),
        };
        let discover_commit = stage_permit(&store, &current_fence, JobStage::Discover);
        store
            .commit_asset_stage(
                &asset.asset_id,
                JobStage::Discover,
                &current_fence,
                &discover_commit,
            )
            .unwrap();
        let discover_retry = raw_stage_permit(&store, &current_fence, JobStage::Discover);
        store
            .commit_asset_stage(
                &asset.asset_id,
                JobStage::Discover,
                &current_fence,
                &discover_retry,
            )
            .unwrap();
        let late_derived = raw_stage_permit(&store, &current_fence, JobStage::Detect);
        let detect_commit = raw_stage_permit(&store, &current_fence, JobStage::Detect);
        store
            .commit_asset_stage(
                &asset.asset_id,
                JobStage::Detect,
                &current_fence,
                &detect_commit,
            )
            .unwrap();
        assert!(store
            .create_derived_face(
                derived_face(
                    "job.jpg",
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    1,
                ),
                &current_fence,
                &late_derived,
            )
            .is_err());
        let stale_projection = stage_permit(&store, &current_fence, JobStage::Persist);
        assert!(store
            .publish_projection(projection.clone(), &stale, &stale_projection)
            .is_err());
        let late_projection = raw_stage_permit(&store, &current_fence, JobStage::Persist);
        let current_projection = raw_stage_permit(&store, &current_fence, JobStage::Persist);
        store
            .publish_projection(projection.clone(), &current_fence, &current_projection)
            .unwrap();
        assert_eq!(
            store.job_asset(&asset.asset_id).unwrap().next_stage,
            JobStage::Suggest.as_str()
        );
        assert_eq!(
            store.cached_projection(&projection.media_key).unwrap(),
            Some(projection.clone())
        );
        store.caches.write().unwrap().projections.clear();
        assert!(store
            .cached_projection(&projection.media_key)
            .unwrap()
            .is_none());
        assert_eq!(store.refresh_projection_cache().unwrap(), 1);
        assert_eq!(
            store.cached_projection(&projection.media_key).unwrap(),
            Some(projection.clone())
        );
        store.caches.write().unwrap().projections.clear();
        assert_eq!(
            store.warm_projection(&projection.media_key).unwrap(),
            Some(projection.clone())
        );
        let persist_commit = raw_stage_permit(&store, &current_fence, JobStage::Persist);
        store
            .commit_asset_stage(
                &asset.asset_id,
                JobStage::Persist,
                &current_fence,
                &persist_commit,
            )
            .unwrap();
        assert!(store
            .publish_projection(projection.clone(), &current_fence, &late_projection)
            .is_err());
        let late_failure = raw_stage_permit(&store, &current_fence, JobStage::Suggest);
        for stage in [JobStage::Suggest, JobStage::Complete] {
            let permit = raw_stage_permit(&store, &current_fence, stage);
            store
                .commit_asset_stage(&asset.asset_id, stage, &current_fence, &permit)
                .unwrap();
        }
        assert!(store
            .record_asset_failure(
                &asset.asset_id,
                "late",
                "must be rejected",
                &current_fence,
                &late_failure,
            )
            .is_err());
        let job_after: IndexJob = store.require(JOB_TABLE, &job.job_id, "job").unwrap();
        assert_eq!(
            (job_after.discovered, job_after.completed, job_after.failed),
            (1, 1, 0)
        );
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Cancelled)
            .unwrap();
        store
            .create_look(&person.person_id, "Cache invalidation")
            .unwrap();
        assert!(store
            .cached_projection(&projection.media_key)
            .unwrap()
            .is_none());
        assert!(store
            .warm_projection(&projection.media_key)
            .unwrap()
            .is_none());

        let stale_job = create_running_job(&store, "root-stale", "model-job");
        let stale_asset = store
            .enqueue_asset(&stale_job.job_id, "stale.jpg", "sha256:stale")
            .unwrap();
        let stale_fence = fence(&stale_job, &stale_asset);
        let stale_revision_permit = stage_permit(&store, &stale_fence, JobStage::Discover);
        let updated = store
            .update_person(
                &person.person_id,
                person.revision,
                "Grace Hopper",
                Vec::new(),
            )
            .unwrap();
        assert_eq!(updated.revision, person.revision + 1);
        assert!(store
            .commit_asset_stage(
                &stale_asset.asset_id,
                JobStage::Discover,
                &stale_fence,
                &stale_revision_permit,
            )
            .is_err());

        let recover_job = store.create_job("root-recover", "model-job").unwrap();
        store
            .set_job_lifecycle(&recover_job.job_id, JobLifecycle::Running)
            .unwrap();

        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        store.add_hold(HoldReason::ImmersiveFullscreen).unwrap();
        assert!(!store.can_admit(JobLifecycle::Running).unwrap());
        store.remove_hold(HoldReason::ImmersiveFullscreen).unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert!(!store.can_admit(JobLifecycle::Running).unwrap());
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(
            reopened.desired_mode().unwrap(),
            DesiredMode::OperatorPaused
        );
        assert!(reopened.holds().unwrap().is_empty());
        let recovered: IndexJob = reopened
            .require(JOB_TABLE, &recover_job.job_id, "job")
            .unwrap();
        assert_eq!(recovered.lifecycle().unwrap(), JobLifecycle::Paused);
        let cancelled: IndexJob = reopened.require(JOB_TABLE, &job.job_id, "job").unwrap();
        assert_eq!(cancelled.lifecycle().unwrap(), JobLifecycle::Cancelled);
        close(&root, reopened);
    }

    #[test]
    fn hnsw_candidates_are_planner_selected_and_exactly_reranked() {
        let root = workspace("hnsw");
        let store = MatchStore::open(&root).unwrap();
        store.register_model_generation("model-v1", true).unwrap();
        store.activate_model_generation("model-v1").unwrap();
        let job = create_running_job(&store, "root-vectors", "model-v1");
        for index in 0..12 {
            let media_key = format!("media-{index}.jpg");
            let media_fingerprint = format!("sha256:{index}");
            let asset = store
                .enqueue_asset(&job.job_id, &media_key, &media_fingerprint)
                .unwrap();
            let asset_fence = fence(&job, &asset);
            let detect_permit = stage_permit(&store, &asset_fence, JobStage::Detect);
            let observation = store
                .create_derived_face(
                    derived_face(&media_key, &media_fingerprint, index as u32),
                    &asset_fence,
                    &detect_permit,
                )
                .unwrap();
            let embed_permit = stage_permit(&store, &asset_fence, JobStage::Embed);
            store
                .put_embedding(
                    FaceEmbedding {
                        embedding_id: embedding_id(&observation.face_id, "model-v1"),
                        face_id: observation.face_id.clone(),
                        vector: vector(index, index as f32 / 100.0),
                        model_generation: "model-v1".to_string(),
                        schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                        media_fingerprint: observation.media_fingerprint,
                        face_revision: observation.face_revision,
                        job_id: job.job_id.clone(),
                        active: true,
                        created_at: now(),
                    },
                    &asset_fence,
                    &embed_permit,
                )
                .unwrap();
        }
        let result = store.nearest(&vector(0, 0.01), "model-v1", 12, 4).unwrap();
        assert!(result.plan_uses_hnsw, "plan: {}", result.plan);
        let status = store.status().unwrap();
        assert_eq!(status["vector"]["last_query_plan"]["observed"], true);
        assert_eq!(status["vector"]["last_query_plan"]["uses_hnsw"], true);
        assert_eq!(
            result.neighbors[0].face_id,
            derived_face_id("media-0.jpg", "sha256:0", 0, MATCH_SCHEMA_GENERATION)
        );
        assert!(result
            .neighbors
            .windows(2)
            .all(|pair| pair[0].exact_cosine >= pair[1].exact_cosine));

        store.register_model_generation("model-v2", true).unwrap();
        store.activate_model_generation("model-v2").unwrap();
        let generations = store.list::<ModelGeneration>(GENERATION_TABLE).unwrap();
        assert_eq!(
            generations
                .iter()
                .find(|row| row.generation == "model-v1")
                .unwrap()
                .state,
            "usable"
        );
        store.activate_model_generation("model-v1").unwrap();
        close(&root, store);
    }

    #[test]
    fn wp087_resource_telemetry_retains_between_poll_peaks_and_exact_lease_balance() {
        let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
        let baseline = governor.telemetry().unwrap();
        let request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 8,
            cpu_inference: 1,
            decoded_bytes: 16,
            gpu_vram_bytes: 1,
            worker_memory_bytes: 64,
            surreal_writes: 1,
            vector_index_builds: 1,
        };
        let mut first = governor.try_acquire(request).unwrap();
        let second = governor
            .try_acquire(ResourceRequest {
                vector_index_builds: 0,
                ..request
            })
            .unwrap();
        assert_eq!(
            governor.try_acquire(request).err().as_deref(),
            Some("resource_pressure")
        );
        let replacement = ResourceRequest {
            cpu_inference: 2,
            queued_bytes: 16,
            ..request
        };
        assert_eq!(
            governor
                .try_replace(&mut first, replacement)
                .err()
                .as_deref(),
            Some("resource_pressure")
        );
        assert_eq!(first.request, request);
        drop(second);
        governor.try_replace(&mut first, replacement).unwrap();
        first.release_worker_preparation_bytes().unwrap();
        first.release_worker_preparation_bytes().unwrap();
        drop(first);
        // No telemetry sample observed either live lease. Admission-time peaks
        // must survive the releases, unlike a maximum of sampled live gauges.
        let final_state = governor.telemetry().unwrap();
        assert_eq!(governor.usage().unwrap(), ResourceUsage::default());
        assert_eq!(final_state.current_usage, ResourceUsage::default());
        assert_eq!(final_state.lifetime_id, baseline.lifetime_id);
        assert_eq!(final_state.scope, "governor_lifetime_including_warmup");
        assert_eq!(
            final_state.peak_usage,
            ResourceRequest {
                admitted_items: 2,
                queued_items: 2,
                queued_bytes: 16,
                cpu_inference: 2,
                decoded_bytes: 32,
                gpu_vram_bytes: 2,
                worker_memory_bytes: 128,
                surreal_writes: 2,
                vector_index_builds: 1,
            }
        );
        assert_eq!((final_state.acquisitions, final_state.releases), (2, 2));
        assert_eq!(
            (final_state.replacements, final_state.preparation_releases),
            (1, 1)
        );
        assert_eq!(final_state.pressure_events, 2);
        assert!(!final_state.overflow);
        assert_eq!(
            governor.clone().telemetry().unwrap().lifetime_id,
            final_state.lifetime_id
        );
        assert_ne!(
            MatchResourceGovernor::new(governor.budget())
                .unwrap()
                .telemetry()
                .unwrap()
                .lifetime_id,
            final_state.lifetime_id
        );
    }

    #[test]
    fn wp087_resource_telemetry_overflow_is_explicit_and_does_not_wrap_or_change_admission() {
        let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
        governor.usage.lock().unwrap().acquisitions = u64::MAX;
        let request = ResourceRequest {
            cpu_inference: 1,
            ..ResourceRequest::default()
        };
        let lease = governor.try_acquire(request).unwrap();
        let active = governor.telemetry().unwrap();
        assert!(active.overflow);
        assert_eq!(active.acquisitions, u64::MAX);
        assert_eq!(active.current_usage, request);
        drop(lease);
        let final_state = governor.telemetry().unwrap();
        assert_eq!(final_state.current_usage, ResourceUsage::default());
        assert_eq!(final_state.releases, 1);
        assert!(final_state.overflow);
    }

    #[test]
    fn wp087_governor_interval_retains_transient_pressure_and_spanning_stage_lease() {
        let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
        let mut lease = governor
            .try_acquire(ResourceRequest {
                cpu_inference: 1,
                admitted_items: 1,
                ..ResourceRequest::default()
            })
            .unwrap();
        lease.tag_stage(JobStage::Detect).unwrap();
        let first = governor.interval_checkpoint().unwrap();
        let one = &first["governor_interval"];
        assert_eq!(one["closing_live_stage_leases"][1], 1);
        assert_eq!(one["closing_unclassified_leases"], 0);
        assert_eq!(one["peak_usage"]["cpu_inference"], 1);
        let transient = governor
            .try_acquire(ResourceRequest {
                cpu_inference: 1,
                ..ResourceRequest::default()
            })
            .unwrap();
        assert_eq!(
            governor
                .try_acquire(ResourceRequest {
                    cpu_inference: 1,
                    ..ResourceRequest::default()
                })
                .err()
                .as_deref(),
            Some("resource_pressure")
        );
        drop(transient);
        drop(lease);
        // Ordinary read-only telemetry must not consume another observer's interval.
        let _ = governor.telemetry().unwrap();
        let second = governor.clone().interval_checkpoint().unwrap();
        let two = &second["governor_interval"];
        assert_eq!(two["lifetime_id"], one["lifetime_id"]);
        assert_eq!(two["runtime_id"], first["runtime_id"]);
        assert_eq!(
            two["sequence"].as_u64().unwrap(),
            one["sequence"].as_u64().unwrap() + 1
        );
        assert_eq!(two["start_us"], one["end_us"]);
        assert_eq!(two["opening_usage"], one["closing_usage"]);
        assert_eq!(
            two["opening_live_stage_leases"],
            one["closing_live_stage_leases"]
        );
        assert_eq!(two["peak_usage"]["cpu_inference"], 2);
        assert_eq!(two["pressure_events"], 1);
        assert_eq!(two["acquisitions"], 1);
        assert_eq!(two["releases"], 2);
        assert_eq!(
            two["closing_usage"],
            serde_json::to_value(ResourceUsage::default()).unwrap()
        );
        assert_eq!(
            two["closing_live_stage_leases"],
            serde_json::to_value([0_u64; 7]).unwrap()
        );
        assert_eq!(two["closing_unclassified_leases"], 0);
        let third = governor.interval_checkpoint().unwrap();
        assert_eq!(third["governor_interval"]["peak_usage"]["cpu_inference"], 0);
        assert_eq!(third["governor_interval"]["pressure_events"], 0);
        assert_eq!(
            third["lease_activity"]["endpoint_scope"],
            "admitted_resource_leases_excluding_kernel_execution"
        );
    }

    #[test]
    fn wp087_public_diagnostics_observe_the_shared_live_governor_without_identity_rows() {
        let root = workspace("wp087-live-governor-diagnostics");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("wp087-private-person-canary", vec![])
            .unwrap();
        let lease = store
            .governor()
            .try_acquire(ResourceRequest {
                cpu_inference: 1,
                decoded_bytes: 64,
                ..ResourceRequest::default()
            })
            .unwrap();
        let snapshot = store.clone().public_snapshot().unwrap();
        assert_eq!(snapshot["execution"]["resource_usage"]["cpu_inference"], 1);
        assert_eq!(
            snapshot["execution"]["resource_telemetry"]["current_usage"]["decoded_bytes"],
            64
        );
        assert_eq!(snapshot["execution"]["resource_budget"]["cpu_inference"], 2);
        assert_eq!(snapshot["catalog"]["total_people"], 1);
        assert_eq!(snapshot["catalog"]["materialized_people_rows"], 0);
        assert!(!snapshot.to_string().contains("wp087-private-person-canary"));
        let lifetime = snapshot["execution"]["resource_telemetry"]["lifetime_id"].clone();
        drop(lease);
        let terminal = store.public_snapshot().unwrap();
        assert_eq!(
            terminal["execution"]["resource_telemetry"]["lifetime_id"],
            lifetime
        );
        assert_eq!(
            terminal["execution"]["resource_telemetry"]["current_usage"]["cpu_inference"],
            0
        );
        assert_eq!(
            terminal["execution"]["resource_telemetry"]["peak_usage"]["cpu_inference"],
            1
        );
        assert_eq!(
            terminal["execution"]["resource_telemetry"]["acquisitions"],
            1
        );
        assert_eq!(terminal["execution"]["resource_telemetry"]["releases"], 1);
        close(&root, store);
    }

    #[test]
    fn wp087_public_stage_counts_reconcile_canonical_rows_after_restart_without_keys() {
        let root = workspace("wp087-canonical-stage-diagnostics");
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(
            store.public_snapshot().unwrap()["execution"]["index_stage"],
            ""
        );
        for (index, stage) in [JobStage::Detect, JobStage::Persist, JobStage::Detect]
            .into_iter()
            .enumerate()
        {
            let asset = JobAsset {
                asset_id: format!("stage-asset-{index}"),
                job_id: "historical-paused-job".into(),
                media_key: format!("private-stage-key-canary-{index}"),
                source_path: Some("private-stage-path-canary".into()),
                media_fingerprint: "a".repeat(64),
                next_stage: stage.as_str().into(),
                completed_stages: vec!["discover".into()],
                failure_code: None,
                failure_message: None,
                skipped_code: None,
                skipped_message: None,
                schema_generation: MATCH_SCHEMA_GENERATION.into(),
                model_generation: "stage-test-generation".into(),
                identity_revision: 0,
                catalog_revision: 0,
                updated_at: now(),
            };
            store
                .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
                .unwrap();
        }
        let snapshot = store.public_snapshot().unwrap();
        assert_eq!(snapshot["execution"]["index_stage"], "detect:2,persist:1");
        assert_eq!(
            snapshot["execution"]["index_stage_counts"],
            json!({"detect":2,"persist":1})
        );
        assert_eq!(
            snapshot["execution"]["index_stage_scope"],
            "persisted_asset_next_stage_counts"
        );
        assert!(!snapshot.to_string().contains("private-stage"));
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(
            reopened.public_snapshot().unwrap()["execution"]["index_stage_counts"],
            snapshot["execution"]["index_stage_counts"]
        );
        let mut invalid: JobAsset = reopened
            .require(JOB_ASSET_TABLE, "stage-asset-0", "JobAsset")
            .unwrap();
        invalid.next_stage = "private-stage-invalid-canary".into();
        reopened
            .upsert_json(JOB_ASSET_TABLE, &invalid.asset_id, &invalid)
            .unwrap();
        assert_eq!(
            reopened.public_snapshot().unwrap_err(),
            "Match persisted stage is unknown"
        );
        close(&root, reopened);
    }

    #[test]
    fn wp087_public_progress_counts_all_jobs_beyond_recent_projection_after_restart() {
        let root = workspace("wp087-canonical-all-job-progress");
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(
            store.public_snapshot().unwrap()["job_progress"],
            json!({"discovered":0,"completed":0,"failed":0,"skipped":0})
        );
        let timestamp = now();
        let rows = (0..201)
            .map(|index| {
                let weight = if index == 0 { 100 } else { 1 };
                let job = IndexJob {
                    job_id: format!("canonical-job-{index:04}"),
                    root_key: "private-root-canary".into(),
                    lifecycle: JobLifecycle::Completed.as_str().into(),
                    schema_generation: MATCH_SCHEMA_GENERATION.into(),
                    model_generation: "diagnostic-generation".into(),
                    identity_revision: 0,
                    catalog_revision: 0,
                    discovered: 6 * weight,
                    completed: 3 * weight,
                    failed: weight,
                    skipped: 2 * weight,
                    failure_code: None,
                    failure_message: None,
                    created_at: if index == 0 {
                        "2000-01-01T00:00:00Z".into()
                    } else {
                        timestamp.clone()
                    },
                    updated_at: if index == 0 {
                        "2000-01-01T00:00:00Z".into()
                    } else {
                        timestamp.clone()
                    },
                };
                (job.job_id.clone(), serde_json::to_value(job).unwrap())
            })
            .collect::<Vec<_>>();
        let refs = rows
            .iter()
            .map(|(id, value)| (JOB_TABLE, id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&refs, &[]).unwrap();
        let snapshot = store.public_snapshot().unwrap();
        assert_eq!(snapshot["jobs"].as_array().unwrap().len(), 200);
        assert_eq!(
            snapshot["job_progress"],
            json!({"discovered":1800,"completed":900,"failed":300,"skipped":600})
        );
        assert_eq!(snapshot["job_progress_scope"], "canonical_all_index_jobs");
        let displayed_completed: u64 = snapshot["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["completed"].as_u64().unwrap())
            .sum();
        assert_eq!(displayed_completed, 600);
        assert!(!snapshot.to_string().contains("private-root-canary"));
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(
            reopened.public_snapshot().unwrap()["job_progress"],
            snapshot["job_progress"]
        );
        close(&root, reopened);
    }

    #[test]
    fn snapshot_resource_authority_is_continuous_across_saturated_detect_transfer() {
        let budget = ResourceBudget {
            worker_memory_bytes: 0,
            admitted_items: 4,
            queued_items: 4,
            queued_bytes: 100,
            cpu_inference: 1,
            decoded_bytes: 100,
            gpu_vram_bytes: 1,
            surreal_writes: 1,
            vector_index_builds: 1,
        };
        let governor = MatchResourceGovernor::new(budget).unwrap();
        let snapshot_request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 90,
            ..ResourceRequest::default()
        };
        let mut first_snapshot = governor.try_acquire(snapshot_request).unwrap();
        let cpu_saturation_request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 1,
            cpu_inference: 1,
            ..ResourceRequest::default()
        };
        let cpu_saturation = governor.try_acquire(cpu_saturation_request).unwrap();
        let usage_while_waiting = governor.usage().unwrap();
        let detect_request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 90,
            cpu_inference: 1,
            decoded_bytes: 90,
            surreal_writes: 1,
            ..ResourceRequest::default()
        };

        assert_eq!(
            governor.try_replace(&mut first_snapshot, detect_request),
            Err("resource_pressure".to_string())
        );
        assert_eq!(governor.usage().unwrap(), usage_while_waiting);
        assert_eq!(first_snapshot.request, snapshot_request);
        assert_eq!(
            governor.try_acquire(snapshot_request).err(),
            Some("resource_pressure".to_string())
        );

        drop(cpu_saturation);
        governor
            .try_replace(&mut first_snapshot, detect_request)
            .unwrap();
        assert_eq!(governor.usage().unwrap(), detect_request);
        assert_eq!(first_snapshot.request, detect_request);
        drop(first_snapshot);
        assert_eq!(governor.usage().unwrap(), ResourceUsage::default());
    }

    #[test]
    fn background_admission_and_resource_leases_are_bounded_and_released() {
        let root = workspace("governor");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("governor-model", true)
            .unwrap();
        store.activate_model_generation("governor-model").unwrap();
        let job = create_running_job(&store, "root-governor", "governor-model");
        let asset = store
            .enqueue_asset(&job.job_id, "governor.jpg", "sha256:governor")
            .unwrap();
        let revision_fence = fence(&job, &asset);
        let coordinator = MediaIoCoordinator::new();
        let request = ResourceRequest {
            worker_memory_bytes: 0,
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 1024,
            cpu_inference: 1,
            decoded_bytes: 4096,
            gpu_vram_bytes: 0,
            surreal_writes: 1,
            vector_index_builds: 0,
        };
        let permit = store
            .acquire_background_stage(
                &coordinator,
                RootIdentity::new("local-fixture", 1, RootKind::Local),
                &revision_fence,
                JobStage::Discover,
                request,
            )
            .unwrap();
        assert_eq!(store.governor().usage().unwrap(), request);
        let diagnostics = coordinator.diagnostics();
        assert_eq!(
            diagnostics.roots[0].classes[WorkClass::Background as usize].active,
            1
        );
        permit.finish(PermitOutcome::Success);
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        assert_eq!(
            coordinator.diagnostics().roots[0].classes[WorkClass::Background as usize].active,
            0
        );
        store.add_hold(HoldReason::ImmersiveFullscreen).unwrap();
        assert!(store
            .acquire_background_stage(
                &coordinator,
                RootIdentity::new("local-fixture", 1, RootKind::Local),
                &revision_fence,
                JobStage::Discover,
                request,
            )
            .is_err());
        store.remove_hold(HoldReason::ImmersiveFullscreen).unwrap();
        let budget = store.governor().budget();
        let saturation = ResourceRequest {
            worker_memory_bytes: 0,
            admitted_items: budget.admitted_items,
            queued_items: budget.queued_items,
            queued_bytes: budget.queued_bytes,
            cpu_inference: budget.cpu_inference,
            decoded_bytes: budget.decoded_bytes,
            gpu_vram_bytes: budget.gpu_vram_bytes,
            surreal_writes: budget.surreal_writes,
            vector_index_builds: budget.vector_index_builds,
        };
        let saturated = store
            .acquire_background_stage(
                &coordinator,
                RootIdentity::new("local-fixture", 1, RootKind::Local),
                &revision_fence,
                JobStage::Discover,
                saturation,
            )
            .unwrap();
        assert!(store
            .acquire_background_stage(
                &coordinator,
                RootIdentity::new("local-fixture", 1, RootKind::Local),
                &revision_fence,
                JobStage::Discover,
                request,
            )
            .is_err());
        assert!(store
            .holds()
            .unwrap()
            .contains(&HoldReason::ResourcePressure.as_str().to_string()));
        drop(saturated);
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        assert!(!store
            .holds()
            .unwrap()
            .contains(&HoldReason::ResourcePressure.as_str().to_string()));
        close(&root, store);
    }

    #[test]
    fn canonical_keys_ids_trust_evidence_and_cannot_links_are_enforced() {
        let root = workspace("guards");
        let store = MatchStore::open(&root).unwrap();
        assert!(store
            .create_face(face("bad", "Media\\Bad.JPG", "sha256:bad", true))
            .is_err());
        let mut person = store.create_person("Lin", Vec::new()).unwrap();
        let alternate_person = store.create_person("Mina", Vec::new()).unwrap();
        let look = store.create_look(&person.person_id, "Default").unwrap();
        let set = store.create_template_set(&look.look_id, "Trusted").unwrap();
        let manual = store
            .create_face(face(
                "manual-face",
                "media/good.jpg",
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                true,
            ))
            .unwrap();
        store
            .register_model_generation("guard-model", true)
            .unwrap();
        store.activate_model_generation("guard-model").unwrap();
        let job = create_running_job(&store, "root-guard", "guard-model");
        let asset = store
            .enqueue_asset(&job.job_id, &manual.media_key, &manual.media_fingerprint)
            .unwrap();
        let manual_fence = fence(&job, &asset);
        let embed_permit = stage_permit(&store, &manual_fence, JobStage::Embed);
        store
            .put_embedding(
                embedding(&manual, "guard-model", &job),
                &manual_fence,
                &embed_permit,
            )
            .unwrap();
        for candidate in [&person, &alternate_person] {
            let suggestion = Suggestion {
                suggestion_id: suggestion_id(&manual.face_id, &candidate.person_id),
                face_id: manual.face_id.clone(),
                candidate_person_id: candidate.person_id.clone(),
                similarity: 0.88,
                model_generation: "guard-model".to_string(),
                calibration_generation: None,
                envelope_hash: None,
                media_fingerprint: manual.media_fingerprint.clone(),
                face_revision: manual.face_revision,
                person_revision: candidate.revision,
                job_id: job.job_id.clone(),
                created_at: now(),
            };
            let permit = stage_permit(&store, &manual_fence, JobStage::Suggest);
            store
                .record_suggestion(suggestion, &manual_fence, &permit)
                .unwrap();
        }
        assert_eq!(store.list::<Suggestion>(SUGGESTION_TABLE).unwrap().len(), 2);
        let operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        store
            .review_same(&manual.face_id, &person.person_id, &operator_fence)
            .unwrap();
        assert!(store
            .list::<Suggestion>(SUGGESTION_TABLE)
            .unwrap()
            .is_empty());
        let operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        store
            .move_to_look(&manual.face_id, &look.look_id, &operator_fence)
            .unwrap();
        let mut bad_evidence = TrustedEligibility {
            provenance: "fixture".to_string(),
            model_generation: "guard-model".to_string(),
            embedding_id: "caller-controlled".to_string(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
        };
        let operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        assert!(store
            .authorize_trusted_reference(
                &set.set_id,
                &manual.face_id,
                true,
                bad_evidence.clone(),
                &operator_fence,
            )
            .is_err());
        bad_evidence.embedding_id = embedding_id(&manual.face_id, "guard-model");

        let mut legacy_pose = manual.clone();
        legacy_pose.pose_bucket = "manual_aligned".to_string();
        store
            .upsert_json(FACE_TABLE, &legacy_pose.face_id, &legacy_pose)
            .unwrap();
        let operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        let pose_error = store
            .authorize_trusted_reference(
                &set.set_id,
                &manual.face_id,
                true,
                bad_evidence.clone(),
                &operator_fence,
            )
            .unwrap_err();
        assert!(pose_error.contains("pose evidence is not eligible"));
        store
            .upsert_json(FACE_TABLE, &manual.face_id, &manual)
            .unwrap();

        let operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        store
            .authorize_trusted_reference(
                &set.set_id,
                &manual.face_id,
                true,
                bad_evidence,
                &operator_fence,
            )
            .unwrap();

        let stale_operator_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        for mut tampered in [
            stale_operator_fence.clone(),
            stale_operator_fence.clone(),
            stale_operator_fence.clone(),
            stale_operator_fence.clone(),
            stale_operator_fence.clone(),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, mut fence)| {
            match index {
                0 => fence.face_revision += 1,
                1 => fence.person_revision += 1,
                2 => fence.identity_revision += 1,
                3 => fence.catalog_revision += 1,
                4 => fence.assignment_operation_id = Some("wrong-operation".to_string()),
                _ => unreachable!(),
            }
            fence
        }) {
            assert!(store
                .review_not_sure(&manual.face_id, &person.person_id, &tampered)
                .is_err());
        }
        person = store
            .update_person(
                &person.person_id,
                person.revision,
                "Lin Updated",
                Vec::new(),
            )
            .unwrap();
        assert!(store
            .review_different(&manual.face_id, &person.person_id, &stale_operator_fence,)
            .is_err());
        assert!(store
            .review_not_sure(&manual.face_id, &person.person_id, &stale_operator_fence)
            .is_err());
        let stale_global_fence = store
            .operator_mutation_fence(&manual.face_id, &person.person_id)
            .unwrap();
        store.create_look(&person.person_id, "Alternate").unwrap();
        assert!(store
            .move_to_look(&manual.face_id, &look.look_id, &stale_global_fence,)
            .is_err());
        assert!(store
            .authorize_trusted_reference(
                &set.set_id,
                &manual.face_id,
                true,
                TrustedEligibility {
                    provenance: "stale fixture".to_string(),
                    model_generation: "guard-model".to_string(),
                    embedding_id: embedding_id(&manual.face_id, "guard-model"),
                    policy_version: TRUSTED_POLICY_VERSION.to_string(),
                },
                &stale_global_fence,
            )
            .is_err());

        let derived_job = create_running_job(&store, "root-guard", "guard-model");
        let derived_asset = store
            .enqueue_asset(
                &derived_job.job_id,
                "media/new.jpg",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap();
        let mut noncanonical = derived_face(
            "media/new.jpg",
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            0,
        );
        noncanonical.face_id = "caller-id".to_string();
        let retry_payload = noncanonical.clone();
        let derived_fence = fence(&derived_job, &derived_asset);
        let detect_permit = stage_permit(&store, &derived_fence, JobStage::Detect);
        let derived = store
            .create_derived_face(noncanonical, &derived_fence, &detect_permit)
            .unwrap();
        assert_ne!(derived.face_id, "caller-id");
        assert_eq!(
            derived.face_id,
            derived_face_id(
                "media/new.jpg",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                0,
                MATCH_SCHEMA_GENERATION,
            )
        );
        let retry_permit = stage_permit(&store, &derived_fence, JobStage::Detect);
        let retry = store
            .create_derived_face(retry_payload, &derived_fence, &retry_permit)
            .unwrap();
        assert_eq!(retry.face_id, derived.face_id);
        let operator_fence = store
            .operator_mutation_fence(&derived.face_id, &person.person_id)
            .unwrap();
        store
            .this_is_not(&derived.face_id, &person.person_id, &operator_fence)
            .unwrap();
        let fresh_job = create_running_job(&store, "root-guard", "guard-model");
        let fresh_asset = store
            .enqueue_asset(
                &fresh_job.job_id,
                "media/new.jpg",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap();
        let suggestion = Suggestion {
            suggestion_id: suggestion_id(&derived.face_id, &person.person_id),
            face_id: derived.face_id.clone(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.99,
            model_generation: "guard-model".to_string(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: derived.media_fingerprint.clone(),
            face_revision: derived.face_revision,
            person_revision: person.revision,
            job_id: fresh_job.job_id.clone(),
            created_at: now(),
        };
        let fresh_fence = fence(&fresh_job, &fresh_asset);
        let suggestion_permit = stage_permit(&store, &fresh_fence, JobStage::Suggest);
        assert!(store
            .record_suggestion(suggestion, &fresh_fence, &suggestion_permit)
            .is_err());
        let strict_permit = stage_permit(&store, &fresh_fence, JobStage::Suggest);
        assert!(store
            .assign_face(
                &derived.face_id,
                &person.person_id,
                None,
                AssignmentState::CommittedStrictAutomatic,
                "cannot-link-fixture",
                Some("guard-model"),
                Some("cal-guard"),
                Some("envelope-guard"),
                None,
                Some(&fresh_fence),
                Some(&strict_permit),
                derived.face_revision,
                person.revision,
            )
            .is_err());

        let permit_job = create_running_job(&store, "root-permits", "guard-model");
        let duplicate_a_asset = store
            .enqueue_asset(
                &permit_job.job_id,
                "duplicates/a.jpg",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap();
        let duplicate_b_asset = store
            .enqueue_asset(
                &permit_job.job_id,
                "duplicates/b.jpg",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap();
        let duplicate_a_fence = fence(&permit_job, &duplicate_a_asset);
        let duplicate_b_fence = fence(&permit_job, &duplicate_b_asset);
        let duplicate_a_permit = stage_permit(&store, &duplicate_a_fence, JobStage::Detect);
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/b.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    0,
                ),
                &duplicate_b_fence,
                &duplicate_a_permit,
            )
            .is_err());
        let duplicate_a_write_permit = stage_permit(&store, &duplicate_a_fence, JobStage::Detect);
        let duplicate_a = store
            .create_derived_face(
                derived_face(
                    "duplicates/a.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    0,
                ),
                &duplicate_a_fence,
                &duplicate_a_write_permit,
            )
            .unwrap();
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/a.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    0,
                ),
                &duplicate_a_fence,
                &duplicate_a_write_permit,
            )
            .is_err());

        let mut duplicate_b_permit = stage_permit(&store, &duplicate_b_fence, JobStage::Detect);
        duplicate_b_permit.store_session = "wrong-session".to_string();
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/b.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    0,
                ),
                &duplicate_b_fence,
                &duplicate_b_permit,
            )
            .is_err());
        duplicate_b_permit.store_session = store.session_id.clone();
        let duplicate_b = store
            .create_derived_face(
                derived_face(
                    "duplicates/b.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                    0,
                ),
                &duplicate_b_fence,
                &duplicate_b_permit,
            )
            .unwrap();
        assert_ne!(duplicate_a.face_id, duplicate_b.face_id);

        let staged_asset = store
            .enqueue_asset(
                &permit_job.job_id,
                "duplicates/staged.jpg",
                "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            )
            .unwrap();
        let staged_fence = fence(&permit_job, &staged_asset);
        assert!(store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("zero-accounting", 1, RootKind::Local),
                &staged_fence,
                JobStage::Discover,
                ResourceRequest::default(),
            )
            .is_err());
        assert!(store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("wrong-order", 1, RootKind::Local),
                &staged_fence,
                JobStage::Embed,
                stage_request(JobStage::Embed),
            )
            .is_err());
        let staged_detect = stage_permit(&store, &staged_fence, JobStage::Detect);
        assert!(store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("wrong-order", 1, RootKind::Local),
                &staged_fence,
                JobStage::Suggest,
                stage_request(JobStage::Suggest),
            )
            .is_err());
        let staged_face = store
            .create_derived_face(
                derived_face(
                    "duplicates/staged.jpg",
                    "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                    0,
                ),
                &staged_fence,
                &staged_detect,
            )
            .unwrap();
        let undersized = store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("undersized", 1, RootKind::Local),
                &staged_fence,
                JobStage::Detect,
                ResourceRequest {
                    worker_memory_bytes: 0,
                    admitted_items: 1,
                    queued_items: 1,
                    queued_bytes: 1,
                    cpu_inference: 1,
                    decoded_bytes: 1,
                    gpu_vram_bytes: 0,
                    surreal_writes: 1,
                    vector_index_builds: 0,
                },
            )
            .unwrap();
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/staged.jpg",
                    "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                    1,
                ),
                &staged_fence,
                &undersized,
            )
            .is_err());
        drop(undersized);
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        let stale_detect = raw_stage_permit(&store, &staged_fence, JobStage::Detect);
        let detect_commit = raw_stage_permit(&store, &staged_fence, JobStage::Detect);
        store
            .commit_asset_stage(
                &staged_asset.asset_id,
                JobStage::Detect,
                &staged_fence,
                &detect_commit,
            )
            .unwrap();
        let embed_only_permit = stage_permit(&store, &staged_fence, JobStage::Embed);
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/staged.jpg",
                    "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                    0,
                ),
                &staged_fence,
                &embed_only_permit,
            )
            .is_err());
        assert!(store
            .create_derived_face(
                derived_face(
                    "duplicates/staged.jpg",
                    "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                    0,
                ),
                &staged_fence,
                &stale_detect,
            )
            .is_err());
        let fresh_embed_permit = stage_permit(&store, &staged_fence, JobStage::Embed);
        store
            .put_embedding(
                embedding(&staged_face, "guard-model", &permit_job),
                &staged_fence,
                &fresh_embed_permit,
            )
            .unwrap();

        assert_eq!(
            store
                .rekey_media(
                    "duplicates/a.jpg",
                    "duplicates/moved.jpg",
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                )
                .unwrap(),
            1
        );
        let moved_retry_payload: FaceObservation = store
            .require(FACE_TABLE, &duplicate_a.face_id, "rekeyed derived Face")
            .unwrap();
        let retry_job = create_running_job(&store, "root-rekey", "guard-model");
        let retry_asset = store
            .enqueue_asset(
                &retry_job.job_id,
                "duplicates/moved.jpg",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap();
        let retry_fence = fence(&retry_job, &retry_asset);
        let retry_detect = stage_permit(&store, &retry_fence, JobStage::Detect);
        let moved_retry = store
            .create_derived_face(moved_retry_payload, &retry_fence, &retry_detect)
            .unwrap();
        assert_eq!(moved_retry.face_id, duplicate_a.face_id);
        let status = store.status().unwrap();
        assert!(status["counts"]["faces"].as_u64().unwrap() >= 5);
        assert_eq!(asset.media_key, "media/good.jpg");
        close(&root, store);
    }

    // Test-only fixture provisioning; never opens an existing workspace database.
    fn wp087_seed_people_fixture(root: &Path, count: usize) -> Result<Value, String> {
        if !root.is_absolute() || count == 0 || count > 10_000 {
            return Err("fixture requires an absolute root and 1..=10000 People".to_string());
        }
        if root.exists() {
            let metadata = std::fs::symlink_metadata(root).map_err(|error| error.to_string())?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("fixture root must not be a reparse point".to_string());
                }
            }
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || std::fs::read_dir(root)
                    .map_err(|error| error.to_string())?
                    .next()
                    .is_some()
            {
                return Err("fixture root must be a new or empty directory".to_string());
            }
        } else {
            std::fs::create_dir(root).map_err(|error| format!("create fixture root: {error}"))?;
        }
        let root = root.canonicalize().map_err(|error| error.to_string())?;
        let generation = uuid::Uuid::new_v4().to_string();
        let store = MatchStore::open(&root)?;
        let result = (|| {
            let guard = store.mutation_write_guard("WP087 synthetic fixture")?;
            if !store.list_unlocked::<Person>(PERSON_TABLE)?.is_empty() {
                return Err("fixture canonical People table is not empty".to_string());
            }
            store.invalidate_calibrations_unlocked("person_created")?;
            let mut execution = store.execution_state_unlocked()?;
            let initial_catalog_revision = execution.catalog_revision;
            let initial_identity_revision = execution.identity_revision;
            let mut expected = Vec::with_capacity(count);
            for start in (0..count).step_by(256) {
                let mut batch = Vec::new();
                for index in start..count.min(start + 256) {
                    store.bump_revisions(&mut execution, false, true)?;
                    let timestamp = now();
                    let person = Person {
                        person_id: new_id("person"),
                        name: format!("Person{index:05}"),
                        aliases: Vec::new(),
                        cover_media_key: None,
                        hidden: false,
                        favorite: false,
                        revision: 1,
                        catalog_revision: execution.catalog_revision,
                        created_at: timestamp.clone(),
                        updated_at: timestamp,
                    };
                    batch.push((
                        PERSON_TABLE,
                        person.person_id.clone(),
                        serde_json::to_value(&person).map_err(|error| error.to_string())?,
                    ));
                    expected.push(person);
                }
                batch.push((
                    EXECUTION_TABLE,
                    "global".to_string(),
                    serde_json::to_value(&execution).map_err(|error| error.to_string())?,
                ));
                let borrowed = batch
                    .iter()
                    .map(|(table, id, value)| (*table, id.as_str(), value.clone()))
                    .collect::<Vec<_>>();
                store.transactional_upserts_deletes_unlocked(&borrowed, &[])?;
            }
            let mut canonical = store.list_unlocked::<Person>(PERSON_TABLE)?;
            canonical.sort_by(|left, right| left.name.cmp(&right.name));
            if canonical != expected {
                return Err("fixture canonical rows differ from generated rows".to_string());
            }
            let canonical_rows =
                serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
            let manifest = json!({
                "schema_version": 1,
                "fixture_kind": "wp087_synthetic_people_catalog",
                "generation_uuid": generation,
                "helper_version": "wp087_seed_people_fixture_v1",
                "source_path": "product/src/match_store.rs",
                "source_sha256": format!("{:x}", Sha256::digest(include_bytes!("match_store.rs"))),
                "match_schema_version": MATCH_SCHEMA_VERSION,
                "match_schema_generation": MATCH_SCHEMA_GENERATION,
                "expected_nonhidden_people": count,
                "canonical_people_rows_read": canonical.len(),
                "canonical_rows_sha256": format!("{:x}", Sha256::digest(&canonical_rows)),
                "canonical_rows_digest_scope": "serde_json_Vec_Person_sorted_by_name_utf8_no_newline",
                "initial_catalog_revision": initial_catalog_revision,
                "catalog_revision": execution.catalog_revision,
                "identity_revision": initial_identity_revision,
                "generated_names": "Person00000_through_Person09999_prefix_of_expected_count",
                "generated_media_count": 0,
                "generated_face_count": 0,
                "wp082_heldout_evidence": false,
                "independent_runtime_verdict": "pending_fresh_gui_observation",
            });
            drop(guard);
            store.refresh_autocomplete()?;
            let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
            if bytes.len() > 16 * 1024 {
                return Err("fixture manifest exceeds 16 KiB".to_string());
            }
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("wp087-people-fixture.json"))
                .map_err(|error| error.to_string())?;
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            Ok(manifest)
        })();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root))?;
        result
    }

    #[test]
    #[ignore = "explicit new FACIAL_WP087_FIXTURE_ROOT required; generates canonical 10000-People benchmark workspace"]
    fn wp087_generate_10000_people_fixture() {
        let root = std::env::var_os("FACIAL_WP087_FIXTURE_ROOT")
            .map(PathBuf::from)
            .expect("FACIAL_WP087_FIXTURE_ROOT must explicitly select a new or empty absolute workspace");
        wp087_seed_people_fixture(&root, 10_000).expect("generate synthetic People fixture");
    }

    #[test]
    fn wp087_people_fixture_helper_seeds_canonical_revisions_and_refuses_occupied_targets() {
        let root = workspace("wp087-fixture-helper");
        let manifest = wp087_seed_people_fixture(&root, 3).unwrap();
        assert_eq!(manifest["expected_nonhidden_people"], 3);
        assert_eq!(manifest["canonical_people_rows_read"], 3);
        assert_eq!(
            manifest["catalog_revision"].as_u64().unwrap(),
            manifest["initial_catalog_revision"].as_u64().unwrap() + 3
        );
        assert!(wp087_seed_people_fixture(&root, 3)
            .unwrap_err()
            .contains("empty directory"));
        let store = MatchStore::open(&root).unwrap();
        let page = store.catalog_snapshot(0, 1, false).unwrap();
        assert_eq!(page.total_people, 3);
        assert_eq!(page.rows[0].person.name, "Person00000");
        assert_eq!(
            page.evidence.catalog_revision,
            manifest["catalog_revision"].as_u64().unwrap()
        );
        assert_eq!(
            store.public_snapshot().unwrap()["catalog"]["total_people"],
            3
        );
        let mut canonical = store.list::<Person>(PERSON_TABLE).unwrap();
        canonical.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(
            manifest["canonical_rows_sha256"],
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&canonical).unwrap())
            )
        );
        let first = &canonical[0];
        store
            .update_person_preferences(&first.person_id, first.revision, None, true, false)
            .unwrap();
        assert_eq!(
            store.public_snapshot().unwrap()["catalog"]["total_people"],
            2
        );
        assert_eq!(store.catalog_snapshot(0, 1, true).unwrap().total_people, 3);
        close(&root, store);
    }

    #[test]
    fn wp087_catalog_evidence_counts_canonical_nonhidden_rows_and_binds_revision() {
        let root = workspace("wp087-catalog-evidence");
        let store = MatchStore::open(&root).unwrap();
        let visible = store.create_person("Visible", Vec::new()).unwrap();
        let hidden = store.create_person("Hidden", Vec::new()).unwrap();
        store
            .update_person_preferences(&hidden.person_id, hidden.revision, None, true, false)
            .unwrap();
        let page = store.catalog_snapshot(0, 1, false).unwrap();
        assert_eq!(page.total_people, 1);
        assert_eq!(page.rows[0].person.person_id, visible.person_id);
        assert_eq!(page.evidence.total_people, 1);
        assert_eq!(page.evidence.count_scope, "canonical_nonhidden_people");
        assert_eq!(page.evidence.schema_generation, MATCH_SCHEMA_GENERATION);
        assert_eq!(page.evidence.store_session_id, store.store.session_id());
        let public = store.public_snapshot().unwrap();
        assert_eq!(
            public["catalog"]["evidence"],
            serde_json::to_value(&page.evidence).unwrap()
        );
        let all = store.catalog_snapshot(0, 1, true).unwrap();
        assert_eq!(all.evidence.total_people, 2);
        assert_eq!(all.evidence.count_scope, "canonical_all_people");
        store.create_person("Another", Vec::new()).unwrap();
        let changed = store.catalog_snapshot(0, 1, false).unwrap();
        assert_eq!(changed.evidence.total_people, 2);
        assert_ne!(
            changed.evidence.catalog_revision,
            page.evidence.catalog_revision
        );
        assert_eq!(
            changed.evidence.store_session_id,
            page.evidence.store_session_id
        );
        let evidence_json = serde_json::to_string(&changed.evidence).unwrap();
        assert!(!evidence_json.contains(&visible.person_id));
        assert!(!evidence_json.contains("Visible"));
        close(&root, store);
    }

    #[test]
    fn wp083_catalog_roots_settings_and_job_controls_are_bounded_and_honest() {
        let root = workspace("wp083-operations");
        let media_root = root.join("explicit-media-root");
        std::fs::create_dir_all(media_root.join("exports")).unwrap();
        let store = MatchStore::open(&root).unwrap();

        let empty = store.catalog_snapshot(0, 32, false).unwrap();
        assert_eq!(empty.total_people, 0);
        assert!(!empty.indexing_started);
        assert!(!empty.settled);

        for index in 0..6 {
            store
                .create_person(&format!("Person {index:04}"), Vec::new())
                .unwrap();
        }
        let page = store.catalog_snapshot(2, 2, false).unwrap();
        assert_eq!(page.total_people, 6);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.offset, 2);
        assert_eq!(page.limit, 2);
        assert!(store.catalog_snapshot(0, 513, false).is_err());

        let configured = store
            .configure_index_root(&media_root, vec!["exports".to_string()])
            .unwrap();
        assert_eq!(configured.exclusions, vec!["exports"]);
        assert!(store
            .configure_index_root(&media_root, vec!["../escape".to_string()])
            .is_err());

        store
            .register_model_generation("wp083-model", true)
            .unwrap();
        let job = store
            .start_index_job(&configured.root_id, "wp083-model")
            .unwrap();
        let running = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        assert_eq!(running.lifecycle, "running");
        let paused = store.control_job(&job.job_id, "pause").unwrap();
        assert_eq!(paused.lifecycle, "pausing");
        let paused = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Paused)
            .unwrap();
        assert_eq!(paused.lifecycle, "paused");
        let resumed = store.control_job(&job.job_id, "resume").unwrap();
        assert_eq!(resumed.lifecycle, "running");
        let cancelled = store.control_job(&job.job_id, "cancel").unwrap();
        assert_eq!(cancelled.lifecycle, "cancelled");
        assert!(store.control_job(&job.job_id, "resume").is_err());
        assert!(store.control_job(&job.job_id, "retry").is_err());

        let settings = store.settings_snapshot().unwrap();
        assert_eq!(settings["materialized_people_rows"], 0);
        assert!(settings.get("catalog").is_none());
        assert!(settings.get("suggestions").is_none());
        assert_eq!(settings["roots"].as_array().unwrap().len(), 1);
        let public = store.public_snapshot().unwrap();
        assert_eq!(public["catalog"]["materialized_people_rows"], 0);
        assert_eq!(public["privacy"]["paths"], false);

        let source_path = media_root.join("Gallery-Image.JPG");
        std::fs::write(&source_path, b"wp083-source").unwrap();
        let retry_job = store
            .start_index_job(&configured.root_id, "wp083-model")
            .unwrap();
        store
            .set_job_lifecycle(&retry_job.job_id, JobLifecycle::Running)
            .unwrap();
        let source_asset = store
            .enqueue_asset_with_source_path_for_test(
                &retry_job.job_id,
                "gallery/source.jpg",
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                Some(&source_path),
            )
            .unwrap();
        let canonical_source = source_path
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            source_asset.source_path.as_deref(),
            Some(canonical_source.as_str())
        );
        let retry_fence = fence(&retry_job, &source_asset);
        let failure_permit = stage_permit(&store, &retry_fence, JobStage::Discover);
        store
            .record_asset_failure(
                &source_asset.asset_id,
                "io",
                "fixture read failed",
                &retry_fence,
                &failure_permit,
            )
            .unwrap();
        let failed_settings = store.settings_snapshot().unwrap();
        assert_eq!(
            failed_settings["failed_assets"][0]["source_path"].as_str(),
            Some(canonical_source.as_str())
        );
        store
            .set_job_lifecycle(&retry_job.job_id, JobLifecycle::Failed)
            .unwrap();
        let retrying = store.control_job(&retry_job.job_id, "retry").unwrap();
        assert_eq!(retrying.lifecycle, "retrying");
        let reset_asset = store.job_asset(&source_asset.asset_id).unwrap();
        assert_eq!(reset_asset.next_stage, "discover");
        assert!(reset_asset.failure_code.is_none());
        assert_eq!(reset_asset.source_path, source_asset.source_path);

        let gallery_person = store.create_person("Gallery Person", Vec::new()).unwrap();
        let gallery_face = store
            .create_face(face(
                "gallery-face",
                &source_asset.media_key,
                &source_asset.media_fingerprint,
                true,
            ))
            .unwrap();
        let gallery_fence = store
            .operator_mutation_fence(&gallery_face.face_id, &gallery_person.person_id)
            .unwrap();
        store
            .review_same(
                &gallery_face.face_id,
                &gallery_person.person_id,
                &gallery_fence,
            )
            .unwrap();
        store
            .update_person_preferences(
                &gallery_person.person_id,
                gallery_person.revision,
                Some(source_asset.media_key.clone()),
                false,
                true,
            )
            .unwrap();
        let cover_catalog = store.catalog_snapshot(0, 32, false).unwrap();
        let cover_row = cover_catalog
            .rows
            .iter()
            .find(|row| row.person.person_id == gallery_person.person_id)
            .unwrap();
        assert_eq!(
            cover_row.cover_source_path.as_deref(),
            Some(canonical_source.as_str())
        );
        let gallery = store
            .person_gallery(&gallery_person.person_id, 0, 32)
            .unwrap();
        assert_eq!(gallery.total_media, 1);
        assert_eq!(gallery.media_keys, vec![source_asset.media_key.clone()]);
        assert_eq!(gallery.media_paths, vec![canonical_source.clone()]);
        assert!(!json_contains(
            &store.public_snapshot().unwrap(),
            &source_path.to_string_lossy()
        ));

        let prior_catalog_revision = store.execution_state().unwrap().catalog_revision;
        store
            .rekey_media(
                &source_asset.media_key,
                "gallery/source-renamed.jpg",
                &source_asset.media_fingerprint,
            )
            .unwrap();
        let moved_person = store
            .require::<Person>(PERSON_TABLE, &gallery_person.person_id, "Person")
            .unwrap();
        assert_eq!(
            moved_person.cover_media_key.as_deref(),
            Some("gallery/source-renamed.jpg")
        );
        assert!(moved_person.revision > gallery_person.revision);
        assert!(store.execution_state().unwrap().catalog_revision > prior_catalog_revision);
        let moved_gallery = store
            .person_gallery(&gallery_person.person_id, 0, 32)
            .unwrap();
        assert_eq!(moved_gallery.media_keys, vec!["gallery/source-renamed.jpg"]);
        assert_eq!(moved_gallery.media_paths, vec![canonical_source.clone()]);
        let moved_catalog = store.catalog_snapshot(0, 32, false).unwrap();
        let moved_cover = moved_catalog
            .rows
            .iter()
            .find(|row| row.person.person_id == gallery_person.person_id)
            .unwrap();
        assert_eq!(
            moved_cover.person.cover_media_key.as_deref(),
            Some("gallery/source-renamed.jpg")
        );
        assert_eq!(
            moved_cover.cover_source_path.as_deref(),
            Some(canonical_source.as_str())
        );

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        let reopened_gallery = reopened
            .person_gallery(&gallery_person.person_id, 0, 32)
            .unwrap();
        assert_eq!(
            reopened_gallery.media_keys,
            vec!["gallery/source-renamed.jpg"]
        );
        let reopened_catalog = reopened.catalog_snapshot(0, 32, false).unwrap();
        let reopened_cover = reopened_catalog
            .rows
            .iter()
            .find(|row| row.person.person_id == gallery_person.person_id)
            .unwrap();
        assert_eq!(
            reopened_cover.person.cover_media_key.as_deref(),
            Some("gallery/source-renamed.jpg")
        );
        assert_eq!(
            reopened_cover.cover_source_path.as_deref(),
            Some(canonical_source.as_str())
        );
        close(&root, reopened);
    }

    #[test]
    fn wp083_unidentified_query_stays_correct_beyond_the_ui_materialization_bound() {
        let root = workspace("wp083-unidentified-bound");
        let store = MatchStore::open(&root).unwrap();
        let assigned_ids = (0..600)
            .map(|index| format!("assigned-face-{index:04}"))
            .collect::<Vec<_>>();
        let unassigned_id = "unassigned-face-z".to_string();
        let mut upserts = Vec::with_capacity(assigned_ids.len() * 2 + 1);

        for (index, face_id) in assigned_ids.iter().enumerate() {
            let observation = face(
                face_id,
                &format!("assigned/media-{index:04}.jpg"),
                &format!("sha256:assigned-{index:04}"),
                false,
            );
            let timestamp = now();
            let assignment = Assignment {
                assignment_id: face_id.clone(),
                face_id: face_id.clone(),
                person_id: "person-bounded-query".to_string(),
                media_key: observation.media_key.clone(),
                look_id: None,
                placement: "unsorted".to_string(),
                state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                provenance: "wp083-large-cardinality-regression".to_string(),
                locked: true,
                model_generation: None,
                calibration_generation: None,
                envelope_hash: None,
                face_revision: 1,
                person_revision: 1,
                operation_id: format!("operation-{index:04}"),
                created_at: timestamp.clone(),
                updated_at: timestamp,
            };
            upserts.push((
                FACE_TABLE,
                face_id.as_str(),
                serde_json::to_value(observation).unwrap(),
            ));
            upserts.push((
                ASSIGNMENT_TABLE,
                face_id.as_str(),
                serde_json::to_value(assignment).unwrap(),
            ));
        }
        let unassigned = face(
            &unassigned_id,
            "zzzz/unassigned-late.jpg",
            "sha256:unassigned-late",
            false,
        );
        upserts.push((
            FACE_TABLE,
            unassigned_id.as_str(),
            serde_json::to_value(unassigned).unwrap(),
        ));
        store.transactional_upserts_deletes(&upserts, &[]).unwrap();

        let snapshot = store.ui_snapshot(0, 1).unwrap();
        assert_eq!(
            snapshot["unidentified_media"],
            json!(["zzzz/unassigned-late.jpg"])
        );
        close(&root, store);
    }

    #[test]
    fn wp083_person_gallery_uses_composite_assignment_media_index_at_high_cardinality() {
        let root = workspace("wp083-gallery-composite-index");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Gallery Person", Vec::new()).unwrap();
        let noise_person = store.create_person("Noise Person", Vec::new()).unwrap();
        let mut owned = Vec::<(String, String, Value)>::new();

        let mut append_assignment =
            |face_id: String, media_key: String, owner: &Person, operation: String| {
                let observation = face(&face_id, &media_key, &format!("sha256:{face_id}"), false);
                let timestamp = now();
                let assignment = Assignment {
                    assignment_id: face_id.clone(),
                    face_id: face_id.clone(),
                    person_id: owner.person_id.clone(),
                    media_key,
                    look_id: None,
                    placement: "unsorted".to_string(),
                    state: AssignmentState::OperatorConfirmed.as_str().to_string(),
                    provenance: "wp083-gallery-cardinality".to_string(),
                    locked: true,
                    model_generation: None,
                    calibration_generation: None,
                    envelope_hash: None,
                    face_revision: 1,
                    person_revision: owner.revision,
                    operation_id: operation,
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                };
                owned.push((
                    FACE_TABLE.to_string(),
                    face_id.clone(),
                    serde_json::to_value(observation).unwrap(),
                ));
                owned.push((
                    ASSIGNMENT_TABLE.to_string(),
                    face_id,
                    serde_json::to_value(assignment).unwrap(),
                ));
            };
        for index in 0..600 {
            append_assignment(
                format!("gallery-face-{index:04}"),
                format!("gallery/media-{index:04}.jpg"),
                &person,
                format!("gallery-operation-{index:04}"),
            );
        }
        for index in 0..100 {
            append_assignment(
                format!("gallery-duplicate-{index:04}"),
                format!("gallery/media-{index:04}.jpg"),
                &person,
                format!("gallery-duplicate-operation-{index:04}"),
            );
        }
        for index in 0..30 {
            append_assignment(
                format!("noise-face-{index:04}"),
                format!("gallery/media-{index:04}.jpg"),
                &noise_person,
                format!("noise-operation-{index:04}"),
            );
        }
        let refs = owned
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&refs, &[]).unwrap();

        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        store
            .register_model_generation("wp083-gallery-model", true)
            .unwrap();
        let job = create_running_job(&store, &configured.root_id, "wp083-gallery-model");
        let first_path = media_root.join("page-first.jpg");
        let last_path = media_root.join("page-last.jpg");
        std::fs::write(&first_path, b"first").unwrap();
        std::fs::write(&last_path, b"last").unwrap();
        store
            .enqueue_asset_with_source_path_for_test(
                &job.job_id,
                "gallery/media-0511.jpg",
                "sha256:gallery-page-first",
                Some(&first_path),
            )
            .unwrap();

        let unrelated_assets = (0..1_000)
            .map(|index| {
                let media_key = format!("unrelated/media-{index:04}.jpg");
                let asset = JobAsset {
                    asset_id: format!("unrelated-asset-{index:04}"),
                    job_id: job.job_id.clone(),
                    media_key,
                    source_path: Some(format!("unrelated/source-{index:04}.jpg")),
                    media_fingerprint: format!("sha256:unrelated-{index:04}"),
                    next_stage: JobStage::Discover.as_str().to_string(),
                    completed_stages: Vec::new(),
                    failure_code: None,
                    failure_message: None,
                    skipped_code: None,
                    skipped_message: None,
                    schema_generation: job.schema_generation.clone(),
                    model_generation: job.model_generation.clone(),
                    identity_revision: job.identity_revision,
                    catalog_revision: job.catalog_revision,
                    updated_at: now(),
                };
                (
                    JOB_ASSET_TABLE.to_string(),
                    asset.asset_id.clone(),
                    serde_json::to_value(asset).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let unrelated_refs = unrelated_assets
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store
            .transactional_upserts_deletes(&unrelated_refs, &[])
            .unwrap();
        store
            .enqueue_asset_with_source_path_for_test(
                &job.job_id,
                "gallery/media-0574.jpg",
                "sha256:gallery-page-last",
                Some(&last_path),
            )
            .unwrap();

        let gallery = store.person_gallery(&person.person_id, 511, 64).unwrap();
        assert_eq!(gallery.total_media, 600);
        assert_eq!(gallery.media_keys.len(), 64);
        assert_eq!(
            gallery.media_keys.first().unwrap(),
            "gallery/media-0511.jpg"
        );
        assert_eq!(gallery.media_keys.last().unwrap(), "gallery/media-0574.jpg");
        assert_eq!(
            gallery.media_paths,
            vec![
                first_path
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                last_path
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            ]
        );

        let db = store.store.db();
        let person_id = person.person_id.clone();
        let plan: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query("SELECT VALUE media_key FROM match_assignment WITH INDEX match_assignment_person WHERE person_id = $person_id GROUP BY media_key ORDER BY media_key ASC LIMIT 64 START 511 EXPLAIN FULL;")
                .bind(("person_id", person_id))
                .await
                .map_err(|error| format!("explain Match Person gallery index: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match Person gallery plan: {error}"))
        })
        .unwrap();
        assert!(serde_json::to_string(&plan)
            .unwrap()
            .contains("match_assignment_person"));
        let db = store.store.db();
        let page_keys = vec![
            "gallery/media-0511.jpg".to_string(),
            "gallery/media-0574.jpg".to_string(),
        ];
        let source_plan: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query("SELECT id, media_key FROM match_job_asset WITH INDEX match_job_asset_media WHERE media_key IN $page_keys GROUP BY media_key ORDER BY media_key ASC LIMIT 512 EXPLAIN FULL;")
                .bind(("page_keys", page_keys))
                .await
                .map_err(|error| format!("explain Match gallery source index: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match gallery source plan: {error}"))
        })
        .unwrap();
        assert!(serde_json::to_string(&source_plan)
            .unwrap()
            .contains("match_job_asset_media"));
        close(&root, store);
    }

    #[test]
    fn restored_face_regenerates_embedding_in_later_job_without_rewriting_evidence() {
        let root = workspace("wp085-restored-face-regeneration");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("regeneration-model", true)
            .unwrap();
        let mut persisted = derived_face("restored/face.jpg", &"a".repeat(64), 0);
        let earlier = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
        persisted.created_at = earlier.clone();
        persisted.updated_at = earlier;
        persisted.source_width = Some(1920);
        persisted.source_height = Some(1080);
        persisted.exif_orientation = Some(1);
        // Restore preserves this observation while intentionally excluding vectors.
        store
            .upsert_json(FACE_TABLE, &persisted.face_id, &persisted)
            .unwrap();
        let job = store
            .create_job("regeneration-root", "regeneration-model")
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let asset = store
            .enqueue_asset(
                &job.job_id,
                &persisted.media_key,
                &persisted.media_fingerprint,
            )
            .unwrap();
        let revision_fence = fence(&job, &asset);
        advance_to(&store, &revision_fence, &asset.asset_id, JobStage::Detect);
        let publish = |observation: FaceObservation| {
            let detect = store
                .acquire_background_stage(
                    &MediaIoCoordinator::new(),
                    RootIdentity::new("match-test", 1, RootKind::Local),
                    &revision_fence,
                    JobStage::Detect,
                    ResourceRequest {
                        queued_bytes: 64 * 1024 * 1024,
                        ..stage_request(JobStage::Detect)
                    },
                )
                .unwrap();
            let align = store
                .acquire_fused_inference_audit_stage(
                    &revision_fence,
                    JobStage::Align,
                    64 * 1024 * 1024,
                )
                .unwrap();
            let embed = store
                .acquire_fused_inference_audit_stage(
                    &revision_fence,
                    JobStage::Embed,
                    64 * 1024 * 1024,
                )
                .unwrap();
            let vector = embedding(&observation, &job.model_generation, &job);
            store.commit_fused_identity_inference(
                vec![observation],
                vec![vector],
                &revision_fence,
                &detect,
                &align,
                &embed,
            )
        };
        let mut observed = persisted.clone();
        observed.created_at = now();
        observed.updated_at = observed.created_at.clone();
        let mut changed_geometry = observed.clone();
        changed_geometry.source_width = Some(1919);
        assert!(publish(changed_geometry)
            .unwrap_err()
            .contains("canonical fused face retry"));
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        assert_eq!(
            store.job_asset(&asset.asset_id).unwrap().next_stage,
            "detect"
        );
        let committed = publish(observed.clone()).unwrap();
        assert_eq!(committed, vec![persisted.clone()]);
        assert_eq!(
            store
                .require::<FaceObservation>(FACE_TABLE, &persisted.face_id, "restored Face")
                .unwrap(),
            persisted
        );
        let regenerated: FaceEmbedding = store
            .require(
                EMBEDDING_TABLE,
                &embedding_id(&persisted.face_id, &job.model_generation),
                "regenerated embedding",
            )
            .unwrap();
        assert_eq!(regenerated.job_id, job.job_id);
        assert_eq!(
            store.job_asset(&asset.asset_id).unwrap().next_stage,
            "persist"
        );
        // Even a forced Detect replay cannot turn this exception into same-job
        // timestamp mutability after that job owns the persisted embedding.
        let mut replay_asset = store.job_asset(&asset.asset_id).unwrap();
        replay_asset.next_stage = "detect".to_string();
        replay_asset.completed_stages = vec!["discover".to_string()];
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &replay_asset)
            .unwrap();
        observed.created_at = now();
        observed.updated_at = observed.created_at.clone();
        assert!(publish(observed)
            .unwrap_err()
            .contains("canonical fused face retry"));
        assert_eq!(
            store
                .require::<FaceObservation>(FACE_TABLE, &persisted.face_id, "restored Face")
                .unwrap(),
            persisted
        );
        assert_eq!(
            store
                .require::<FaceEmbedding>(
                    EMBEDDING_TABLE,
                    &regenerated.embedding_id,
                    "regenerated embedding"
                )
                .unwrap(),
            regenerated
        );
        close(&root, store);
    }

    fn persist_fixture(name: &str) -> (PathBuf, MatchStore, RevisionFence, PeopleProjection) {
        let root = workspace(name);
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("persist-model", true)
            .unwrap();
        store.activate_model_generation("persist-model").unwrap();
        let job = create_running_job(&store, "persist-root", "persist-model");
        let asset = store
            .enqueue_asset(&job.job_id, "persist/image.jpg", &"a".repeat(64))
            .unwrap();
        let revision_fence = fence(&job, &asset);
        advance_to(&store, &revision_fence, &asset.asset_id, JobStage::Persist);
        let projection = PeopleProjection {
            media_key: asset.media_key,
            media_fingerprint: asset.media_fingerprint,
            schema_generation: job.schema_generation,
            model_generation: job.model_generation,
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
            person_ids: Vec::new(),
            published_at: now(),
        };
        (root, store, revision_fence, projection)
    }

    #[test]
    fn database_failed_owner_diagnostics_remain_available_without_canonical_reads() {
        let (root, store, revision_fence, _) = persist_fixture("database-failure-diagnostics");
        let recent_jobs_failure = store
            .database_diagnostic_result(Err(
                "query recent Match jobs: safe_unit_timeout: database owner execution deadline"
                    .into(),
            ))
            .unwrap();
        assert_eq!(recent_jobs_failure["availability"], "unavailable");
        assert_eq!(
            recent_jobs_failure["canonical_state"]["error_code"],
            "safe_unit_timeout"
        );
        assert_eq!(
            recent_jobs_failure["execution"]["database_owner"]["phase"],
            "ready"
        );
        for invalid_mode in [
            "invalid-diagnostic-mode",
            "safe_unit_timeout",
            "database_owner_exited",
            "bad: safe_unit_timeout",
            "foo; database_owner_exited",
        ] {
            let db = store.database();
            let mode = invalid_mode.to_string();
            surreal_store::run(async move {
                db.query("UPDATE match_execution:global SET desired_mode = $mode;")
                    .bind(("mode", mode))
                    .await?
                    .check()?;
                Ok(())
            })
            .unwrap();
            for error in [
                store.status().unwrap_err(),
                store.public_snapshot().unwrap_err(),
            ] {
                assert!(
                    error.contains(&format!("unknown Match desired mode {invalid_mode}")),
                    "{error}"
                );
            }
        }
        let db = store.database();
        surreal_store::run(async move {
            db.query("UPDATE match_execution:global SET desired_mode = 'operator_paused';")
                .await?
                .check()?;
            Ok(())
        })
        .unwrap();
        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        store.external_holds.set_playback(true);
        let scope = store
            .store
            .begin_match_unit(std::time::Instant::now() + crate::match_worker::SAFE_UNIT_LIMIT)
            .unwrap();
        let db = store.database();
        let error = surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; RETURN 1; COMMIT TRANSACTION;")
                .test_delay_reply_after_commit(std::time::Duration::from_secs(5))
                .await
        })
        .err()
        .expect("lost acknowledgement must fail");
        assert!(error.contains("commit_outcome_unknown"), "{error}");
        drop(scope);
        store.record_pending_database_failure(&revision_fence.job_id, "safe_unit_timeout", &error);
        store
            .add_hold(HoldReason::DatabaseOwnerQuarantined)
            .unwrap();
        let read_blocker = store.store.transaction_lock().write().unwrap();
        let started = std::time::Instant::now();
        for snapshot in [store.status().unwrap(), store.public_snapshot().unwrap()] {
            assert_eq!(snapshot["availability"], "unavailable");
            assert!(snapshot["execution"]["desired_mode"].is_null());
            assert_eq!(
                snapshot["execution"]["failure_recording"],
                "pending_recovery"
            );
            assert!(snapshot["execution"]["transient_holds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hold| hold == "viewer_playback"));
            let pending = &snapshot["execution"]["pending_failure_records"][0];
            assert_eq!(pending["recording_acknowledged"], false);
            assert_eq!(pending["recording_error_code"], "commit_outcome_unknown");
            assert_eq!(pending["operation_id"].as_str().unwrap().len(), 32);
            assert!(!snapshot.to_string().contains("BEGIN TRANSACTION"));
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        drop(read_blocker);
        store
            .remove_hold(HoldReason::DatabaseOwnerQuarantined)
            .unwrap();
        assert_eq!(store.pending_database_failure_snapshot().unwrap().len(), 1);
        store
            .add_hold(HoldReason::DatabaseOwnerQuarantined)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match store.control_job(&revision_fence.job_id, "resume") {
                Ok(_) => break,
                Err(error)
                    if error.contains("database_owner_exit_pending")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("explicit recovery failed: {error}"),
            }
        }
        assert!(store
            .pending_database_failure_snapshot()
            .unwrap()
            .is_empty());
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert!(store
            .holds()
            .unwrap()
            .iter()
            .any(|hold| hold == "viewer_playback"));
        let expired_scope = store
            .store
            .begin_match_unit(std::time::Instant::now() + std::time::Duration::from_millis(5))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        for snapshot in [store.status().unwrap(), store.public_snapshot().unwrap()] {
            assert_eq!(snapshot["availability"], "unavailable");
            assert_eq!(
                snapshot["canonical_state"]["error_code"],
                "safe_unit_timeout"
            );
            assert_eq!(snapshot["execution"]["database_owner"]["phase"], "ready");
            assert!(snapshot["execution"]["desired_mode"].is_null());
        }
        drop(expired_scope);
        for index in 0..20 {
            store.record_pending_database_failure(
                &format!("bounded-{index}"),
                "private-person-name",
                "private/path SQL",
            );
        }
        let pending = store.pending_database_failure_snapshot().unwrap();
        assert_eq!(pending.len(), 16);
        assert_eq!(
            store
                .pending_database_failures_evicted
                .load(Ordering::Acquire),
            4
        );
        assert!(!serde_json::to_string(&pending).unwrap().contains("private"));
        store.resolve_pending_database_failures(None).unwrap();
        close(&root, store);
    }

    #[test]
    fn database_stage_and_failure_share_original_admission_deadline() {
        let (root, store, revision_fence, _) = persist_fixture("database-stage-original-deadline");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let before = store.job_asset(&asset_id).unwrap();
        let mut stage = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        stage.admitted_at = std::time::Instant::now() - crate::match_worker::SAFE_UNIT_LIMIT;
        let error = store
            .commit_asset_stage(&asset_id, JobStage::Persist, &revision_fence, &stage)
            .unwrap_err();
        assert!(error.contains("safe_unit_timeout"), "{error}");
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        let mut failure = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        failure.admitted_at = std::time::Instant::now() - crate::match_worker::SAFE_UNIT_LIMIT;
        let error = store
            .record_asset_failure(&asset_id, "persist", "fixture", &revision_fence, &failure)
            .unwrap_err();
        assert!(error.contains("safe_unit_timeout"), "{error}");
        assert_eq!(store.job_asset(&asset_id).unwrap(), before);
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn database_preacquired_stage_rejects_new_execution_after_external_hold() {
        let (root, store, revision_fence, _) = persist_fixture("database-stage-hold-admission");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let before = store.job_asset(&asset_id).unwrap();
        let stage = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store.external_holds.set_playback(true);
        let error = store
            .commit_asset_stage(&asset_id, JobStage::Persist, &revision_fence, &stage)
            .unwrap_err();
        assert!(error.contains("admission changed"), "{error}");
        assert_eq!(store.job_asset(&asset_id).unwrap(), before);
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        store.external_holds.set_playback(false);
        close(&root, store);
    }

    #[test]
    fn database_discovery_hold_blocks_new_execution_and_allows_observed_checkpoint() {
        let (root, store, revision_fence, _) =
            persist_fixture("database-discovery-hold-checkpoint");
        let request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 64 * 1024,
            surreal_writes: 1,
            ..ResourceRequest::default()
        };
        let unstarted = store
            .acquire_discovery_write(&revision_fence.job_id, "unstarted/image.jpg", request)
            .unwrap();
        let observation = store
            .begin_discovery_observation(&revision_fence.job_id)
            .unwrap();
        store.external_holds.set_playback(true);
        let error = store
            .enqueue_asset_with_source_path(
                &revision_fence.job_id,
                "unstarted/image.jpg",
                "sha256:unstarted",
                None,
                &unstarted,
            )
            .unwrap_err();
        assert!(error.contains("admission changed"), "{error}");
        let checkpoint = store
            .acquire_discovery_write_for_observation(
                &revision_fence.job_id,
                "settled/image.jpg",
                request,
                &observation,
            )
            .unwrap();
        let settled = store
            .enqueue_asset_with_source_path(
                &revision_fence.job_id,
                "settled/image.jpg",
                "sha256:settled",
                None,
                &checkpoint,
            )
            .unwrap();
        assert_eq!(settled.media_key, "settled/image.jpg");
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        assert!(store
            .get_one::<JobAsset>(
                JOB_ASSET_TABLE,
                &job_asset_id(&revision_fence.job_id, "unstarted/image.jpg")
            )
            .unwrap()
            .is_none());
        store.external_holds.set_playback(false);
        close(&root, store);
    }

    #[test]
    fn database_explicit_recovery_preserves_operator_pause_and_other_holds() {
        let (root, store, revision_fence, _) = persist_fixture("database-owner-explicit-recovery");
        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        store
            .add_hold(HoldReason::DatabaseOwnerQuarantined)
            .unwrap();
        store.add_hold(HoldReason::PowerSaver).unwrap();
        assert!(!store.can_attempt_automatic(JobLifecycle::Running).unwrap());
        let current = store.control_job(&revision_fence.job_id, "resume").unwrap();
        assert_eq!(current.lifecycle().unwrap(), JobLifecycle::Running);
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert_eq!(store.holds().unwrap(), vec!["power_saver"]);
        assert!(!store.can_attempt_automatic(JobLifecycle::Running).unwrap());
        close(&root, store);
    }

    #[test]
    fn database_automatic_write_blocks_filesystem_recovery_without_running_it() {
        let (root, store, revision_fence, _) = persist_fixture("database-stage-recovery-gate");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let before = store.job_asset(&asset_id).unwrap();
        let stage = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store
            .filesystem_recovery_ready
            .store(false, Ordering::Release);
        let error = store
            .commit_asset_stage(&asset_id, JobStage::Persist, &revision_fence, &stage)
            .unwrap_err();
        assert!(error.contains("filesystem recovery is required"), "{error}");
        assert!(!store.filesystem_recovery_ready.load(Ordering::Acquire));
        assert_eq!(store.job_asset(&asset_id).unwrap(), before);
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn persist_projection_and_cursor_rollback_together_then_fresh_retry_commits() {
        let (root, store, revision_fence, projection) = persist_fixture("persist-atomic-rollback");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let failed = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store
            .record_asset_failure(&asset_id, "persist", "fixture", &revision_fence, &failed)
            .unwrap();
        let before_asset = store.job_asset(&asset_id).unwrap();
        let before_job: IndexJob = store
            .require(JOB_TABLE, &revision_fence.job_id, "job")
            .unwrap();
        // The engine accepts the projection UPSERT first, then rejects the
        // cursor write. Canonical reads must prove the whole transaction rolled back.
        surreal_store::run(async {
            store.store.db().query(
                "DEFINE FIELD OVERWRITE next_stage ON TABLE match_job_asset TYPE string ASSERT $value != 'suggest';"
            ).await.map_err(|error| error.to_string())?.check().map_err(|error| error.to_string())?;
            Ok(())
        }).unwrap();
        let rejected = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        assert!(store
            .publish_projection(projection.clone(), &revision_fence, &rejected)
            .is_err());
        assert_eq!(store.job_asset(&asset_id).unwrap(), before_asset);
        assert_eq!(
            store
                .require::<IndexJob>(JOB_TABLE, &revision_fence.job_id, "job")
                .unwrap(),
            before_job
        );
        assert!(store
            .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
            .unwrap()
            .is_none());
        assert!(store
            .cached_projection(&projection.media_key)
            .unwrap()
            .is_none());
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        assert!(rejected.io.lock().unwrap().is_none());
        surreal_store::run(async {
            store
                .store
                .db()
                .query("DEFINE FIELD OVERWRITE next_stage ON TABLE match_job_asset TYPE string;")
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();
        let retry = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store
            .publish_projection(projection.clone(), &revision_fence, &retry)
            .unwrap();
        let committed = store.job_asset(&asset_id).unwrap();
        assert_eq!(committed.next_stage, JobStage::Suggest.as_str());
        assert!(committed.failure_code.is_none());
        assert_eq!(
            store
                .require::<IndexJob>(JOB_TABLE, &revision_fence.job_id, "job")
                .unwrap()
                .failed,
            0
        );
        assert_eq!(
            store
                .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
                .unwrap(),
            Some(projection)
        );
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn persist_lost_ack_reconciles_canonical_cursor_without_replaying_checkpoint() {
        let (root, store, revision_fence, projection) = persist_fixture("persist-lost-ack-cursor");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let before = store.job_asset(&asset_id).unwrap();
        assert_eq!(before.next_stage, JobStage::Persist.as_str());
        assert!(store
            .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
            .unwrap()
            .is_none());

        let permit = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store.test_delay_next_checkpoint_ack(std::time::Duration::from_secs(5));
        let started = std::time::Instant::now();
        let error = store
            .publish_projection(projection.clone(), &revision_fence, &permit)
            .unwrap_err();
        assert!(error.contains("commit_outcome_unknown"), "{error}");
        assert!(
            started.elapsed() <= crate::match_worker::SAFE_UNIT_LIMIT,
            "Persist ACK-loss response exceeded the original safe unit"
        );
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        let blocked = store
            .store
            .begin_match_unit(std::time::Instant::now() + crate::match_worker::SAFE_UNIT_LIMIT);
        assert!(blocked
            .as_ref()
            .err()
            .is_some_and(|reason| reason.contains("commit_outcome_unknown")));

        let reconcile_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match store.reconcile_database_operations() {
                Ok(()) => break,
                Err(error)
                    if error.contains("database_owner_exit_pending")
                        && std::time::Instant::now() < reconcile_deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("Persist receipt reconciliation failed: {error}"),
            }
        }
        let committed = store.job_asset(&asset_id).unwrap();
        assert_eq!(committed.next_stage, JobStage::Suggest.as_str());
        assert_eq!(
            committed
                .completed_stages
                .iter()
                .filter(|stage| stage.as_str() == JobStage::Persist.as_str())
                .count(),
            1,
            "lost ACK duplicated the durable Persist cursor"
        );
        assert_eq!(
            store
                .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
                .unwrap(),
            Some(projection.clone())
        );

        let retry = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        let retry_error = store
            .publish_projection(projection.clone(), &revision_fence, &retry)
            .unwrap_err();
        assert!(
            retry_error.contains("durable stage cursor"),
            "{retry_error}"
        );
        assert_eq!(store.job_asset(&asset_id).unwrap(), committed);
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(reopened.job_asset(&asset_id).unwrap(), committed);
        assert_eq!(
            reopened
                .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
                .unwrap(),
            Some(projection)
        );
        close(&root, reopened);
    }

    #[test]
    fn persist_poisoned_cache_preserves_committed_outcome_and_restores_projection() {
        let (root, store, revision_fence, projection) = persist_fixture("persist-poisoned-cache");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let permit = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        poison_match_caches(&store);
        assert!(store.caches.is_poisoned());
        store
            .publish_projection(projection.clone(), &revision_fence, &permit)
            .unwrap();
        assert_eq!(
            store.job_asset(&asset_id).unwrap().next_stage,
            JobStage::Suggest.as_str()
        );
        assert_eq!(
            store
                .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
                .unwrap(),
            Some(projection.clone())
        );
        assert_eq!(
            store.cached_projection(&projection.media_key).unwrap(),
            Some(projection)
        );
        assert!(!store.caches.is_poisoned());
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        assert!(permit.io.lock().unwrap().is_none());
        close(&root, store);
    }

    #[test]
    fn persist_lock_deadline_leaves_no_projection_or_cursor_and_releases_resources() {
        let (root, store, revision_fence, projection) = persist_fixture("persist-lock-deadline");
        let asset_id = job_asset_id(&revision_fence.job_id, &revision_fence.media_key);
        let before = store.job_asset(&asset_id).unwrap();
        let permit = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        let held = store.store.transaction_lock().write().unwrap();
        let error = store
            .publish_projection(projection.clone(), &revision_fence, &permit)
            .unwrap_err();
        assert!(error.starts_with("safe_unit_timeout:"), "{error}");
        assert!(permit.admitted_at.elapsed() >= crate::match_worker::SAFE_UNIT_LIMIT);
        assert!(permit.admitted_at.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        assert!(permit.io.lock().unwrap().is_none());
        drop(held);
        assert_eq!(store.job_asset(&asset_id).unwrap(), before);
        assert!(store
            .get_one::<PeopleProjection>(PROJECTION_TABLE, &projection.media_key)
            .unwrap()
            .is_none());
        assert!(store
            .cached_projection(&projection.media_key)
            .unwrap()
            .is_none());
        let fresh = raw_stage_permit(&store, &revision_fence, JobStage::Persist);
        store
            .publish_projection(projection, &revision_fence, &fresh)
            .unwrap();
        assert_eq!(
            store.job_asset(&asset_id).unwrap().next_stage,
            JobStage::Suggest.as_str()
        );
        assert_eq!(store.governor.usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn fused_identity_commit_is_atomic_and_restart_resumes_at_persist() {
        let root = workspace("wp083-fused-identity-commit");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp083-fused-model", true)
            .unwrap();
        let job = store.create_job("fused-root", "wp083-fused-model").unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let asset = store
            .enqueue_asset(&job.job_id, "fused/image.jpg", "sha256:fused")
            .unwrap();
        let revision_fence = fence(&job, &asset);
        advance_to(&store, &revision_fence, &asset.asset_id, JobStage::Detect);
        let detect = store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("match-test", 1, RootKind::Local),
                &revision_fence,
                JobStage::Detect,
                ResourceRequest {
                    queued_bytes: 64 * 1024 * 1024,
                    ..stage_request(JobStage::Detect)
                },
            )
            .unwrap();
        let align = store
            .acquire_fused_inference_audit_stage(&revision_fence, JobStage::Align, 64 * 1024 * 1024)
            .unwrap();
        let embed = store
            .acquire_fused_inference_audit_stage(&revision_fence, JobStage::Embed, 64 * 1024 * 1024)
            .unwrap();
        let observation = derived_face(&asset.media_key, &asset.media_fingerprint, 0);
        let face_embedding = embedding(&observation, &job.model_generation, &job);
        let committed = store
            .commit_fused_identity_inference(
                vec![observation.clone()],
                vec![face_embedding],
                &revision_fence,
                &detect,
                &align,
                &embed,
            )
            .unwrap();
        assert_eq!(committed.len(), 1);
        let advanced = store.job_asset(&asset.asset_id).unwrap();
        assert_eq!(advanced.next_stage, JobStage::Persist.as_str());
        assert_eq!(
            advanced.completed_stages,
            vec!["discover", "detect", "align", "embed"]
        );
        assert_eq!(store.count(FACE_TABLE).unwrap(), 1);
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 1);
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let resumed = reopened.job_asset(&asset.asset_id).unwrap();
        assert_eq!(resumed.next_stage, JobStage::Persist.as_str());
        assert_eq!(reopened.count(FACE_TABLE).unwrap(), 1);
        assert_eq!(reopened.count(EMBEDDING_TABLE).unwrap(), 1);
        close(&root, reopened);
    }

    #[test]
    fn fused_duplicate_batch_rejects_without_rows_or_cursor_advance() {
        let root = workspace("wp085-fused-duplicate-atomic-reject");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp085-fused-duplicate-model", true)
            .unwrap();
        let job = store
            .create_job("fused-duplicate-root", "wp085-fused-duplicate-model")
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let asset = store
            .enqueue_asset(
                &job.job_id,
                "fused/duplicate.jpg",
                &format!("sha256:{}", "f".repeat(64)),
            )
            .unwrap();
        let revision_fence = fence(&job, &asset);
        advance_to(&store, &revision_fence, &asset.asset_id, JobStage::Detect);
        let detect = store
            .acquire_background_stage(
                &MediaIoCoordinator::new(),
                RootIdentity::new("match-test", 1, RootKind::Local),
                &revision_fence,
                JobStage::Detect,
                ResourceRequest {
                    queued_bytes: 64 * 1024 * 1024,
                    ..stage_request(JobStage::Detect)
                },
            )
            .unwrap();
        let align = store
            .acquire_fused_inference_audit_stage(&revision_fence, JobStage::Align, 64 * 1024 * 1024)
            .unwrap();
        let embed = store
            .acquire_fused_inference_audit_stage(&revision_fence, JobStage::Embed, 64 * 1024 * 1024)
            .unwrap();
        let observation = derived_face(&asset.media_key, &asset.media_fingerprint, 0);
        let face_embedding = embedding(&observation, &job.model_generation, &job);
        let error = store
            .commit_fused_identity_inference(
                vec![observation.clone(), observation],
                vec![face_embedding.clone(), face_embedding],
                &revision_fence,
                &detect,
                &align,
                &embed,
            )
            .unwrap_err();
        assert!(error.contains("duplicate canonical face_id"));
        let unchanged = store.job_asset(&asset.asset_id).unwrap();
        assert_eq!(unchanged.next_stage, JobStage::Detect.as_str());
        assert!(!unchanged
            .completed_stages
            .iter()
            .any(|stage| stage == JobStage::Detect.as_str()));
        assert_eq!(store.count(FACE_TABLE).unwrap(), 0);
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn restart_preserves_per_job_pause_intent_under_both_desired_modes() {
        let root = workspace("wp083-restart-pause-intent");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp083-restart-model", true)
            .unwrap();

        let running = store
            .create_job("running-mode-root", "wp083-restart-model")
            .unwrap();
        store
            .set_job_lifecycle(&running.job_id, JobLifecycle::Running)
            .unwrap();
        let pausing = store
            .create_job("pausing-mode-root", "wp083-restart-model")
            .unwrap();
        store
            .set_job_lifecycle(&pausing.job_id, JobLifecycle::Running)
            .unwrap();
        store.control_job(&pausing.job_id, "pause").unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let recovered_running: IndexJob = reopened
            .require(JOB_TABLE, &running.job_id, "IndexJob")
            .unwrap();
        let recovered_pausing: IndexJob = reopened
            .require(JOB_TABLE, &pausing.job_id, "IndexJob")
            .unwrap();
        assert_eq!(
            recovered_running.lifecycle().unwrap(),
            JobLifecycle::Retrying
        );
        assert_eq!(recovered_pausing.lifecycle().unwrap(), JobLifecycle::Paused);

        let globally_paused_running = reopened
            .create_job("globally-paused-running-root", "wp083-restart-model")
            .unwrap();
        reopened
            .set_job_lifecycle(&globally_paused_running.job_id, JobLifecycle::Running)
            .unwrap();
        let globally_paused_pausing = reopened
            .create_job("globally-paused-pausing-root", "wp083-restart-model")
            .unwrap();
        reopened
            .set_job_lifecycle(&globally_paused_pausing.job_id, JobLifecycle::Running)
            .unwrap();
        reopened
            .control_job(&globally_paused_pausing.job_id, "pause")
            .unwrap();
        reopened
            .set_desired_mode(DesiredMode::OperatorPaused)
            .unwrap();
        drop(reopened);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        for job_id in [
            globally_paused_running.job_id,
            globally_paused_pausing.job_id,
        ] {
            let recovered: IndexJob = reopened.require(JOB_TABLE, &job_id, "IndexJob").unwrap();
            assert_eq!(recovered.lifecycle().unwrap(), JobLifecycle::Paused);
        }
        close(&root, reopened);
    }

    #[test]
    fn discovery_commits_accept_pausing_and_retry_retains_unresolved_placeholders() {
        let root = workspace("wp083-discovery-pause-retry");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let success_path = media_root.join("success.jpg");
        let failure_path = media_root.join("failure.jpg");
        std::fs::write(&success_path, b"success").unwrap();
        std::fs::write(&failure_path, b"failure").unwrap();
        let store = MatchStore::open(&root).unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        store
            .register_model_generation("wp083-discovery-model", true)
            .unwrap();

        let pausing = create_running_job(&store, &configured.root_id, "wp083-discovery-model");
        let success_permit = store
            .acquire_discovery_write(
                &pausing.job_id,
                "pause/success.jpg",
                ResourceRequest {
                    admitted_items: 1,
                    queued_items: 1,
                    queued_bytes: 64 * 1024,
                    surreal_writes: 1,
                    ..ResourceRequest::default()
                },
            )
            .unwrap();
        let failure_permit = store
            .acquire_discovery_write(
                &pausing.job_id,
                "pause/failure.jpg",
                ResourceRequest {
                    admitted_items: 1,
                    queued_items: 1,
                    queued_bytes: 64 * 1024,
                    surreal_writes: 1,
                    ..ResourceRequest::default()
                },
            )
            .unwrap();
        store.control_job(&pausing.job_id, "pause").unwrap();
        store
            .enqueue_asset_with_source_path(
                &pausing.job_id,
                "pause/success.jpg",
                "sha256:pause-success",
                Some(&success_path),
                &success_permit,
            )
            .unwrap();
        store
            .record_discovery_failure(
                &pausing.job_id,
                "pause/failure.jpg",
                Some(&failure_path),
                "io",
                "in-flight discovery failure while pause is pending",
                &failure_permit,
            )
            .unwrap();
        assert!(store
            .acquire_discovery_write(
                &pausing.job_id,
                "pause/late.jpg",
                ResourceRequest {
                    admitted_items: 1,
                    queued_items: 1,
                    queued_bytes: 64 * 1024,
                    surreal_writes: 1,
                    ..ResourceRequest::default()
                },
            )
            .is_err());
        let durable = store.job(&pausing.job_id).unwrap();
        assert_eq!(durable.lifecycle().unwrap(), JobLifecycle::Pausing);
        assert_eq!(durable.discovered, 2);
        assert_eq!(durable.failed, 1);
        let paused = store
            .set_job_lifecycle(&pausing.job_id, JobLifecycle::Paused)
            .unwrap();
        assert_eq!(paused.lifecycle().unwrap(), JobLifecycle::Paused);
        assert!(paused.failure_code.is_none());
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());

        let unresolved = create_running_job(&store, &configured.root_id, "wp083-discovery-model");
        let placeholder = store
            .record_discovery_failure_for_test(
                &unresolved.job_id,
                "retry/vanished.jpg",
                None,
                "io",
                "source vanished before fingerprint",
            )
            .unwrap();
        store
            .set_job_lifecycle(&unresolved.job_id, JobLifecycle::Failed)
            .unwrap();
        let retrying = store.control_job(&unresolved.job_id, "retry").unwrap();
        assert_eq!(retrying.lifecycle().unwrap(), JobLifecycle::Retrying);
        assert_eq!(retrying.failed, 1);
        assert_eq!(
            store
                .job_asset(&placeholder.asset_id)
                .unwrap()
                .failure_code
                .as_deref(),
            Some("io")
        );
        let running = store
            .set_job_lifecycle(&unresolved.job_id, JobLifecycle::Running)
            .unwrap();
        assert_eq!(running.failed, 1);
        assert_ne!(running.lifecycle().unwrap(), JobLifecycle::Completed);

        // Positive rediscovery replaces the unavailable fingerprint and
        // clears the retained failure/count atomically.
        let recovered_path = media_root.join("recovered.jpg");
        std::fs::write(&recovered_path, b"recovered").unwrap();
        let recovered = store
            .enqueue_asset_with_source_path_for_test(
                &unresolved.job_id,
                "retry/vanished.jpg",
                "sha256:recovered",
                Some(&recovered_path),
            )
            .unwrap();
        assert_eq!(recovered.failure_code, None);
        assert!(!recovered.media_fingerprint.starts_with("unavailable:"));
        assert_eq!(store.job(&unresolved.job_id).unwrap().failed, 0);
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn discovery_writer_saturation_is_concurrent_and_raii_clean() {
        let root = workspace("wp083-discovery-writer-saturation");
        let mut store = MatchStore::open(&root).unwrap();
        let mut budget = ResourceBudget::default();
        budget.surreal_writes = 1;
        store.governor = MatchResourceGovernor::new(budget).unwrap();
        store
            .register_model_generation("wp083-discovery-governor-model", true)
            .unwrap();
        let job = create_running_job(
            &store,
            "wp083-discovery-governor-root",
            "wp083-discovery-governor-model",
        );
        let request = ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 64 * 1024,
            surreal_writes: 1,
            ..ResourceRequest::default()
        };
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let holder_store = store.clone();
            let holder_job = job.job_id.clone();
            let holder_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let permit = holder_store
                    .acquire_discovery_write(&holder_job, "saturation/a.jpg", request)
                    .unwrap();
                holder_barrier.wait();
                holder_barrier.wait();
                drop(permit);
            });
            barrier.wait();
            assert_eq!(
                store
                    .acquire_discovery_write(&job.job_id, "saturation/b.jpg", request)
                    .err()
                    .as_deref(),
                Some("resource_pressure")
            );
            barrier.wait();
        });
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        assert!(!store
            .holds()
            .unwrap()
            .iter()
            .any(|hold| hold == HoldReason::ResourcePressure.as_str()));

        let success = store
            .acquire_discovery_write(&job.job_id, "saturation/recovered.jpg", request)
            .unwrap();
        store
            .enqueue_asset_with_source_path(
                &job.job_id,
                "saturation/recovered.jpg",
                "sha256:recovered",
                None,
                &success,
            )
            .unwrap();
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());

        let undersized = store
            .acquire_discovery_write(
                &job.job_id,
                "saturation/undersized.jpg",
                ResourceRequest {
                    queued_bytes: 1,
                    ..request
                },
            )
            .unwrap();
        assert!(store
            .enqueue_asset_with_source_path(
                &job.job_id,
                "saturation/undersized.jpg",
                "sha256:undersized",
                None,
                &undersized,
            )
            .is_err());
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());

        let wrong_media = store
            .acquire_discovery_write(&job.job_id, "saturation/bound.jpg", request)
            .unwrap();
        assert!(store
            .enqueue_asset_with_source_path(
                &job.job_id,
                "saturation/wrong.jpg",
                "sha256:wrong",
                None,
                &wrong_media,
            )
            .is_err());
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());

        let late = store
            .acquire_discovery_write(&job.job_id, "saturation/late.jpg", request)
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Cancelled)
            .unwrap();
        assert!(store
            .enqueue_asset_with_source_path(
                &job.job_id,
                "saturation/late.jpg",
                "sha256:late",
                None,
                &late,
            )
            .is_err());
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
        close(&root, store);
    }

    #[test]
    fn schema_v9_migration_resets_legacy_ephemeral_inference_cursors() {
        let root = workspace("wp083-v9-migration");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp083-migration-model", true)
            .unwrap();
        let job = store
            .create_job("migration-root", "wp083-migration-model")
            .unwrap();
        let asset = store
            .enqueue_asset(&job.job_id, "migration/image.jpg", "sha256:migration")
            .unwrap();
        let asset_id = asset.asset_id.clone();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("UPDATE match_job_asset SET next_stage = 'align', completed_stages = ['discover', 'detect'] WHERE asset_id = $asset_id; UPDATE match_schema_state:global SET schema_version = 8;")
                .bind(("asset_id", asset_id))
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let migrated = reopened.job_asset(&asset.asset_id).unwrap();
        assert_eq!(migrated.next_stage, JobStage::Detect.as_str());
        assert_eq!(migrated.completed_stages, vec!["discover"]);
        close(&root, reopened);
    }

    #[test]
    fn schema_v10_migration_backfills_assignment_media_keys_and_composite_index() {
        let root = workspace("wp083-v10-assignment-migration");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Migration Person", Vec::new()).unwrap();
        let observation = face(
            "migration-face",
            "migration/gallery.jpg",
            "sha256:migration-gallery",
            false,
        );
        let timestamp = now();
        let assignment = Assignment {
            assignment_id: observation.face_id.clone(),
            face_id: observation.face_id.clone(),
            person_id: person.person_id.clone(),
            media_key: observation.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "wp083-v10-migration-regression".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: observation.face_revision,
            person_revision: person.revision,
            operation_id: "migration-operation".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let migration_rows = vec![
            (
                FACE_TABLE,
                observation.face_id.as_str(),
                serde_json::to_value(&observation).unwrap(),
            ),
            (
                ASSIGNMENT_TABLE,
                assignment.assignment_id.as_str(),
                serde_json::to_value(&assignment).unwrap(),
            ),
        ];
        store
            .transactional_upserts_deletes(&migration_rows, &[])
            .unwrap();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; DEFINE FIELD OVERWRITE media_key ON TABLE match_assignment TYPE option<string>; UPDATE match_assignment UNSET media_key; DEFINE INDEX OVERWRITE match_assignment_person ON TABLE match_assignment FIELDS person_id; UPDATE match_schema_state:global SET schema_version = 9; COMMIT TRANSACTION;")
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let migrated = reopened
            .require::<Assignment>(ASSIGNMENT_TABLE, &assignment.assignment_id, "assignment")
            .unwrap();
        assert_eq!(migrated.media_key, observation.media_key);
        let gallery = reopened.person_gallery(&person.person_id, 0, 64).unwrap();
        assert_eq!(gallery.total_media, 1);
        assert_eq!(gallery.media_keys, vec!["migration/gallery.jpg"]);
        let db = reopened.store.db();
        let table_info = surreal_store::run(async move {
            let mut response = db
                .query("INFO FOR TABLE match_assignment;")
                .await
                .map_err(|error| error.to_string())?;
            let info: Option<Value> = response.take(0).map_err(|error| error.to_string())?;
            Ok::<_, String>(info.unwrap_or(Value::Null))
        })
        .unwrap();
        let table_info = serde_json::to_string(&table_info).unwrap();
        assert!(table_info.contains("match_assignment_person"));
        assert!(table_info.contains("person_id"));
        assert!(table_info.contains("media_key"));
        close(&root, reopened);
    }

    #[test]
    fn schema_v10_migration_rejects_assignment_without_canonical_face() {
        let root = workspace("wp083-v10-orphan-migration");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Orphan Person", Vec::new()).unwrap();
        let observation = face(
            "orphan-face",
            "orphan/gallery.jpg",
            "sha256:orphan-gallery",
            false,
        );
        let timestamp = now();
        let assignment = Assignment {
            assignment_id: observation.face_id.clone(),
            face_id: observation.face_id.clone(),
            person_id: person.person_id.clone(),
            media_key: observation.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: AssignmentState::OperatorConfirmed.as_str().to_string(),
            provenance: "wp083-v10-orphan-regression".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: observation.face_revision,
            person_revision: person.revision,
            operation_id: "orphan-operation".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let migration_rows = vec![
            (
                FACE_TABLE,
                observation.face_id.as_str(),
                serde_json::to_value(&observation).unwrap(),
            ),
            (
                ASSIGNMENT_TABLE,
                assignment.assignment_id.as_str(),
                serde_json::to_value(&assignment).unwrap(),
            ),
        ];
        store
            .transactional_upserts_deletes(&migration_rows, &[])
            .unwrap();
        let db = store.store.db();
        let face_id = observation.face_id.clone();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; DEFINE FIELD OVERWRITE media_key ON TABLE match_assignment TYPE option<string>; UPDATE match_assignment UNSET media_key; DELETE type::record('match_face_observation', $face_id); DEFINE INDEX OVERWRITE match_assignment_person ON TABLE match_assignment FIELDS person_id; UPDATE match_schema_state:global SET schema_version = 9; COMMIT TRANSACTION;")
                .bind(("face_id", face_id))
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let error = match MatchStore::open(&root) {
            Ok(_) => panic!("orphan assignment migration unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.contains("assignments without a canonical face media key"));
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    }

    #[test]
    fn schema_v19_migration_adds_face_leading_trust_indexes() {
        let root = workspace("wp085-v19-trust-face-indexes");
        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Preserved Person", Vec::new()).unwrap();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; REMOVE INDEX match_trusted_member_face ON TABLE match_trusted_member; REMOVE INDEX match_trusted_search_face_person ON TABLE match_trusted_search_embedding; UPDATE match_schema_state:global SET schema_version = 18; COMMIT TRANSACTION;")
                .await.map_err(|error| error.to_string())?
                .check().map_err(|error| error.to_string())
        }).unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(
            reopened
                .require::<Person>(PERSON_TABLE, &person.person_id, "preserved Person")
                .unwrap(),
            person
        );
        let db = reopened.store.db();
        let (member_info, search_info) = surreal_store::run(async move {
            let mut response = db.query("INFO FOR TABLE match_trusted_member; INFO FOR TABLE match_trusted_search_embedding;")
                .await.map_err(|error| error.to_string())?;
            let member: Option<Value> = response.take(0).map_err(|error| error.to_string())?;
            let search: Option<Value> = response.take(1).map_err(|error| error.to_string())?;
            Ok::<_, String>((member.unwrap(), search.unwrap()))
        }).unwrap();
        assert!(member_info["indexes"]["match_trusted_member_face"]
            .as_str()
            .unwrap()
            .contains("face_id"));
        let search_index = search_info["indexes"]["match_trusted_search_face_person"]
            .as_str()
            .unwrap();
        assert!(search_index.contains("face_id, person_id"));
        let marker = reopened
            .get_one::<Value>("match_schema_state", "global")
            .unwrap()
            .unwrap();
        assert_eq!(
            marker["schema_version"].as_u64(),
            Some(MATCH_SCHEMA_VERSION)
        );
        close(&root, reopened);
    }

    #[test]
    fn wp087_source_candidates_preserve_case_and_reject_unsafe_aliases() {
        let candidates =
            match_source_path_candidates(Path::new(r"D:\Media\MixedCase.PNG")).unwrap();
        assert!(candidates.len() <= 8);
        assert!(candidates.contains(&r"\\?\D:\Media\MixedCase.PNG".to_string()));
        assert!(candidates.contains(&"//?/D:/Media/MixedCase.PNG".to_string()));
        assert_eq!(candidates.len(), 8);
        assert!(candidates
            .iter()
            .all(|path| path.ends_with("MixedCase.PNG")));
        assert!(!candidates.contains(&r"D:\media\mixedcase.png".to_string()));
        let unc = match_source_path_candidates(Path::new(r"\\Server\Share\MixedCase.PNG")).unwrap();
        assert!(unc.contains(&r"\\?\UNC\Server\Share\MixedCase.PNG".to_string()));
        assert!(unc.contains(&"//?/UNC/Server/Share/MixedCase.PNG".to_string()));
        assert_eq!(unc.len(), 4);
        for unsafe_path in [
            "D界/image.png",
            r"D:relative.png",
            r"D:\Media\..\image.png",
            r"\\.\C:\image.png",
            r"D:\Media\image.png ",
            r"D:\Media\image.png:stream",
            r"relative\image.png",
        ] {
            let result =
                std::panic::catch_unwind(|| match_source_path_candidates(Path::new(unsafe_path)));
            assert!(result.is_ok(), "source validation panicked: {unsafe_path}");
            assert!(result.unwrap().is_err(), "{unsafe_path}");
        }
    }

    #[test]
    fn wp087_unindexed_source_metadata_reports_availability_without_canonical_identity() {
        let root = workspace("wp087-unindexed-source");
        let store = MatchStore::open(&root).unwrap();
        let source = root.join("missing-source.PNG");
        let value = store.media_metadata_for_source(&source).unwrap();
        assert_eq!(value["media_key"], Value::Null);
        assert_eq!(value["source_resolution"], "unindexed");
        assert_eq!(value["configured"], false);
        assert_eq!(value["rows"], serde_json::json!([]));
        assert!(value.get("error").is_none());
        assert!(value.get("source_geometry").is_none());
        assert!(store.media_faces_for_source(&source).unwrap().is_none());
        let media_root = root.join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        store.configure_index_root(&media_root, Vec::new()).unwrap();
        let configured = store.media_metadata_for_source(&source).unwrap();
        assert_eq!(configured["configured"], true);
        assert_eq!(configured["media_key"], Value::Null);
        assert_eq!(configured["rows"], serde_json::json!([]));
        assert!(store
            .media_metadata_for_source(Path::new("relative-source.PNG"))
            .is_err());
        close(&root, store);
    }

    #[test]
    fn wp087_source_resolver_groups_history_rejects_ambiguity_and_stale_asset() {
        let root = workspace("wp087-source-index");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("source-model", true)
            .unwrap();
        let job = store.create_job("source-root", "source-model").unwrap();
        let mut asset = store
            .enqueue_asset(&job.job_id, "source/image.png", "sha256:source")
            .unwrap();
        asset.source_path = Some(r"\\?\D:\Media\MixedCase.PNG".into());
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        for index in 0..128 {
            let mut history = asset.clone();
            history.asset_id = format!("source-history-{index:04}");
            store
                .upsert_json(JOB_ASSET_TABLE, &history.asset_id, &history)
                .unwrap();
        }
        let selected = Path::new(r"D:\Media\MixedCase.PNG");
        assert_eq!(
            store
                .canonical_media_key_for_source(selected)
                .unwrap()
                .as_deref(),
            Some("source/image.png")
        );
        let mut other = asset.clone();
        other.asset_id = "source-ambiguous".into();
        other.media_key = "other/image.png".into();
        store
            .upsert_json(JOB_ASSET_TABLE, &other.asset_id, &other)
            .unwrap();
        assert!(store
            .canonical_media_key_for_source(selected)
            .unwrap_err()
            .contains("ambiguous"));
        store
            .transactional_upserts_deletes(&[], &[(JOB_ASSET_TABLE, other.asset_id.as_str())])
            .unwrap();
        asset.asset_id = "source-newest".into();
        asset.updated_at = "9999-12-31T23:59:59Z".into();
        asset.source_path = Some(r"D:\Elsewhere\MixedCase.PNG".into());
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        assert!(store
            .canonical_media_key_for_source(selected)
            .unwrap_err()
            .contains("stale"));
        close(&root, store);
    }

    #[test]
    fn schema_v21_migration_adds_source_index_without_asset_rewrite() {
        let root = workspace("wp087-source-index-migration");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("migration-source-model", true)
            .unwrap();
        let job = store
            .create_job("migration-source-root", "migration-source-model")
            .unwrap();
        let mut asset = store
            .enqueue_asset(
                &job.job_id,
                "migration/image.png",
                "sha256:migration-source",
            )
            .unwrap();
        asset.source_path = Some(r"\\?\D:\Media\MixedCase.PNG".into());
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        let original_asset_bytes =
            serde_json::to_vec(&store.job_assets(&job.job_id).unwrap()).unwrap();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; REMOVE INDEX match_job_asset_source ON TABLE match_job_asset; UPDATE match_schema_state:global SET schema_version = 21; COMMIT TRANSACTION;")
                .await.map_err(|error| error.to_string())?.check().map_err(|error| error.to_string())
        }).unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert_eq!(
            serde_json::to_vec(&reopened.job_assets(&job.job_id).unwrap()).unwrap(),
            original_asset_bytes
        );
        assert_eq!(
            reopened
                .canonical_media_key_for_source(Path::new(r"D:\Media\MixedCase.PNG"))
                .unwrap()
                .as_deref(),
            Some("migration/image.png")
        );
        let db = reopened.store.db();
        let info: Option<Value> = surreal_store::run(async move {
            let mut response = db.query("INFO FOR TABLE match_job_asset; SELECT VALUE media_key FROM match_job_asset WITH INDEX match_job_asset_source WHERE source_path IN $source_paths GROUP BY media_key ORDER BY media_key ASC LIMIT 2 EXPLAIN FULL;")
                .bind(("source_paths", match_source_path_candidates(Path::new(r"D:\Media\MixedCase.PNG")).unwrap())).await.map_err(|error| error.to_string())?;
            let info: Option<Value> = response.take(0).map_err(|error| error.to_string())?;
            let plan: Value = response.take(1).map_err(|error| error.to_string())?;
            assert!(plan.to_string().contains("match_job_asset_source"), "{plan}");
            fn inspect_source_plan(value: &Value, scans: &mut usize) {
                match value {
                    Value::Array(items) => {
                        for item in items {
                            inspect_source_plan(item, scans);
                        }
                    }
                    Value::Object(fields) => {
                        if let Some(operator) = fields.get("operator").and_then(Value::as_str) {
                            if operator == "IndexScan" {
                                assert_eq!(value["attributes"]["index"].as_str(), Some("match_job_asset_source"), "{value}");
                                *scans += 1;
                            } else if operator.contains("Scan") {
                                assert_eq!(operator, "UnionIndexScan", "{value}");
                            }
                        }
                        for child in fields.values() {
                            inspect_source_plan(child, scans);
                        }
                    }
                    _ => {}
                }
            }
            let mut source_scans = 0;
            inspect_source_plan(&plan, &mut source_scans);
            assert!(source_scans > 0, "{plan}");
            assert!(!json_contains(&plan, "Iterate Table") && !json_contains(&plan, "TableScan") && !json_contains(&plan, "IterateTable"), "{plan}");
            Ok::<_, String>(info)
        }).unwrap();
        assert!(info.unwrap().to_string().contains("match_job_asset_source"));
        close(&root, reopened);
    }

    #[test]
    fn schema_v11_migration_adds_job_asset_media_index() {
        let root = workspace("wp083-v11-job-asset-media-index");
        let store = MatchStore::open(&root).unwrap();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; REMOVE INDEX match_job_asset_media ON TABLE match_job_asset; UPDATE match_schema_state:global SET schema_version = 10; COMMIT TRANSACTION;")
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let db = reopened.store.db();
        let table_info = surreal_store::run(async move {
            let mut response = db
                .query("INFO FOR TABLE match_job_asset;")
                .await
                .map_err(|error| error.to_string())?;
            let info: Option<Value> = response.take(0).map_err(|error| error.to_string())?;
            Ok::<_, String>(info.unwrap_or(Value::Null))
        })
        .unwrap();
        let table_info = serde_json::to_string(&table_info).unwrap();
        assert!(table_info.contains("match_job_asset_media"));
        assert!(table_info.contains("media_key"));
        close(&root, reopened);
    }

    #[test]
    fn schema_v14_migration_adds_candidate_leading_suggestion_inventory_indexes() {
        let root = workspace("wp084-v14-suggestion-inventory-indexes");
        let store = MatchStore::open(&root).unwrap();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; REMOVE INDEX match_suggestion_person_inventory ON TABLE match_suggestion; REMOVE INDEX match_suggestion_person_face_inventory ON TABLE match_suggestion; UPDATE match_schema_state:global SET schema_version = 13; COMMIT TRANSACTION;")
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let db = reopened.store.db();
        let (table_info, schema_state, assignment_inventory_plan) = surreal_store::run(async move {
            let mut response = db
                .query("INFO FOR TABLE match_suggestion; SELECT * OMIT id FROM ONLY match_schema_state:global; SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person_inventory WHERE person_id = $person_id AND assignment_id > $after_assignment_id ORDER BY assignment_id ASC LIMIT 512 EXPLAIN FULL;")
                .bind(("person_id", "migration-plan-person".to_string()))
                .bind(("after_assignment_id", String::new()))
                .await
                .map_err(|error| error.to_string())?;
            let info: Option<Value> = response.take(0).map_err(|error| error.to_string())?;
            let schema_state: Option<Value> =
                response.take(1).map_err(|error| error.to_string())?;
            let assignment_inventory_plan: Vec<Value> =
                response.take(2).map_err(|error| error.to_string())?;
            Ok::<_, String>((
                info.unwrap_or(Value::Null),
                schema_state.unwrap_or(Value::Null),
                assignment_inventory_plan,
            ))
        })
        .unwrap();
        let table_info = serde_json::to_string(&table_info).unwrap();
        assert!(table_info.contains("match_suggestion_person_inventory"));
        assert!(table_info.contains("candidate_person_id"));
        assert!(table_info.contains("suggestion_id"));
        assert!(table_info.contains("match_suggestion_person_face_inventory"));
        assert!(table_info.contains("face_id"));
        assert_eq!(
            schema_state.get("schema_version").and_then(Value::as_u64),
            Some(MATCH_SCHEMA_VERSION)
        );
        assert!(serde_json::to_string(&assignment_inventory_plan)
            .unwrap()
            .contains("match_assignment_person_inventory"));
        close(&root, reopened);
    }

    #[test]
    fn job_failures_and_settings_pages_remain_durable_and_reachable() {
        let root = workspace("wp083-job-failure-pages");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp083-failure-model", true)
            .unwrap();
        let mut jobs = Vec::new();
        for index in 0..3 {
            jobs.push(
                store
                    .create_job(&format!("failure-root-{index}"), "wp083-failure-model")
                    .unwrap(),
            );
        }
        store
            .set_job_lifecycle(&jobs[0].job_id, JobLifecycle::Running)
            .unwrap();
        store
            .record_job_failure(&jobs[0].job_id, "io", "private/source.jpg failed")
            .unwrap();
        let page = store.settings_snapshot_page(1, 1).unwrap();
        assert_eq!(page["page"]["offset"], 1);
        assert_eq!(page["page"]["limit"], 1);
        assert_eq!(page["page"]["total_jobs"], 3);
        assert_eq!(page["jobs"].as_array().unwrap().len(), 1);
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let failed: IndexJob = reopened
            .require(JOB_TABLE, &jobs[0].job_id, "IndexJob")
            .unwrap();
        assert_eq!(failed.failure_code.as_deref(), Some("io"));
        assert_eq!(
            failed.failure_message.as_deref(),
            Some("private/source.jpg failed")
        );
        let public = reopened.public_snapshot().unwrap();
        assert!(!json_contains(&public, "private/source.jpg"));
        close(&root, reopened);
    }

    #[test]
    fn pausing_execution_error_becomes_durable_failed_and_retryable() {
        let root = workspace("wp083-pausing-execution-failure");
        let store = MatchStore::open(&root).unwrap();
        store
            .register_model_generation("wp083-pausing-model", true)
            .unwrap();
        let job = store
            .create_job("pausing-failure-root", "wp083-pausing-model")
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let pausing = store.control_job(&job.job_id, "pause").unwrap();
        assert_eq!(pausing.lifecycle, "pausing");

        let failed = store
            .record_job_failure(
                &job.job_id,
                "io",
                "configured Match root changed during execution",
            )
            .unwrap();
        assert_eq!(failed.lifecycle, "failed");
        assert_eq!(failed.failure_code.as_deref(), Some("io"));
        assert_eq!(
            failed.failure_message.as_deref(),
            Some("configured Match root changed during execution")
        );
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        let durable = reopened.job(&job.job_id).unwrap();
        assert_eq!(durable.lifecycle, "failed");
        assert_eq!(durable.failure_code.as_deref(), Some("io"));
        assert_eq!(
            durable.failure_message.as_deref(),
            Some("configured Match root changed during execution")
        );
        let retrying = reopened.control_job(&job.job_id, "retry").unwrap();
        assert_eq!(retrying.lifecycle, "retrying");
        assert!(retrying.failure_code.is_none());
        assert!(retrying.failure_message.is_none());
        close(&root, reopened);
    }
}

#[cfg(test)]
mod resident_budget_tests {
    use super::*;
    #[test]
    fn wp086_resident_admission_is_atomic_and_shrink_preserves_charge() {
        let governor = MatchResourceGovernor::new(ResourceBudget {
            worker_memory_bytes: 100,
            queued_bytes: 240,
            ..Default::default()
        })
        .unwrap();
        let mut resident = governor
            .try_acquire(ResourceRequest {
                worker_memory_bytes: 100,
                queued_bytes: 240,
                ..Default::default()
            })
            .unwrap();
        resident.release_worker_preparation_bytes().unwrap();
        assert!(governor
            .try_acquire(ResourceRequest {
                worker_memory_bytes: 1,
                queued_bytes: 240,
                ..Default::default()
            })
            .is_err());
        assert_eq!(governor.usage().unwrap().queued_bytes, 0);
        assert_eq!(governor.usage().unwrap().worker_memory_bytes, 100);
        let snapshot = governor
            .try_acquire(ResourceRequest {
                queued_bytes: 240,
                ..Default::default()
            })
            .unwrap();
        drop(resident);
        assert_eq!(governor.usage().unwrap().worker_memory_bytes, 0);
        assert_eq!(governor.usage().unwrap().queued_bytes, 240);
        drop(snapshot);
    }
}
