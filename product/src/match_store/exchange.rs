//! Bounded, versioned Match identity exchange and sidecar-only MWG XMP interop (WP-085).
//!
//! The Facial bundle is canonical plain JSON. It deliberately contains no
//! archive, compression, embedding, crop, vector-index, job, or projection
//! payload. Every import is completely parsed, hashed, and graph-validated
//! before the Match write lock is upgraded to a mutation.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub const IDENTITY_BUNDLE_FORMAT: &str = "facial-identity-bundle";
pub const IDENTITY_BUNDLE_VERSION: u32 = 1;
pub const IDENTITY_BUNDLE_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const IDENTITY_BUNDLE_MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
pub const IDENTITY_BUNDLE_MAX_ENTITIES: usize = 250_000;
pub const IDENTITY_BUNDLE_MAX_STRING_BYTES: usize = 32 * 1024 * 1024;
pub const IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES: usize = 64 * 1024;
pub const IDENTITY_BUNDLE_MAX_NESTING_DEPTH: usize = 32;
pub const IDENTITY_BUNDLE_MAX_REFERENCES: usize = 1_000_000;
pub const IDENTITY_BUNDLE_MAX_JSON_TOKENS: usize = 16_000_000;
pub const IDENTITY_XMP_MAX_SIDECAR_BYTES: usize = 4 * 1024 * 1024;
pub const IDENTITY_RELOCATION_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const IDENTITY_RELOCATION_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES: usize = 4_096;
pub const IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT: usize = 100;
/// Portable audit timestamps may lead the importing host only by this small,
/// explicit allowance. Comparisons are performed on parsed RFC3339 instants,
/// so alternate offsets cannot bypass the ceiling.
pub const IDENTITY_BUNDLE_MAX_CLOCK_SKEW_SECONDS: i64 = 300;

const CORRECTION_MEDIA_OPERATION_TABLE: &str = "match_correction_media_operation";
const IDENTITY_IMPORT_JOURNAL_TABLE: &str = "match_identity_import_journal";
const IDENTITY_IMPORT_JOURNAL_ID: &str = "global";
const IDENTITY_IMPORT_JOURNAL_VERSION: u64 = 2;
const IDENTITY_IMPORT_RECOVERY_PREFIX: &str = "match-import-recovery-";
const IDENTITY_IMPORT_DERIVED_RECOVERY_FORMAT: &str = "facial-match-import-derived-recovery";
const IDENTITY_IMPORT_DERIVED_RECOVERY_VERSION: u32 = 1;
const IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX: &str = ".facial-derived.json";
const IDENTITY_IMPORT_DERIVED_PREFLIGHT_PAGE: usize = 256;
const PORTABLE_TABLES: [&str; 17] = [
    super::context::MEDIA_CONTEXT_TABLE,
    super::video::VIDEO_OBSERVATION_TABLE,
    super::context::CONTEXT_TABLE,
    PERSON_TABLE,
    LOOK_TABLE,
    TEMPLATE_SET_TABLE,
    TRUSTED_MEMBER_TABLE,
    FACE_TABLE,
    ASSIGNMENT_TABLE,
    SUGGESTION_TABLE,
    CONSTRAINT_TABLE,
    FACE_DISPOSITION_TABLE,
    OPERATION_TABLE,
    CORRECTION_MEDIA_OPERATION_TABLE,
    SUGGESTION_SOURCE_PROVENANCE_TABLE,
    ROOT_CONFIG_TABLE,
    GENERATION_TABLE,
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityBundleManifest {
    pub format: String,
    pub version: u32,
    pub schema_version: u64,
    pub schema_generation: String,
    pub content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortableMatchRoot {
    pub root_id: String,
    /// A non-authoritative portable token. Import never treats it as a host
    /// path; callers must resolve it through an explicit relocation map.
    pub portable_path: String,
    pub exclusions: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, SurrealValue, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortableCorrectionMediaOperation {
    pub mapping_id: String,
    pub media_key: String,
    pub media_fingerprint: String,
    pub operation_id: String,
    pub kind: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IdentityBundleGraph {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_context: Vec<CanonicalMediaContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_observations: Vec<StoredVideoObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_context: Vec<StoredReviewContext>,
    pub people: Vec<Person>,
    pub looks: Vec<Look>,
    pub trusted_template_sets: Vec<TrustedTemplateSet>,
    pub trusted_memberships: Vec<TrustedTemplateMembership>,
    pub faces: Vec<FaceObservation>,
    pub assignments: Vec<Assignment>,
    pub suggestions: Vec<Suggestion>,
    pub constraints: Vec<CannotLinkConstraint>,
    pub dispositions: Vec<FaceDisposition>,
    pub operations: Vec<MatchOperation>,
    pub correction_media_operations: Vec<PortableCorrectionMediaOperation>,
    #[serde(default)]
    pub suggestion_source_provenance: Vec<SuggestionSourceProvenance>,
    pub roots: Vec<PortableMatchRoot>,
    pub model_generations: Vec<ModelGeneration>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IdentityBundleV1 {
    pub manifest: IdentityBundleManifest,
    pub graph: IdentityBundleGraph,
}

#[derive(Debug)]
pub(super) struct IdentityBundleFileSnapshot {
    pub bundle: IdentityBundleV1,
    pub canonical_path: PathBuf,
    pub file_sha256: String,
    pub canonical_bytes: usize,
    pub bytes: Vec<u8>,
    guard: File,
}

impl IdentityBundleFileSnapshot {
    pub(super) fn try_clone_guarded(&self) -> Result<Self, String> {
        Ok(Self {
            bundle: self.bundle.clone(),
            canonical_path: self.canonical_path.clone(),
            file_sha256: self.file_sha256.clone(),
            canonical_bytes: self.canonical_bytes,
            bytes: self.bytes.clone(),
            guard: self
                .guard
                .try_clone()
                .map_err(|error| format!("duplicate identity bundle guard: {error}"))?,
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityBundleLimits {
    pub bundle_bytes: usize,
    pub decoded_bytes: usize,
    pub entity_count: usize,
    pub string_bytes: usize,
    pub single_string_bytes: usize,
    pub nesting_depth: usize,
    pub reference_count: usize,
    pub json_token_work: usize,
    pub xmp_sidecar_bytes: usize,
    pub relocation_file_bytes: u64,
    pub relocation_total_bytes: u64,
    pub relocation_evidence_handles: usize,
    pub import_derived_recovery_bytes: usize,
}

impl Default for IdentityBundleLimits {
    fn default() -> Self {
        Self {
            bundle_bytes: IDENTITY_BUNDLE_MAX_BYTES,
            decoded_bytes: IDENTITY_BUNDLE_MAX_DECODED_BYTES,
            entity_count: IDENTITY_BUNDLE_MAX_ENTITIES,
            string_bytes: IDENTITY_BUNDLE_MAX_STRING_BYTES,
            single_string_bytes: IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES,
            nesting_depth: IDENTITY_BUNDLE_MAX_NESTING_DEPTH,
            reference_count: IDENTITY_BUNDLE_MAX_REFERENCES,
            json_token_work: IDENTITY_BUNDLE_MAX_JSON_TOKENS,
            xmp_sidecar_bytes: IDENTITY_XMP_MAX_SIDECAR_BYTES,
            relocation_file_bytes: IDENTITY_RELOCATION_MAX_FILE_BYTES,
            relocation_total_bytes: IDENTITY_RELOCATION_MAX_TOTAL_BYTES,
            relocation_evidence_handles: IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES,
            import_derived_recovery_bytes: IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityBundleExportPreview {
    pub format: String,
    pub version: u32,
    pub content_sha256: String,
    pub canonical_bytes: usize,
    pub entity_count: usize,
    pub table_counts: BTreeMap<String, usize>,
    pub excluded_payloads: Vec<String>,
    pub limits: IdentityBundleLimits,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityBundleExportReceipt {
    pub output_path: PathBuf,
    pub content_sha256: String,
    pub canonical_bytes: usize,
    pub entity_count: usize,
    pub publication_warning: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdentityImportMode {
    Merge,
    Replace,
}

impl IdentityImportMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Replace => "replace",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityImportConflict {
    pub table: String,
    pub stable_id: String,
}

#[derive(Clone, Debug)]
pub struct IdentityImportPlan {
    pub plan_token: String,
    content_sha256: String,
    mode: IdentityImportMode,
    creates: usize,
    updates: usize,
    identical: usize,
    deletes: usize,
    conflicts: Vec<IdentityImportConflict>,
    pub unresolved_root_ids: Vec<String>,
    pub unresolved_media_keys: Vec<String>,
    already_applied: bool,
    entity_count: usize,
    reference_count: usize,
    bundle: IdentityBundleV1,
    relocated_graph: IdentityBundleGraph,
    relocations: BTreeMap<String, String>,
    relocated_media_sha256: BTreeMap<String, String>,
    current_state_sha256: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityImportPlanSummary {
    pub sample_limit: usize,
    pub plan_token: String,
    pub content_sha256: String,
    pub mode: IdentityImportMode,
    pub creates: usize,
    pub updates: usize,
    pub identical: usize,
    pub deletes: usize,
    pub conflict_count: usize,
    pub conflicts: Vec<IdentityImportConflict>,
    pub conflicts_truncated: bool,
    pub unresolved_root_count: usize,
    pub unresolved_root_ids: Vec<String>,
    pub unresolved_roots_truncated: bool,
    pub unresolved_media_count: usize,
    pub unresolved_media_keys: Vec<String>,
    pub unresolved_media_truncated: bool,
    pub already_applied: bool,
    pub entity_count: usize,
    pub reference_count: usize,
}

impl IdentityImportPlan {
    /// Redacted, persistence-safe dry-run DTO. Bundle payload and current row
    /// snapshots remain private; apply-by-file recomputes them from canonical
    /// state and requires this exact token.
    pub fn summary(&self) -> IdentityImportPlanSummary {
        let (conflicts, conflicts_truncated) = bounded_receipt_sample(&self.conflicts);
        let (unresolved_root_ids, unresolved_roots_truncated) =
            bounded_receipt_sample(&self.unresolved_root_ids);
        let (unresolved_media_keys, unresolved_media_truncated) =
            bounded_receipt_sample(&self.unresolved_media_keys);
        IdentityImportPlanSummary {
            sample_limit: IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT,
            plan_token: self.plan_token.clone(),
            content_sha256: self.content_sha256.clone(),
            mode: self.mode,
            creates: self.creates,
            updates: self.updates,
            identical: self.identical,
            deletes: self.deletes,
            conflict_count: self.conflicts.len(),
            conflicts,
            conflicts_truncated,
            unresolved_root_count: self.unresolved_root_ids.len(),
            unresolved_root_ids,
            unresolved_roots_truncated,
            unresolved_media_count: self.unresolved_media_keys.len(),
            unresolved_media_keys,
            unresolved_media_truncated,
            already_applied: self.already_applied,
            entity_count: self.entity_count,
            reference_count: self.reference_count,
        }
    }

    pub(super) fn has_destructive_changes(&self) -> bool {
        self.updates != 0 || self.deletes != 0
    }
}

fn bounded_receipt_sample<T: Clone>(values: &[T]) -> (Vec<T>, bool) {
    (
        values
            .iter()
            .take(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT)
            .cloned()
            .collect(),
        values.len() > IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT,
    )
}

#[derive(Clone, Debug)]
pub struct IdentityImportRollback {
    expected_post_state_sha256: String,
    expected_post_derived_sha256: String,
    pre_rows: Vec<OwnedPortableRow>,
    pre_derived: ExchangeDerivedSnapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ExchangeDerivedSnapshot {
    trusted_search_rows: Vec<TrustedSearchEmbedding>,
    trusted_index_build: Option<TrustedIndexBuildReceipt>,
    calibrations: Vec<CalibrationActivation>,
}

#[derive(Clone, Debug)]
pub struct IdentityImportReceipt {
    pub plan_token: String,
    pub content_sha256: String,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub already_applied: bool,
    pub reconciled_trusted_search_rows: usize,
    pub post_state_sha256: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub rollback: IdentityImportRollback,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityImportReceiptSummary {
    pub plan_token: String,
    pub content_sha256: String,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub already_applied: bool,
    pub reconciled_trusted_search_rows: usize,
    pub post_state_sha256: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
}

impl IdentityImportReceipt {
    pub fn summary(&self) -> IdentityImportReceiptSummary {
        IdentityImportReceiptSummary {
            plan_token: self.plan_token.clone(),
            content_sha256: self.content_sha256.clone(),
            created: self.created,
            updated: self.updated,
            deleted: self.deleted,
            already_applied: self.already_applied,
            reconciled_trusted_search_rows: self.reconciled_trusted_search_rows,
            post_state_sha256: self.post_state_sha256.clone(),
            identity_revision: self.identity_revision,
            catalog_revision: self.catalog_revision,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityReconciliation {
    pub content_sha256: String,
    pub state_sha256: String,
    pub entity_count: usize,
    pub reference_count: usize,
    pub trusted_search_rows: usize,
    pub identity_revision: u64,
    pub catalog_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MwgXmpRegion {
    pub face_id: String,
    pub person_id: String,
    pub name: String,
    /// Facial top-left x/y/width/height, normalized to the oriented media.
    pub bounds_normalized: [f32; 4],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct XmpUnsupportedSemantic {
    pub code: String,
    pub stable_id: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MwgXmpSidecarPreview {
    pub preview_token: String,
    pub media_key: String,
    pub sidecar_path: PathBuf,
    pub content_sha256: String,
    pub sidecar_bytes: usize,
    pub regions: Vec<MwgXmpRegion>,
    pub unsupported: Vec<XmpUnsupportedSemantic>,
    #[serde(skip)]
    canonical_xml: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MwgXmpSidecarReceipt {
    pub preview_token: String,
    pub media_key: String,
    pub sidecar_path: PathBuf,
    pub content_sha256: String,
    pub sidecar_bytes: usize,
    pub original_media_rows_mutated: usize,
    pub publication_warning: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MwgXmpImportPreview {
    pub preview_token: String,
    pub sidecar_path: PathBuf,
    pub content_sha256: String,
    pub sidecar_bytes: usize,
    pub regions: Vec<MwgXmpRegion>,
    pub unsupported: Vec<XmpUnsupportedSemantic>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MwgXmpImportReceipt {
    pub preview_token: String,
    pub sidecar_path: PathBuf,
    pub content_sha256: String,
    /// XMP is an interoperability projection, never Match truth. Parsed rows
    /// are returned for an explicit later correction workflow and no evidence
    /// state is promoted by this import primitive.
    pub staged_regions: Vec<MwgXmpRegion>,
    pub applied_match_rows: usize,
    pub original_media_rows_mutated: usize,
}

#[derive(Clone, Debug, Serialize)]
struct BundleHashMaterial<'a> {
    format: &'a str,
    version: u32,
    schema_version: u64,
    schema_generation: &'a str,
    graph: &'a IdentityBundleGraph,
}

#[derive(Clone, Debug, Serialize)]
struct PlanTokenMaterial<'a> {
    content_sha256: &'a str,
    current_state_sha256: &'a str,
    mode: &'a str,
    relocations: &'a BTreeMap<String, String>,
    relocated_media_sha256: &'a BTreeMap<String, String>,
    creates: usize,
    updates: usize,
    identical: usize,
    deletes: usize,
    conflicts: &'a [IdentityImportConflict],
    unresolved_root_ids: &'a [String],
    unresolved_media_keys: &'a [String],
    already_applied: bool,
    entity_count: usize,
    reference_count: usize,
}

fn identity_import_plan_token(material: &PlanTokenMaterial<'_>) -> Result<String, String> {
    let bytes = serde_json::to_vec(material)
        .map_err(|error| format!("serialize identity import plan token: {error}"))?;
    Ok(sha256_bytes(&bytes))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct StoredIdentityGraph {
    #[serde(default)]
    media_context: Vec<CanonicalMediaContext>,
    video_observations: Vec<StoredVideoObservation>,
    review_context: Vec<StoredReviewContext>,
    people: Vec<Person>,
    looks: Vec<Look>,
    trusted_template_sets: Vec<TrustedTemplateSet>,
    trusted_memberships: Vec<TrustedTemplateMembership>,
    faces: Vec<FaceObservation>,
    assignments: Vec<Assignment>,
    suggestions: Vec<Suggestion>,
    constraints: Vec<CannotLinkConstraint>,
    dispositions: Vec<FaceDisposition>,
    operations: Vec<MatchOperation>,
    correction_media_operations: Vec<PortableCorrectionMediaOperation>,
    suggestion_source_provenance: Vec<SuggestionSourceProvenance>,
    roots: Vec<MatchIndexRoot>,
    model_generations: Vec<ModelGeneration>,
}

#[derive(Default)]
struct LegacySuggestionHistory {
    face_embeddings: BTreeMap<
        (String, String),
        Vec<(
            chrono::DateTime<chrono::FixedOffset>,
            FaceObservation,
            FaceEmbedding,
        )>,
    >,
    people: BTreeMap<String, Vec<(chrono::DateTime<chrono::FixedOffset>, Person)>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LegacyFaceEmbeddingAnchorKey {
    face_id: String,
    embedding_id: String,
    face_revision: u64,
    media_fingerprint: String,
    model_generation: String,
    job_id: String,
}

#[derive(Debug)]
struct LegacySuggestionLookup {
    current_faces: BTreeMap<String, FaceObservation>,
    current_people: BTreeMap<String, Person>,
    historical_face_embeddings: BTreeMap<
        LegacyFaceEmbeddingAnchorKey,
        Vec<(
            chrono::DateTime<chrono::FixedOffset>,
            FaceObservation,
            FaceEmbedding,
        )>,
    >,
    historical_people: BTreeMap<String, Vec<(chrono::DateTime<chrono::FixedOffset>, Person)>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct OwnedPortableRow {
    table: String,
    stable_id: String,
    value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IdentityImportJournal {
    journal_id: String,
    version: u64,
    plan_token: String,
    pre_state_sha256: String,
    recovery_path: String,
    recovery_file_sha256: String,
    recovery_content_sha256: String,
    relocations_json: String,
    phase: String,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct IdentityImportDerivedRecovery {
    format: String,
    version: u32,
    snapshot_sha256: String,
    snapshot: ExchangeDerivedSnapshot,
    #[serde(default)]
    raw_operation_rows: Vec<OwnedPortableRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IdentityImportDerivedBinding {
    canonical_path: String,
    file_sha256: String,
    snapshot_sha256: String,
    canonical_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IdentityImportRecoveryEvidence {
    roots: BTreeMap<String, String>,
    derived: IdentityImportDerivedBinding,
    #[serde(default)]
    persisted_suggestion_provenance_ids: Vec<String>,
}

struct GuardedIdentityImportDerivedRecovery {
    recovery: IdentityImportDerivedRecovery,
    _guard: GuardedFileBytes,
}

impl MatchStore {
    pub fn identity_bundle_limits() -> IdentityBundleLimits {
        IdentityBundleLimits::default()
    }

    pub fn preview_identity_bundle_export(&self) -> Result<IdentityBundleExportPreview, String> {
        let bundle = self.build_identity_bundle()?;
        export_preview(&bundle)
    }

    pub fn export_identity_bundle(
        &self,
        output_path: &Path,
    ) -> Result<IdentityBundleExportReceipt, String> {
        let bundle = self.build_identity_bundle()?;
        let preview = export_preview(&bundle)?;
        let bytes = canonical_bundle_bytes(&bundle)?;
        preflight_canonical_identity_bundle_bytes(&bytes)?;
        let publication = write_new_regular_file(output_path, &bytes, IDENTITY_BUNDLE_MAX_BYTES)?;
        Ok(IdentityBundleExportReceipt {
            output_path: output_path.to_path_buf(),
            content_sha256: preview.content_sha256,
            canonical_bytes: preview.canonical_bytes,
            entity_count: preview.entity_count,
            publication_warning: publication.warning,
        })
    }

    pub fn export_identity_bundle_expected(
        &self,
        output_path: &Path,
        expected_content_sha256: &str,
    ) -> Result<IdentityBundleExportReceipt, String> {
        validate_sha256(
            "expected identity bundle content hash",
            expected_content_sha256,
        )?;
        let _guard = self.database_read_guard("Match identity export read lock is poisoned")?;
        let bundle = self.build_identity_bundle_unlocked()?;
        if bundle.manifest.content_sha256 != expected_content_sha256 {
            return Err("stale identity export preview: Match content changed".to_string());
        }
        let preview = export_preview(&bundle)?;
        let bytes = canonical_bundle_bytes(&bundle)?;
        preflight_canonical_identity_bundle_bytes(&bytes)?;
        let publication = write_new_regular_file(output_path, &bytes, IDENTITY_BUNDLE_MAX_BYTES)?;
        Ok(IdentityBundleExportReceipt {
            output_path: output_path.to_path_buf(),
            content_sha256: preview.content_sha256,
            canonical_bytes: preview.canonical_bytes,
            entity_count: preview.entity_count,
            publication_warning: publication.warning,
        })
    }

    /// Durable restart-safe recovery uses the same verified BundleV1 format.
    /// The caller chooses a unique app-owned path and persists the returned
    /// hash beside its operation receipt before starting a destructive apply.
    pub fn export_identity_recovery_bundle(
        &self,
        output_path: &Path,
    ) -> Result<IdentityBundleExportReceipt, String> {
        let bundle = self.build_identity_bundle()?;
        let preview = export_preview(&bundle)?;
        let bytes = canonical_bundle_bytes(&bundle)?;
        preflight_canonical_identity_bundle_bytes(&bytes)?;
        publish_recovery_bytes(output_path, &bytes)?;
        Ok(IdentityBundleExportReceipt {
            output_path: output_path.to_path_buf(),
            content_sha256: preview.content_sha256,
            canonical_bytes: preview.canonical_bytes,
            entity_count: preview.entity_count,
            publication_warning: None,
        })
    }

    pub fn read_identity_bundle(path: &Path) -> Result<IdentityBundleV1, String> {
        Ok(Self::read_identity_bundle_snapshot(path)?.bundle)
    }

    pub(super) fn read_identity_bundle_snapshot(
        path: &Path,
    ) -> Result<IdentityBundleFileSnapshot, String> {
        let snapshot =
            read_bounded_regular_file_snapshot(path, IDENTITY_BUNDLE_MAX_BYTES, "identity bundle")?;
        let bundle = parse_identity_bundle_bytes(&snapshot.bytes)?;
        Ok(IdentityBundleFileSnapshot {
            bundle,
            canonical_path: snapshot.canonical_path,
            file_sha256: sha256_bytes(&snapshot.bytes),
            canonical_bytes: snapshot.bytes.len(),
            bytes: snapshot.bytes,
            guard: snapshot.guard,
        })
    }

    pub(super) fn revalidate_identity_bundle_snapshot_path(
        snapshot: &IdentityBundleFileSnapshot,
    ) -> Result<(), String> {
        let observed = read_bounded_regular_file_snapshot(
            &snapshot.canonical_path,
            IDENTITY_BUNDLE_MAX_BYTES,
            "retained identity recovery bundle",
        )?;
        if !same_open_file_identity(&snapshot.guard, &observed.guard)?
            || observed.bytes != snapshot.bytes
            || sha256_bytes(&observed.bytes) != snapshot.file_sha256
        {
            return Err(
                "retained identity recovery bundle changed path or byte identity".to_string(),
            );
        }
        Ok(())
    }

    pub(super) fn publish_identity_bundle_snapshot_copy(
        snapshot: &IdentityBundleFileSnapshot,
        path: &Path,
    ) -> Result<(), String> {
        publish_recovery_bytes(path, &snapshot.bytes)
    }

    pub fn preview_identity_bundle_import(
        &self,
        path: &Path,
        relocations: &BTreeMap<String, String>,
        mode: IdentityImportMode,
    ) -> Result<IdentityImportPlan, String> {
        let bundle = Self::read_identity_bundle(path)?;
        self.plan_identity_bundle_import(bundle, relocations, mode)
    }

    pub fn preview_identity_bundle_import_bytes(
        &self,
        bytes: &[u8],
        relocations: &BTreeMap<String, String>,
        mode: IdentityImportMode,
    ) -> Result<IdentityImportPlan, String> {
        let bundle = parse_identity_bundle_bytes(bytes)?;
        self.plan_identity_bundle_import(bundle, relocations, mode)
    }

    pub(super) fn preview_identity_bundle_import_snapshot(
        &self,
        bundle: IdentityBundleV1,
        relocations: &BTreeMap<String, String>,
        mode: IdentityImportMode,
    ) -> Result<IdentityImportPlan, String> {
        self.plan_identity_bundle_import(bundle, relocations, mode)
    }

    /// Receipt-backed apply entrypoint for separate preview/apply intents. The
    /// file and canonical Match state are re-read and the complete dry-run is
    /// recomputed; no in-memory plan is trusted across commands.
    pub fn apply_identity_bundle_import_file(
        &self,
        path: &Path,
        relocations: &BTreeMap<String, String>,
        mode: IdentityImportMode,
        expected_plan_token: &str,
    ) -> Result<IdentityImportReceipt, String> {
        validate_sha256("identity import plan token", expected_plan_token)?;
        let plan = self.preview_identity_bundle_import(path, relocations, mode)?;
        if plan.plan_token != expected_plan_token {
            return Err("stale or mismatched identity import plan token".to_string());
        }
        self.apply_identity_bundle_import(plan)
    }

    /// Restart-safe rollback is an ordinary token-gated Replace import from
    /// the pre-import recovery bundle. This method exists to keep service code
    /// from depending on the process-local rollback optimization.
    pub fn rollback_identity_bundle_import_file(
        &self,
        recovery_path: &Path,
        relocations: &BTreeMap<String, String>,
        expected_plan_token: &str,
    ) -> Result<IdentityImportReceipt, String> {
        let receipt = self.apply_identity_bundle_import_file(
            recovery_path,
            relocations,
            IdentityImportMode::Replace,
            expected_plan_token,
        )?;
        let _guard = self.mutation_write_guard("Match restart rollback calibration")?;
        self.reactivate_rollback_calibrations_unlocked().map_err(|error| {
            format!(
                "identity recovery bundle restored portable identity state, but strict calibration recovery remained fail-closed: {error}"
            )
        })?;
        Ok(receipt)
    }

    pub fn apply_identity_bundle_import(
        &self,
        plan: IdentityImportPlan,
    ) -> Result<IdentityImportReceipt, String> {
        self.apply_identity_bundle_import_with_media_policy(plan, false)
    }

    pub(super) fn apply_identity_recovery_bundle_import(
        &self,
        plan: IdentityImportPlan,
    ) -> Result<IdentityImportReceipt, String> {
        self.apply_identity_bundle_import_with_media_policy(plan, true)
    }

    fn apply_identity_bundle_import_with_media_policy(
        &self,
        plan: IdentityImportPlan,
        allow_missing_media: bool,
    ) -> Result<IdentityImportReceipt, String> {
        validate_sha256("identity import plan token", &plan.plan_token)?;
        let recomputed_plan_token = identity_import_plan_token(&PlanTokenMaterial {
            content_sha256: &plan.content_sha256,
            current_state_sha256: &plan.current_state_sha256,
            mode: plan.mode.as_str(),
            relocations: &plan.relocations,
            relocated_media_sha256: &plan.relocated_media_sha256,
            creates: plan.creates,
            updates: plan.updates,
            identical: plan.identical,
            deletes: plan.deletes,
            conflicts: &plan.conflicts,
            unresolved_root_ids: &plan.unresolved_root_ids,
            unresolved_media_keys: &plan.unresolved_media_keys,
            already_applied: plan.already_applied,
            entity_count: plan.entity_count,
            reference_count: plan.reference_count,
        })?;
        if recomputed_plan_token != plan.plan_token {
            return Err("stale or mutated identity import plan token".to_string());
        }
        if !plan.unresolved_root_ids.is_empty() {
            let (sample, _) = bounded_receipt_sample(&plan.unresolved_root_ids);
            return Err(format!(
                "identity import has {} unresolved roots; sample: {}",
                plan.unresolved_root_ids.len(),
                sample.join(",")
            ));
        }
        if !allow_missing_media && !plan.unresolved_media_keys.is_empty() {
            let (sample, _) = bounded_receipt_sample(&plan.unresolved_media_keys);
            return Err(format!(
                "identity import has {} unresolved media keys; sample: {}",
                plan.unresolved_media_keys.len(),
                sample.join(",")
            ));
        }
        if plan.mode == IdentityImportMode::Merge && !plan.conflicts.is_empty() {
            return Err(
                "identity merge has conflicts; use the reported dry-run or explicit replace"
                    .to_string(),
            );
        }
        let _guard = self.mutation_write_guard("Match identity import")?;
        let media_resolution = resolve_relocated_media(&plan.relocated_graph)?;
        if !media_resolution.content_mismatches.is_empty() {
            let (sample, _) = bounded_receipt_sample(&media_resolution.content_mismatches);
            return Err(format!(
                "identity import media content changed after preview; {} keys no longer match their planned digests; sample: {}",
                media_resolution.content_mismatches.len(),
                sample.join(",")
            ));
        }
        if !allow_missing_media && !media_resolution.unresolved.is_empty() {
            let (sample, _) = bounded_receipt_sample(&media_resolution.unresolved);
            return Err(format!(
                "identity import media resolution changed after preview; {} keys are now unresolved; sample: {}",
                media_resolution.unresolved.len(),
                sample.join(",")
            ));
        }
        let observed_media_sha256 = media_resolution
            .evidence
            .iter()
            .map(|item| (item.media_key.clone(), item.observed_sha256.clone()))
            .collect::<BTreeMap<_, _>>();
        if observed_media_sha256 != plan.relocated_media_sha256 {
            return Err(
                "identity import media content changed after preview; observed digests no longer match the plan"
                    .to_string(),
            );
        }
        let current = self.collect_stored_identity_graph_unlocked()?;
        let current_raw_rows = stored_graph_rows(&current)?;
        let current_comparison_rows = stored_graph_portable_comparison_rows(&current)?;
        let current_state_sha256 = portable_rows_digest(&current_comparison_rows)?;
        if current_state_sha256 != plan.current_state_sha256 {
            return Err(
                "stale identity import plan: Match state changed after preview".to_string(),
            );
        }
        validate_relocated_identity_graph(&plan.relocated_graph)?;
        let desired_rows = bundle_graph_rows(&plan.relocated_graph)?;
        let (mut upserts, deletes, created, updated) =
            mutation_rows(&current_comparison_rows, &desired_rows, plan.mode)?;
        let changed = !upserts.is_empty() || !deletes.is_empty();
        let committed_execution = if changed {
            let execution = self.next_exchange_execution_unlocked()?;
            upserts.push(OwnedPortableRow {
                table: EXECUTION_TABLE.to_string(),
                stable_id: "global".to_string(),
                value: serde_json::to_value(&execution).map_err(|error| error.to_string())?,
            });
            execution
        } else {
            self.execution_state_unlocked()?
        };
        let pre_derived = self.capture_exchange_derived_snapshot_unlocked()?;
        let rollback = IdentityImportRollback {
            expected_post_state_sha256: String::new(),
            expected_post_derived_sha256: String::new(),
            pre_rows: current_raw_rows.clone(),
            pre_derived: pre_derived.clone(),
        };
        let current_raw_state_sha256 = portable_rows_digest(&current_raw_rows)?;
        let mut journal = self.prepare_identity_import_journal_unlocked(
            &plan,
            &current,
            &current_raw_state_sha256,
            &pre_derived,
        )?;
        #[cfg(test)]
        if REMOVE_NEXT_RESOLVED_MEDIA_BEFORE_IMPORT_COMMIT
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(item) = media_resolution.evidence.first() {
                if let Err(error) = fs::remove_file(&item.path) {
                    self.cleanup_uncommitted_import_recovery_files(&journal);
                    return Err(format!(
                        "identity import media changed at the pre-mutation boundary: injected media removal was refused by the retained evidence handle: {error}"
                    ));
                }
            }
        }
        if let Err(error) = revalidate_relocated_media_evidence(&media_resolution.evidence) {
            self.cleanup_uncommitted_import_recovery_files(&journal);
            return Err(format!(
                "identity import media changed at the pre-mutation boundary: {error}"
            ));
        }
        if let Err(error) = self
            .commit_owned_exchange_rows_with_import_journal_unlocked(&upserts, &deletes, &journal)
        {
            let recovery = self.recover_present_identity_import_journal_unlocked();
            if matches!(
                self.get_one_unlocked::<IdentityImportJournal>(
                    IDENTITY_IMPORT_JOURNAL_TABLE,
                    IDENTITY_IMPORT_JOURNAL_ID,
                ),
                Ok(None)
            ) {
                self.cleanup_uncommitted_import_recovery_files(&journal);
            }
            return Err(format!(
                "identity import portable transaction failed ({error}); journal recovery: {recovery:?}"
            ));
        }
        self.invalidate_match_caches_for_pending_import_unlocked();
        #[cfg(test)]
        if leave_import_journal_for_plan(&plan.plan_token) {
            return Err("injected identity import interruption after portable commit".to_string());
        }
        let trusted_search_rows = match self.reconcile_trusted_search_unlocked() {
            Ok(count) => count,
            Err(error) => {
                let recovery = self.recover_failed_import_with_journal_unlocked(
                    &current_raw_rows,
                    &pre_derived,
                    &journal,
                );
                return Err(format!(
                    "identity import trusted-search reconciliation failed ({error}); pre-import recovery: {recovery:?}"
                ));
            }
        };
        journal.phase = "derived_reconciled".to_string();
        journal.updated_at = now();
        if let Err(error) = self.store_identity_import_journal_unlocked(&journal) {
            let recovery = self.recover_failed_import_with_journal_unlocked(
                &current_raw_rows,
                &pre_derived,
                &journal,
            );
            return Err(format!(
                "identity import could not durably advance its recovery journal ({error}); pre-import recovery: {recovery:?}"
            ));
        }
        let post_check = (|| {
            let observed = self.collect_stored_identity_graph_unlocked()?;
            let observed_rows = stored_graph_portable_comparison_rows(&observed)?;
            let post_state_sha256 = portable_rows_digest(&observed_rows)?;
            Ok::<_, String>((observed_rows, post_state_sha256))
        })();
        let (observed_rows, post_state_sha256) = match post_check {
            Ok(observed) => observed,
            Err(error) => {
                let recovery = self.recover_failed_import_with_journal_unlocked(
                    &current_raw_rows,
                    &pre_derived,
                    &journal,
                );
                return Err(format!(
                    "identity import post-write verification failed ({error}); pre-import recovery: {recovery:?}"
                ));
            }
        };
        if !rows_equal(&observed_rows, &desired_rows, plan.mode) {
            let recovery = self.recover_failed_import_with_journal_unlocked(
                &current_raw_rows,
                &pre_derived,
                &journal,
            );
            return Err(format!(
                "identity import post-write reconciliation mismatch; pre-import recovery: {recovery:?}"
            ));
        }
        let mut rollback = rollback;
        rollback.expected_post_state_sha256 = post_state_sha256.clone();
        rollback.expected_post_derived_sha256 = match self
            .capture_exchange_derived_snapshot_unlocked()
            .and_then(|snapshot| exchange_derived_snapshot_digest(&snapshot))
        {
            Ok(digest) => digest,
            Err(error) => {
                let recovery = self.recover_failed_import_with_journal_unlocked(
                    &current_raw_rows,
                    &pre_derived,
                    &journal,
                );
                return Err(format!(
                    "identity import derived rollback-fence capture failed ({error}); pre-import recovery: {recovery:?}"
                ));
            }
        };
        #[cfg(test)]
        let injected_post_commit_error = if REMOVE_NEXT_RESOLVED_MEDIA_AFTER_IMPORT_COMMIT
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            media_resolution.evidence.first().and_then(|item| {
                fs::remove_file(&item.path).err().map(|error| {
                    format!(
                        "injected media removal was refused by the retained evidence handle: {error}"
                    )
                })
            })
        } else {
            None
        };
        #[cfg(test)]
        let media_validation = injected_post_commit_error.map_or_else(
            || revalidate_relocated_media_evidence(&media_resolution.evidence),
            Err,
        );
        #[cfg(not(test))]
        let media_validation = revalidate_relocated_media_evidence(&media_resolution.evidence);
        if let Err(error) = media_validation {
            let recovery = self.recover_failed_import_with_journal_unlocked(
                &current_raw_rows,
                &pre_derived,
                &journal,
            );
            return Err(format!(
                "identity import media changed during commit ({error}); pre-import recovery: {recovery:?}"
            ));
        }
        if changed {
            self.publish_exchange_revisions_to_cache(&committed_execution);
        }
        journal.phase = "completion_verified".to_string();
        journal.updated_at = now();
        if let Err(error) = self.store_identity_import_journal_unlocked(&journal) {
            let recovery = self.recover_failed_import_with_journal_unlocked(
                &current_raw_rows,
                &pre_derived,
                &journal,
            );
            return Err(format!(
                "identity import completion could not be journaled ({error}); pre-import recovery: {recovery:?}"
            ));
        }
        if let Err(error) = self.clear_identity_import_journal_unlocked(&journal) {
            if matches!(
                self.get_one_unlocked::<IdentityImportJournal>(
                    IDENTITY_IMPORT_JOURNAL_TABLE,
                    IDENTITY_IMPORT_JOURNAL_ID,
                ),
                Ok(None)
            ) {
                self.cleanup_uncommitted_import_recovery_files(&journal);
            } else {
                let recovery = self.recover_present_identity_import_journal_unlocked();
                return Err(format!(
                    "identity import completion journal could not be cleared ({error}); recovery: {recovery:?}"
                ));
            }
        }
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(IdentityImportReceipt {
            plan_token: plan.plan_token,
            content_sha256: plan.bundle.manifest.content_sha256,
            created,
            updated,
            deleted: deletes.len(),
            already_applied: plan.already_applied,
            reconciled_trusted_search_rows: trusted_search_rows,
            post_state_sha256,
            identity_revision: committed_execution.identity_revision,
            catalog_revision: committed_execution.catalog_revision,
            rollback,
        })
    }

    pub fn rollback_identity_bundle_import(
        &self,
        rollback: IdentityImportRollback,
    ) -> Result<IdentityReconciliation, String> {
        let _guard = self.mutation_write_guard("Match identity rollback")?;
        let current =
            stored_graph_portable_comparison_rows(&self.collect_stored_identity_graph_unlocked()?)?;
        if portable_rows_digest(&current)? != rollback.expected_post_state_sha256 {
            return Err("stale identity rollback: Match state changed after import".to_string());
        }
        let current_derived = self.capture_exchange_derived_snapshot_unlocked()?;
        if exchange_derived_snapshot_digest(&current_derived)?
            != rollback.expected_post_derived_sha256
        {
            return Err(
                "stale identity rollback: derived Match state changed after import".to_string(),
            );
        }
        self.restore_exchange_rows_unlocked(&rollback.pre_rows)?;
        let trusted_search_rows =
            self.restore_exchange_derived_snapshot_unlocked(&rollback.pre_derived)?;
        let observed = self.collect_stored_identity_graph_unlocked()?;
        let observed_rows = stored_graph_rows(&observed)?;
        if observed_rows != rollback.pre_rows {
            return Err(
                "identity rollback reconciliation did not restore the exact portable rows"
                    .to_string(),
            );
        }
        let graph = stored_to_bundle_graph(&observed, true)?;
        let content_sha256 = bundle_content_sha256(&graph)?;
        let entity_count = graph_entity_count(&graph);
        let reference_count = validate_identity_graph(&graph)?;
        let state_sha256 = portable_rows_digest(&observed_rows)?;
        let execution = self.execution_state_unlocked()?;
        self.publish_exchange_revisions_to_cache(&execution);
        drop(_guard);
        self.refresh_autocomplete_after_committed_catalog_change();
        Ok(IdentityReconciliation {
            content_sha256,
            state_sha256,
            entity_count,
            reference_count,
            trusted_search_rows,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    pub fn reconcile_identity_bundle(
        &self,
        expected: &IdentityBundleV1,
    ) -> Result<IdentityReconciliation, String> {
        #[cfg(test)]
        if FAIL_NEXT_IDENTITY_RECONCILIATION.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("injected explicit identity reconciliation failure".to_string());
        }
        validate_bundle(expected)?;
        let _guard = self.mutation_write_guard("Match identity reconciliation")?;
        let trusted_search_rows = self.reconcile_trusted_search_unlocked()?;
        let stored = self.collect_stored_identity_graph_unlocked()?;
        let graph = stored_to_bundle_graph(&stored, true)?;
        let content_sha256 = bundle_content_sha256(&graph)?;
        if content_sha256 != expected.manifest.content_sha256 {
            return Err(
                "identity reconciliation content hash differs from the expected bundle".to_string(),
            );
        }
        let rows = stored_graph_rows(&stored)?;
        let execution = self.execution_state_unlocked()?;
        Ok(IdentityReconciliation {
            content_sha256,
            state_sha256: portable_rows_digest(&rows)?,
            entity_count: graph_entity_count(&graph),
            reference_count: validate_identity_graph(&graph)?,
            trusted_search_rows,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        })
    }

    pub fn preview_mwg_xmp_sidecar(
        &self,
        media_key: &str,
        sidecar_path: &Path,
    ) -> Result<MwgXmpSidecarPreview, String> {
        validate_exchange_media_key(media_key)?;
        validate_xmp_sidecar_path(sidecar_path, false)?;
        let _guard = self.database_read_guard("Match XMP projection read lock is poisoned")?;
        let stored = self.collect_stored_identity_graph_unlocked()?;
        let people = stored
            .people
            .iter()
            .map(|row| (row.person_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        let assignments = stored
            .assignments
            .iter()
            .map(|row| (row.face_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        let mut regions = Vec::new();
        let mut unsupported = Vec::new();
        let mut applied_dimensions = None;
        for face in stored.faces.iter().filter(|row| row.media_key == media_key) {
            let Some(assignment) = assignments.get(face.face_id.as_str()) else {
                unsupported.push(XmpUnsupportedSemantic {
                    code: "unassigned_face_not_projected".to_string(),
                    stable_id: face.face_id.clone(),
                });
                continue;
            };
            let person = people.get(assignment.person_id.as_str()).ok_or_else(|| {
                format!(
                    "XMP projection found dangling Person {}",
                    assignment.person_id
                )
            })?;
            let bounds: [f32; 4] = face.bounds_normalized.as_slice().try_into().map_err(|_| {
                format!("Face {} does not have four normalized bounds", face.face_id)
            })?;
            let dimensions = match (face.source_width, face.source_height) {
                (Some(width), Some(height)) if width > 0 && height > 0 => [width, height],
                _ => {
                    unsupported.push(XmpUnsupportedSemantic {
                        code: "source_dimensions_not_projected".to_string(),
                        stable_id: face.face_id.clone(),
                    });
                    continue;
                }
            };
            if applied_dimensions.is_some_and(|current| current != dimensions) {
                unsupported.push(XmpUnsupportedSemantic {
                    code: "source_dimensions_mismatch_not_projected".to_string(),
                    stable_id: face.face_id.clone(),
                });
                continue;
            }
            applied_dimensions.get_or_insert(dimensions);
            regions.push(MwgXmpRegion {
                face_id: face.face_id.clone(),
                person_id: person.person_id.clone(),
                name: person.name.clone(),
                bounds_normalized: bounds,
            });
            unsupported.push(XmpUnsupportedSemantic {
                code: "assignment_evidence_and_look_not_projected".to_string(),
                stable_id: assignment.assignment_id.clone(),
            });
        }
        regions.sort_by(|left, right| left.face_id.cmp(&right.face_id));
        unsupported.sort_by(|left, right| {
            (&left.code, &left.stable_id).cmp(&(&right.code, &right.stable_id))
        });
        let canonical_xml = render_mwg_xmp(&regions, applied_dimensions)?;
        // XMP stores region centers while Match stores top-left bounds. Return
        // the exact canonical decode that import will observe, including the
        // unavoidable sub-epsilon f32 subtraction at that representation edge.
        let regions = parse_mwg_xmp(&canonical_xml)?.regions;
        if canonical_xml.len() > IDENTITY_XMP_MAX_SIDECAR_BYTES {
            return Err(format!(
                "MWG XMP sidecar is {} bytes; limit is {IDENTITY_XMP_MAX_SIDECAR_BYTES}",
                canonical_xml.len()
            ));
        }
        let content_sha256 = sha256_bytes(&canonical_xml);
        let token = sha256_bytes(
            serde_json::to_vec(&(
                media_key,
                normalized_host_path(sidecar_path),
                &content_sha256,
            ))
            .map_err(|error| error.to_string())?
            .as_slice(),
        );
        Ok(MwgXmpSidecarPreview {
            preview_token: token,
            media_key: media_key.to_string(),
            sidecar_path: sidecar_path.to_path_buf(),
            content_sha256,
            sidecar_bytes: canonical_xml.len(),
            regions,
            unsupported,
            canonical_xml,
        })
    }

    pub fn export_mwg_xmp_sidecar(
        &self,
        media_key: &str,
        sidecar_path: &Path,
        expected_preview_token: &str,
    ) -> Result<MwgXmpSidecarReceipt, String> {
        validate_sha256("XMP preview token", expected_preview_token)?;
        let preview = self.preview_mwg_xmp_sidecar(media_key, sidecar_path)?;
        if preview.preview_token != expected_preview_token {
            return Err("stale or mismatched XMP sidecar preview token".to_string());
        }
        let publication = write_new_regular_file(
            sidecar_path,
            &preview.canonical_xml,
            IDENTITY_XMP_MAX_SIDECAR_BYTES,
        )?;
        Ok(MwgXmpSidecarReceipt {
            preview_token: preview.preview_token,
            media_key: preview.media_key,
            sidecar_path: preview.sidecar_path,
            content_sha256: preview.content_sha256,
            sidecar_bytes: preview.sidecar_bytes,
            original_media_rows_mutated: 0,
            publication_warning: publication.warning,
        })
    }

    pub fn preview_mwg_xmp_import(path: &Path) -> Result<MwgXmpImportPreview, String> {
        validate_xmp_sidecar_path(path, true)?;
        let bytes =
            read_bounded_regular_file(path, IDENTITY_XMP_MAX_SIDECAR_BYTES, "MWG XMP sidecar")?;
        let parsed = parse_mwg_xmp(&bytes)?;
        let regions = parsed.regions;
        let content_sha256 = sha256_bytes(&bytes);
        let preview_token = sha256_bytes(
            serde_json::to_vec(&(normalized_host_path(path), &content_sha256, &regions))
                .map_err(|error| error.to_string())?
                .as_slice(),
        );
        Ok(MwgXmpImportPreview {
            preview_token,
            sidecar_path: path.to_path_buf(),
            content_sha256,
            sidecar_bytes: bytes.len(),
            regions,
            unsupported: parsed
                .unsupported
                .into_iter()
                .chain(std::iter::once(XmpUnsupportedSemantic {
                    code: "xmp_has_no_facial_evidence_state_or_trust_authority".to_string(),
                    stable_id: "sidecar".to_string(),
                }))
                .collect(),
        })
    }

    pub fn import_mwg_xmp_sidecar(
        path: &Path,
        expected_preview_token: &str,
    ) -> Result<MwgXmpImportReceipt, String> {
        validate_sha256("XMP import preview token", expected_preview_token)?;
        let preview = Self::preview_mwg_xmp_import(path)?;
        if preview.preview_token != expected_preview_token {
            return Err("stale or mismatched XMP import preview token".to_string());
        }
        Ok(MwgXmpImportReceipt {
            preview_token: preview.preview_token,
            sidecar_path: preview.sidecar_path,
            content_sha256: preview.content_sha256,
            staged_regions: preview.regions,
            applied_match_rows: 0,
            original_media_rows_mutated: 0,
        })
    }

    fn build_identity_bundle(&self) -> Result<IdentityBundleV1, String> {
        let _guard = self.database_read_guard("Match identity export read lock is poisoned")?;
        self.build_identity_bundle_unlocked()
    }

    fn build_identity_bundle_unlocked(&self) -> Result<IdentityBundleV1, String> {
        let mut stored = self.collect_stored_identity_graph_unlocked()?;
        self.enrich_legacy_suggestion_source_provenance_unlocked(&mut stored)?;
        let graph = stored_to_bundle_graph(&stored, true)?;
        validate_identity_graph(&graph)?;
        let content_sha256 = bundle_content_sha256(&graph)?;
        let bundle = IdentityBundleV1 {
            manifest: IdentityBundleManifest {
                format: IDENTITY_BUNDLE_FORMAT.to_string(),
                version: IDENTITY_BUNDLE_VERSION,
                schema_version: MATCH_SCHEMA_VERSION,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                content_sha256,
            },
            graph,
        };
        enforce_canonical_bundle_size(&bundle)?;
        Ok(bundle)
    }

    pub(super) fn current_identity_bundle_snapshot_unlocked(
        &self,
    ) -> Result<(IdentityBundleV1, IdentityReconciliation), String> {
        let bundle = self.build_identity_bundle_unlocked()?;
        let stored = self.collect_stored_identity_graph_unlocked()?;
        let rows = stored_graph_rows(&stored)?;
        let execution = self.execution_state_unlocked()?;
        let reconciliation = IdentityReconciliation {
            content_sha256: bundle.manifest.content_sha256.clone(),
            state_sha256: portable_rows_digest(&rows)?,
            entity_count: graph_entity_count(&bundle.graph),
            reference_count: validate_identity_graph(&bundle.graph)?,
            trusted_search_rows: self.count_unlocked_exchange(TRUSTED_SEARCH_TABLE)?,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        };
        Ok((bundle, reconciliation))
    }

    /// Recovery seam: the caller owns the Match transaction read lock. The
    /// exact portable graph is built, published create-new, reread through the
    /// hostile-input path, and independently compared without taking another
    /// lock or mutating derived trusted-search state.
    pub(super) fn export_recovery_bundle_snapshot_unlocked(
        &self,
        path: &Path,
    ) -> Result<
        (
            IdentityBundleV1,
            IdentityBundleExportReceipt,
            IdentityReconciliation,
        ),
        String,
    > {
        let bundle = self.build_identity_bundle_unlocked()?;
        let preview = export_preview(&bundle)?;
        let bytes = canonical_bundle_bytes(&bundle)?;
        preflight_canonical_identity_bundle_bytes(&bytes)?;
        publish_recovery_bytes(path, &bytes)?;
        let reread = Self::read_identity_bundle(path)?;
        if reread != bundle
            || reread.manifest.content_sha256 != preview.content_sha256
            || canonical_bundle_bytes(&reread)?.len() != preview.canonical_bytes
        {
            return Err(
                "recovery bundle reread differs from the locked Match snapshot".to_string(),
            );
        }
        let mut stored = self.collect_stored_identity_graph_unlocked()?;
        let rows = stored_graph_rows(&stored)?;
        self.enrich_legacy_suggestion_source_provenance_unlocked(&mut stored)?;
        let current_graph = stored_to_bundle_graph(&stored, true)?;
        let current_content_sha256 = bundle_content_sha256(&current_graph)?;
        if current_content_sha256 != reread.manifest.content_sha256 {
            return Err(
                "recovery bundle reconciliation differs from current portable graph".to_string(),
            );
        }
        let execution = self.execution_state_unlocked()?;
        let reconciliation = IdentityReconciliation {
            content_sha256: current_content_sha256,
            state_sha256: portable_rows_digest(&rows)?,
            entity_count: graph_entity_count(&current_graph),
            reference_count: validate_identity_graph(&current_graph)?,
            trusted_search_rows: self.count_unlocked_exchange(TRUSTED_SEARCH_TABLE)?,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
        };
        let receipt = IdentityBundleExportReceipt {
            output_path: path.to_path_buf(),
            content_sha256: preview.content_sha256,
            canonical_bytes: preview.canonical_bytes,
            entity_count: preview.entity_count,
            publication_warning: None,
        };
        Ok((reread, receipt, reconciliation))
    }

    fn plan_identity_bundle_import(
        &self,
        bundle: IdentityBundleV1,
        relocations: &BTreeMap<String, String>,
        mode: IdentityImportMode,
    ) -> Result<IdentityImportPlan, String> {
        validate_bundle(&bundle)?;
        let mut relocated_graph = bundle.graph.clone();
        let mut unresolved_root_ids = Vec::new();
        for root in &mut relocated_graph.roots {
            match relocations.get(&root.root_id) {
                Some(path) => {
                    root.portable_path = canonical_relocation_root(path)?;
                }
                None => unresolved_root_ids.push(root.root_id.clone()),
            }
        }
        unresolved_root_ids.sort();
        let media_resolution = resolve_relocated_media(&relocated_graph)?;
        let mut unresolved_media_keys = media_resolution.unresolved;
        unresolved_media_keys.extend(media_resolution.content_mismatches.iter().cloned());
        unresolved_media_keys.sort();
        let relocated_media_sha256 = media_resolution
            .evidence
            .into_iter()
            .map(|item| (item.media_key, item.observed_sha256))
            .collect::<BTreeMap<_, _>>();
        let _guard = self.database_read_guard("Match identity import preview lock is poisoned")?;
        let current = self.collect_stored_identity_graph_unlocked()?;
        let current_rows = stored_graph_portable_comparison_rows(&current)?;
        let desired_rows = bundle_graph_rows(&relocated_graph)?;
        let current_by_key = row_map(&current_rows);
        let desired_by_key = row_map(&desired_rows);
        let mut creates = 0;
        let mut updates = 0;
        let mut identical = 0;
        let mut conflicts = Vec::new();
        for (key, desired) in &desired_by_key {
            match current_by_key.get(key) {
                None => creates += 1,
                Some(current) if current.value == desired.value => identical += 1,
                Some(_) => {
                    updates += 1;
                    conflicts.push(IdentityImportConflict {
                        table: key.0.clone(),
                        stable_id: key.1.clone(),
                    });
                }
            }
        }
        conflicts.sort_by(|left, right| {
            (&left.table, &left.stable_id).cmp(&(&right.table, &right.stable_id))
        });
        let deletes = if mode == IdentityImportMode::Replace {
            current_by_key
                .keys()
                .filter(|key| !desired_by_key.contains_key(*key))
                .count()
        } else {
            0
        };
        let current_state_sha256 = portable_rows_digest(&current_rows)?;
        let reference_count = validate_identity_graph(&bundle.graph)?;
        let already_applied = creates == 0 && updates == 0 && deletes == 0;
        let entity_count = graph_entity_count(&bundle.graph);
        let plan_token = identity_import_plan_token(&PlanTokenMaterial {
            content_sha256: &bundle.manifest.content_sha256,
            current_state_sha256: &current_state_sha256,
            mode: mode.as_str(),
            relocations,
            relocated_media_sha256: &relocated_media_sha256,
            creates,
            updates,
            identical,
            deletes,
            conflicts: &conflicts,
            unresolved_root_ids: &unresolved_root_ids,
            unresolved_media_keys: &unresolved_media_keys,
            already_applied,
            entity_count,
            reference_count,
        })?;
        Ok(IdentityImportPlan {
            plan_token,
            content_sha256: bundle.manifest.content_sha256.clone(),
            mode,
            creates,
            updates,
            identical,
            deletes,
            conflicts,
            unresolved_root_ids,
            unresolved_media_keys,
            already_applied,
            entity_count,
            reference_count,
            bundle,
            relocated_graph,
            relocations: relocations.clone(),
            relocated_media_sha256,
            current_state_sha256,
        })
    }

    fn collect_stored_identity_graph_unlocked(&self) -> Result<StoredIdentityGraph, String> {
        let counts = PORTABLE_TABLES
            .iter()
            .map(|table| self.count_unlocked_exchange(table))
            .collect::<Result<Vec<_>, _>>()?;
        let total = counts.into_iter().try_fold(0usize, |total, count| {
            total
                .checked_add(count)
                .ok_or_else(|| "identity entity count overflow".to_string())
        })?;
        if total > IDENTITY_BUNDLE_MAX_ENTITIES {
            return Err(format!(
                "identity graph has {total} entities; limit is {IDENTITY_BUNDLE_MAX_ENTITIES}"
            ));
        }
        let mut stored = StoredIdentityGraph {
            video_observations: self.list_unlocked(super::video::VIDEO_OBSERVATION_TABLE)?,
            review_context: self.list_unlocked(super::context::CONTEXT_TABLE)?,
            media_context: self.list_unlocked(super::context::MEDIA_CONTEXT_TABLE)?,
            people: self.list_unlocked(PERSON_TABLE)?,
            looks: self.list_unlocked(LOOK_TABLE)?,
            trusted_template_sets: self.list_unlocked(TEMPLATE_SET_TABLE)?,
            trusted_memberships: self.list_unlocked(TRUSTED_MEMBER_TABLE)?,
            faces: self.list_unlocked(FACE_TABLE)?,
            assignments: self.list_unlocked(ASSIGNMENT_TABLE)?,
            suggestions: self.list_unlocked(SUGGESTION_TABLE)?,
            constraints: self.list_unlocked(CONSTRAINT_TABLE)?,
            dispositions: self.list_unlocked(FACE_DISPOSITION_TABLE)?,
            operations: self.list_unlocked(OPERATION_TABLE)?,
            correction_media_operations: self.list_unlocked(CORRECTION_MEDIA_OPERATION_TABLE)?,
            suggestion_source_provenance: self.list_unlocked(SUGGESTION_SOURCE_PROVENANCE_TABLE)?,
            roots: self.list_unlocked(ROOT_CONFIG_TABLE)?,
            model_generations: self.list_unlocked(GENERATION_TABLE)?,
        };
        sort_stored_graph(&mut stored);
        Ok(stored)
    }

    fn enrich_legacy_suggestion_source_provenance_unlocked(
        &self,
        stored: &mut StoredIdentityGraph,
    ) -> Result<(), String> {
        let mut existing = stored
            .suggestion_source_provenance
            .iter()
            .map(|row| (row.operation_id.clone(), row.suggestion_id.clone()))
            .collect::<BTreeSet<_>>();
        let operations = stored.operations.clone();
        let mut indexed_operations = BTreeMap::new();
        let mut provenance_work = 0usize;
        for operation in &operations {
            charge_legacy_provenance_work(&mut provenance_work, 1)?;
            if let Some(indexed) = index_correction_envelope(operation)? {
                charge_legacy_provenance_work(&mut provenance_work, indexed.envelope.rows.len())?;
                indexed_operations.insert(operation.operation_id.clone(), indexed);
            }
        }
        let history = legacy_suggestion_history(&operations, &indexed_operations)?;
        let lookup = build_legacy_suggestion_lookup(stored, &history, &mut provenance_work)?;
        for operation in &operations {
            charge_legacy_provenance_work(&mut provenance_work, 1)?;
            if !matches!(
                operation.kind.as_str(),
                "correction_same"
                    | "correction_batch_same"
                    | "correction_different"
                    | "correction_batch_different"
                    | "correction_change_person"
                    | "correction_batch_change_person"
            ) {
                continue;
            }
            let indexed = indexed_operations
                .get(&operation.operation_id)
                .ok_or_else(|| {
                    format!(
                        "operation {} lacks a correction envelope during provenance enrichment",
                        operation.operation_id
                    )
                })?;
            for row in indexed.envelope.rows.iter().filter(|row| {
                row.table == CorrectionTable::Suggestion
                    && row.before.is_some()
                    && row.after.is_none()
            }) {
                let suggestion: Suggestion = decode_strict_typed_value(
                    row.before
                        .as_ref()
                        .expect("filtered Suggestion before value"),
                    "legacy SuggestionSourceProvenance source",
                )?;
                if !suggestion_is_provenance_source(
                    operation.kind.trim_start_matches("correction_"),
                    &indexed.envelope.rows,
                    &suggestion,
                ) {
                    continue;
                }
                let key = (
                    operation.operation_id.clone(),
                    suggestion.suggestion_id.clone(),
                );
                if existing.contains(&key) {
                    continue;
                }
                let provenance = self.legacy_suggestion_source_provenance_unlocked(
                    &lookup,
                    &mut provenance_work,
                    operation,
                    &suggestion,
                )?;
                stored.suggestion_source_provenance.push(provenance);
                existing.insert(key);
            }
        }
        stored
            .suggestion_source_provenance
            .sort_by(|a, b| a.provenance_id.cmp(&b.provenance_id));
        Ok(())
    }

    fn legacy_suggestion_source_provenance_unlocked(
        &self,
        lookup: &LegacySuggestionLookup,
        provenance_work: &mut usize,
        operation: &MatchOperation,
        suggestion: &Suggestion,
    ) -> Result<SuggestionSourceProvenance, String> {
        let embedding_key = embedding_id(&suggestion.face_id, &suggestion.model_generation);
        let current_face = lookup
            .current_faces
            .get(suggestion.face_id.as_str())
            .filter(|face| {
                face.face_revision == suggestion.face_revision
                    && face.media_fingerprint == suggestion.media_fingerprint
            });
        let current_person = lookup
            .current_people
            .get(suggestion.candidate_person_id.as_str())
            .filter(|person| person.revision >= suggestion.person_revision);
        let current_embedding = self
            .get_one_unlocked::<FaceEmbedding>(EMBEDDING_TABLE, &embedding_key)?
            .filter(|embedding| {
                embedding.face_id == suggestion.face_id
                    && embedding.face_revision == suggestion.face_revision
                    && embedding.media_fingerprint == suggestion.media_fingerprint
                    && embedding.model_generation == suggestion.model_generation
                    && embedding.job_id == suggestion.job_id
                    && embedding.schema_generation == MATCH_SCHEMA_GENERATION
                    && embedding.active
            });

        let source_time = chrono::DateTime::parse_from_rfc3339(&operation.created_at)
            .map_err(|error| format!("legacy provenance operation time is invalid: {error}"))?;
        let anchor_key = LegacyFaceEmbeddingAnchorKey {
            face_id: suggestion.face_id.clone(),
            embedding_id: embedding_key.clone(),
            face_revision: suggestion.face_revision,
            media_fingerprint: suggestion.media_fingerprint.clone(),
            model_generation: suggestion.model_generation.clone(),
            job_id: suggestion.job_id.clone(),
        };
        let historical_face_embeddings = lookup
            .historical_face_embeddings
            .get(&anchor_key)
            .into_iter()
            .flatten()
            .filter(|(later_time, face, embedding)| {
                *later_time >= source_time
                    && embedding.face_id == suggestion.face_id
                    && embedding.face_revision == suggestion.face_revision
                    && embedding.media_fingerprint == suggestion.media_fingerprint
                    && embedding.schema_generation == MATCH_SCHEMA_GENERATION
                    && embedding.active
            })
            .map(|(_, face, embedding)| (face.clone(), embedding.clone()))
            .collect::<Vec<_>>();
        let historical_people = lookup
            .historical_people
            .get(&suggestion.candidate_person_id)
            .into_iter()
            .flatten()
            .filter(|(later_time, person)| {
                *later_time >= source_time && person.revision >= suggestion.person_revision
            })
            .collect::<Vec<_>>();
        charge_legacy_provenance_work(
            provenance_work,
            historical_face_embeddings
                .len()
                .checked_add(historical_people.len())
                .and_then(|value| value.checked_add(1))
                .ok_or("legacy suggestion provenance work overflow")?,
        )?;
        let (face, embedding) = match (current_face, current_embedding.as_ref()) {
            (Some(face), Some(embedding)) => (face.clone(), embedding.clone()),
            _ if historical_face_embeddings.len() == 1 => historical_face_embeddings[0].clone(),
            _ => {
                return Err(format!(
                    "operation {} cannot independently anchor legacy Suggestion {} Face/Embedding provenance",
                    operation.operation_id, suggestion.suggestion_id
                ))
            }
        };
        if current_person.is_none() && historical_people.is_empty() {
            return Err(format!(
                "operation {} cannot independently anchor legacy Suggestion {} Person provenance",
                operation.operation_id, suggestion.suggestion_id
            ));
        }
        let mut provenance = SuggestionSourceProvenance {
            provenance_id: String::new(),
            operation_id: operation.operation_id.clone(),
            operation_kind: operation.kind.trim_start_matches("correction_").to_string(),
            suggestion_id: suggestion.suggestion_id.clone(),
            face_id: suggestion.face_id.clone(),
            candidate_person_id: suggestion.candidate_person_id.clone(),
            media_key: face.media_key,
            media_fingerprint: suggestion.media_fingerprint.clone(),
            face_revision: suggestion.face_revision,
            person_revision: suggestion.person_revision,
            model_generation: suggestion.model_generation.clone(),
            job_id: suggestion.job_id.clone(),
            suggestion_created_at: suggestion.created_at.clone(),
            similarity_bits: suggestion.similarity.to_bits(),
            calibration_generation: suggestion.calibration_generation.clone(),
            envelope_hash: suggestion.envelope_hash.clone(),
            embedding_id: embedding.embedding_id,
            embedding_created_at: embedding.created_at,
            schema_generation: embedding.schema_generation,
            operation_created_at: operation.created_at.clone(),
        };
        provenance.provenance_id = suggestion_source_provenance_id(&provenance);
        Ok(provenance)
    }

    fn count_unlocked_exchange(&self, table: &str) -> Result<usize, String> {
        let db = self.database();
        let sql = format!("SELECT count() AS count FROM {table} GROUP ALL;");
        let rows: Vec<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .await
                .map_err(|error| format!("count identity table {table}: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode identity table count {table}: {error}"))
        })?;
        let count = rows
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        usize::try_from(count).map_err(|_| "identity table count exceeds usize".to_string())
    }

    fn commit_owned_exchange_rows_unlocked(
        &self,
        upserts: &[OwnedPortableRow],
        deletes: &[OwnedPortableRow],
    ) -> Result<(), String> {
        let borrowed_upserts = upserts
            .iter()
            .map(|row| {
                (
                    row.table.as_str(),
                    row.stable_id.as_str(),
                    row.value.clone(),
                )
            })
            .collect::<Vec<_>>();
        let borrowed_deletes = deletes
            .iter()
            .map(|row| (row.table.as_str(), row.stable_id.as_str()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&borrowed_upserts, &borrowed_deletes)
    }

    fn prepare_identity_import_journal_unlocked(
        &self,
        plan: &IdentityImportPlan,
        current: &StoredIdentityGraph,
        current_state_sha256: &str,
        pre_derived: &ExchangeDerivedSnapshot,
    ) -> Result<IdentityImportJournal, String> {
        self.filesystem_recovery_ready
            .store(false, Ordering::Release);
        if self
            .get_one_unlocked::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )?
            .is_some()
        {
            return Err(
                "an interrupted identity import must be recovered before another import"
                    .to_string(),
            );
        }
        validate_sha256("identity import plan token", &plan.plan_token)?;
        validate_sha256("identity import pre-state digest", current_state_sha256)?;
        let state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no app-state parent")?;
        let state_root = absolute_lexical_path(state_root, "identity import recovery root")?;
        let recovery_id = uuid::Uuid::new_v4().simple();
        let recovery_path = state_root.join(format!(
            "{IDENTITY_IMPORT_RECOVERY_PREFIX}{recovery_id}.facial-identity.json"
        ));
        let derived_path = state_root.join(format!(
            "{IDENTITY_IMPORT_RECOVERY_PREFIX}{recovery_id}{IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX}"
        ));
        let (_, receipt, reconciliation) =
            match self.export_recovery_bundle_snapshot_unlocked(&recovery_path) {
                Ok(result) => result,
                Err(error) => {
                    cleanup_recovery_path(&recovery_path);
                    return Err(error);
                }
            };
        if receipt.publication_warning.is_some()
            || reconciliation.state_sha256 != current_state_sha256
        {
            cleanup_recovery_path(&recovery_path);
            return Err(
                "identity import recovery bundle lacks exact durable pre-state evidence"
                    .to_string(),
            );
        }
        let snapshot = match Self::read_identity_bundle_snapshot(&recovery_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                cleanup_recovery_path(&recovery_path);
                return Err(error);
            }
        };
        if snapshot.bundle.manifest.content_sha256 != receipt.content_sha256 {
            drop(snapshot);
            cleanup_recovery_path(&recovery_path);
            return Err("identity import recovery bundle content binding changed".to_string());
        }
        let derived = match self.publish_identity_import_derived_recovery(
            &derived_path,
            pre_derived,
            current,
        ) {
            Ok(binding) => binding,
            Err(error) => {
                drop(snapshot);
                cleanup_recovery_path(&recovery_path);
                cleanup_recovery_path(&derived_path);
                return Err(error);
            }
        };
        let roots = current
            .roots
            .iter()
            .map(|root| (root.root_id.clone(), root.path.clone()))
            .collect::<BTreeMap<_, _>>();
        let persisted_suggestion_provenance_ids = current
            .suggestion_source_provenance
            .iter()
            .map(|row| row.provenance_id.clone())
            .collect();
        let evidence = IdentityImportRecoveryEvidence {
            roots,
            derived,
            persisted_suggestion_provenance_ids,
        };
        let relocations_json = match serde_json::to_string(&evidence) {
            Ok(json) => json,
            Err(error) => {
                drop(snapshot);
                cleanup_recovery_path(&recovery_path);
                cleanup_recovery_path(&derived_path);
                return Err(format!(
                    "serialize identity recovery root evidence: {error}"
                ));
            }
        };
        if relocations_json.len() > IDENTITY_BUNDLE_MAX_STRING_BYTES {
            drop(snapshot);
            cleanup_recovery_path(&recovery_path);
            cleanup_recovery_path(&derived_path);
            return Err("identity recovery root evidence exceeds its bounded size".to_string());
        }
        let recovery_path = match snapshot.canonical_path.to_str() {
            Some(path) => path.to_string(),
            None => {
                drop(snapshot);
                cleanup_recovery_path(&recovery_path);
                cleanup_recovery_path(&derived_path);
                return Err("identity recovery path is not UTF-8".to_string());
            }
        };
        let created_at = now();
        Ok(IdentityImportJournal {
            journal_id: IDENTITY_IMPORT_JOURNAL_ID.to_string(),
            version: IDENTITY_IMPORT_JOURNAL_VERSION,
            plan_token: plan.plan_token.clone(),
            pre_state_sha256: current_state_sha256.to_string(),
            recovery_path,
            recovery_file_sha256: snapshot.file_sha256.clone(),
            recovery_content_sha256: receipt.content_sha256,
            relocations_json,
            phase: "portable_committed".to_string(),
            created_at: created_at.clone(),
            updated_at: created_at,
        })
    }

    fn commit_owned_exchange_rows_with_import_journal_unlocked(
        &self,
        upserts: &[OwnedPortableRow],
        deletes: &[OwnedPortableRow],
        journal: &IdentityImportJournal,
    ) -> Result<(), String> {
        let journal_value = serde_json::to_value(journal).map_err(|error| error.to_string())?;
        let mut borrowed_upserts = Vec::with_capacity(upserts.len() + 1);
        borrowed_upserts.push((
            IDENTITY_IMPORT_JOURNAL_TABLE,
            IDENTITY_IMPORT_JOURNAL_ID,
            journal_value,
        ));
        borrowed_upserts.extend(upserts.iter().map(|row| {
            (
                row.table.as_str(),
                row.stable_id.as_str(),
                row.value.clone(),
            )
        }));
        let borrowed_deletes = deletes
            .iter()
            .map(|row| (row.table.as_str(), row.stable_id.as_str()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&borrowed_upserts, &borrowed_deletes)
    }

    fn store_identity_import_journal_unlocked(
        &self,
        journal: &IdentityImportJournal,
    ) -> Result<(), String> {
        self.transactional_upserts_deletes_unlocked(
            &[((
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
                serde_json::to_value(journal).map_err(|error| error.to_string())?,
            ))],
            &[],
        )
    }

    fn clear_identity_import_journal_unlocked(
        &self,
        journal: &IdentityImportJournal,
    ) -> Result<(), String> {
        let observed = self
            .get_one_unlocked::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )?
            .ok_or("identity import recovery journal disappeared before completion")?;
        if observed != *journal {
            return Err("identity import recovery journal changed before completion".to_string());
        }
        self.transactional_upserts_deletes_unlocked(
            &[],
            &[(IDENTITY_IMPORT_JOURNAL_TABLE, IDENTITY_IMPORT_JOURNAL_ID)],
        )?;
        self.cleanup_uncommitted_import_recovery_files(journal);
        self.reconcile_identity_import_recovery_artifacts_unlocked()?;
        self.filesystem_recovery_ready
            .store(true, Ordering::Release);
        Ok(())
    }

    fn recover_failed_import_with_journal_unlocked(
        &self,
        rows: &[OwnedPortableRow],
        derived: &ExchangeDerivedSnapshot,
        journal: &IdentityImportJournal,
    ) -> Result<(), String> {
        self.invalidate_match_caches_for_pending_import_unlocked();
        let recovery = (|| {
            let mut recovering = journal.clone();
            recovering.phase = "rollback_started".to_string();
            recovering.updated_at = now();
            self.store_identity_import_journal_unlocked(&recovering)?;
            #[cfg(test)]
            if fail_import_rollback_after_portable_restore_for_plan(&journal.plan_token) {
                self.restore_exchange_rows_unlocked(rows)?;
                return Err(
                    "injected identity import derived rollback failure after portable restore"
                        .to_string(),
                );
            }
            self.recover_failed_import_unlocked(rows, derived)?;
            let observed_rows = stored_graph_rows(&self.collect_stored_identity_graph_unlocked()?)?;
            if observed_rows != rows
                || portable_rows_digest(&observed_rows)? != journal.pre_state_sha256
            {
                return Err(
                    "identity import recovery did not restore exact portable state".to_string(),
                );
            }
            let observed_derived = self.capture_exchange_derived_snapshot_unlocked()?;
            if exchange_derived_snapshot_digest(&observed_derived)?
                != exchange_derived_snapshot_digest(derived)?
            {
                return Err(
                    "identity import recovery did not restore exact derived state".to_string(),
                );
            }
            recovering.phase = "rollback_verified".to_string();
            recovering.updated_at = now();
            self.store_identity_import_journal_unlocked(&recovering)?;
            self.clear_identity_import_journal_unlocked(&recovering)?;
            let execution = self.execution_state_unlocked()?;
            self.publish_exchange_revisions_to_cache(&execution);
            Ok(())
        })();
        if recovery.is_err() {
            self.invalidate_match_caches_for_pending_import_unlocked();
        }
        recovery
    }

    fn recover_present_identity_import_journal_unlocked(&self) -> Result<(), String> {
        self.invalidate_match_caches_for_pending_import_unlocked();
        match self.get_one_unlocked::<IdentityImportJournal>(
            IDENTITY_IMPORT_JOURNAL_TABLE,
            IDENTITY_IMPORT_JOURNAL_ID,
        )? {
            Some(journal) => self.recover_identity_import_journal_record_unlocked(journal),
            None => Ok(()),
        }
    }

    pub(super) fn recover_pending_identity_import_before_mutation_unlocked(
        &self,
    ) -> Result<(), String> {
        if self
            .get_one_unlocked::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )?
            .is_some()
        {
            self.recover_present_identity_import_journal_unlocked()
                .map_err(|error| {
                    format!("pending identity import must recover before a Match mutation: {error}")
                })?;
        }
        if self
            .get_one_unlocked::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )?
            .is_some()
        {
            return Err(
                "pending identity import remains unresolved before a Match mutation".to_string(),
            );
        }
        Ok(())
    }

    pub(super) fn require_no_pending_identity_import_unlocked(&self) -> Result<(), String> {
        if self
            .get_one_unlocked::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )?
            .is_some()
        {
            return Err("Match filesystem recovery is required before automatic admission".into());
        }
        Ok(())
    }

    pub(super) fn recover_interrupted_identity_import(&self) -> Result<(), String> {
        let _guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| "Match identity import recovery lock is poisoned".to_string())?;
        self.recover_present_identity_import_journal_unlocked()?;
        self.reconcile_identity_import_recovery_artifacts_unlocked()?;
        Ok(())
    }

    fn recover_identity_import_journal_record_unlocked(
        &self,
        mut journal: IdentityImportJournal,
    ) -> Result<(), String> {
        self.invalidate_match_caches_for_pending_import_unlocked();
        let recovery = (|| {
            validate_identity_import_journal(&journal)?;
            let recovery_path = self.identity_import_recovery_path(&journal)?;
            let snapshot = Self::read_identity_bundle_snapshot(&recovery_path)?;
            if snapshot.file_sha256 != journal.recovery_file_sha256
                || snapshot.bundle.manifest.content_sha256 != journal.recovery_content_sha256
            {
                return Err(
                    "identity import recovery bundle does not match its journal".to_string()
                );
            }
            Self::revalidate_identity_bundle_snapshot_path(&snapshot)?;
            let evidence = parse_identity_import_recovery_evidence(&journal.relocations_json)?;
            let derived_snapshot = self.read_identity_import_derived_recovery(&evidence.derived)?;
            let relocations = &evidence.roots;
            let mut graph = snapshot.bundle.graph.clone();
            if relocations.len() != graph.roots.len() {
                return Err("identity import recovery root evidence is incomplete".to_string());
            }
            for root in &mut graph.roots {
                root.portable_path = relocations
                    .get(&root.root_id)
                    .ok_or_else(|| {
                        format!(
                            "identity import recovery lacks root evidence for {}",
                            root.root_id
                        )
                    })?
                    .clone();
            }
            validate_relocated_identity_graph(&graph)?;
            let mut desired_rows = bundle_graph_rows(&graph)?;
            let persisted_provenance = evidence
                .persisted_suggestion_provenance_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            desired_rows.retain(|row| {
                row.table != SUGGESTION_SOURCE_PROVENANCE_TABLE
                    || persisted_provenance.contains(row.stable_id.as_str())
            });
            restore_raw_vector_operation_rows(
                &mut desired_rows,
                &derived_snapshot.recovery.raw_operation_rows,
            )?;
            if portable_rows_digest(&desired_rows)? != journal.pre_state_sha256 {
                return Err(
                    "identity import recovery bundle does not match the pre-state digest"
                        .to_string(),
                );
            }
            journal.phase = "rollback_started".to_string();
            journal.updated_at = now();
            self.store_identity_import_journal_unlocked(&journal)?;
            self.restore_exchange_rows_unlocked(&desired_rows)?;
            self.restore_exchange_derived_snapshot_unlocked(&derived_snapshot.recovery.snapshot)?;
            let observed_rows = stored_graph_rows(&self.collect_stored_identity_graph_unlocked()?)?;
            if observed_rows != desired_rows
                || portable_rows_digest(&observed_rows)? != journal.pre_state_sha256
            {
                return Err(
                    "startup identity import recovery did not restore exact portable state"
                        .to_string(),
                );
            }
            journal.phase = "rollback_verified".to_string();
            journal.updated_at = now();
            self.store_identity_import_journal_unlocked(&journal)?;
            drop(derived_snapshot);
            drop(snapshot);
            self.clear_identity_import_journal_unlocked(&journal)?;
            let execution = self.execution_state_unlocked()?;
            self.publish_exchange_revisions_to_cache(&execution);
            Ok(())
        })();
        if recovery.is_err() {
            self.invalidate_match_caches_for_pending_import_unlocked();
        }
        recovery
    }

    fn identity_import_recovery_path(
        &self,
        journal: &IdentityImportJournal,
    ) -> Result<PathBuf, String> {
        self.identity_import_app_owned_recovery_path(
            &journal.recovery_path,
            ".facial-identity.json",
            "identity import recovery",
        )
    }

    fn identity_import_derived_recovery_path(
        &self,
        binding: &IdentityImportDerivedBinding,
    ) -> Result<PathBuf, String> {
        self.identity_import_app_owned_recovery_path(
            &binding.canonical_path,
            IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX,
            "identity import derived recovery",
        )
    }

    fn identity_import_app_owned_recovery_path(
        &self,
        raw_path: &str,
        required_suffix: &str,
        label: &str,
    ) -> Result<PathBuf, String> {
        let path = PathBuf::from(raw_path);
        let state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no app-state parent")?;
        if !path.is_absolute()
            || path.parent().map(normalized_host_path) != Some(normalized_host_path(state_root))
        {
            return Err(format!("{label} path escaped app-owned state"));
        }
        let leaf = path
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or_else(|| format!("{label} path has no UTF-8 filename"))?;
        if !leaf.starts_with(IDENTITY_IMPORT_RECOVERY_PREFIX) || !leaf.ends_with(required_suffix) {
            return Err(format!("{label} path has an invalid app-owned leaf"));
        }
        validate_portable_file_leaf(label, path.file_name().unwrap())?;
        Ok(path)
    }

    fn publish_identity_import_derived_recovery(
        &self,
        path: &Path,
        snapshot: &ExchangeDerivedSnapshot,
        current: &StoredIdentityGraph,
    ) -> Result<IdentityImportDerivedBinding, String> {
        let snapshot_sha256 = exchange_derived_snapshot_digest(snapshot)?;
        let snapshot_bytes = serde_json::to_vec(snapshot)
            .map_err(|error| format!("size identity import derived snapshot: {error}"))?
            .len();
        let charged_bytes = charge_derived_recovery_bytes(
            0,
            snapshot_bytes
                .checked_add(1024)
                .ok_or("identity import derived recovery byte count overflow")?,
        )?;
        // Charge every raw vector-bearing operation under the same aggregate
        // ceiling before building the recovery Vec. This keeps refusal ahead
        // of recovery-file publication and journal/state mutation.
        preflight_raw_vector_operation_rows_bytes(&current.operations, charged_bytes)?;
        let recovery = IdentityImportDerivedRecovery {
            format: IDENTITY_IMPORT_DERIVED_RECOVERY_FORMAT.to_string(),
            version: IDENTITY_IMPORT_DERIVED_RECOVERY_VERSION,
            snapshot_sha256: snapshot_sha256.clone(),
            snapshot: snapshot.clone(),
            raw_operation_rows: raw_vector_operation_rows(current)?,
        };
        let bytes = bounded_json_bytes(
            &recovery,
            IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES,
            "identity import derived recovery",
        )?;
        let publication =
            write_new_regular_file(path, &bytes, IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES)?;
        require_durable_recovery_publication(publication)?;
        let guarded = read_bounded_regular_file_snapshot(
            path,
            IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES,
            "identity import derived recovery",
        )?;
        if guarded.bytes != bytes {
            return Err(
                "identity import derived recovery changed after durable publication".to_string(),
            );
        }
        let canonical_path = guarded
            .canonical_path
            .to_str()
            .ok_or("identity import derived recovery path is not UTF-8")?
            .to_string();
        Ok(IdentityImportDerivedBinding {
            canonical_path,
            file_sha256: sha256_bytes(&guarded.bytes),
            snapshot_sha256,
            canonical_bytes: guarded.bytes.len(),
        })
    }

    fn read_identity_import_derived_recovery(
        &self,
        binding: &IdentityImportDerivedBinding,
    ) -> Result<GuardedIdentityImportDerivedRecovery, String> {
        validate_sha256(
            "identity import derived recovery file hash",
            &binding.file_sha256,
        )?;
        validate_sha256(
            "identity import derived recovery snapshot hash",
            &binding.snapshot_sha256,
        )?;
        if binding.canonical_bytes == 0
            || binding.canonical_bytes > IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES
        {
            return Err(
                "identity import derived recovery byte binding is outside its limit".to_string(),
            );
        }
        let path = self.identity_import_derived_recovery_path(binding)?;
        let guarded = read_bounded_regular_file_snapshot(
            &path,
            IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES,
            "identity import derived recovery",
        )?;
        if guarded.canonical_path != path
            || guarded.bytes.len() != binding.canonical_bytes
            || sha256_bytes(&guarded.bytes) != binding.file_sha256
        {
            return Err(
                "identity import derived recovery file does not match its journal binding"
                    .to_string(),
            );
        }
        let recovery: IdentityImportDerivedRecovery = serde_json::from_slice(&guarded.bytes)
            .map_err(|error| format!("decode identity import derived recovery: {error}"))?;
        if recovery.format != IDENTITY_IMPORT_DERIVED_RECOVERY_FORMAT
            || recovery.version != IDENTITY_IMPORT_DERIVED_RECOVERY_VERSION
            || recovery.snapshot_sha256 != binding.snapshot_sha256
            || exchange_derived_snapshot_digest(&recovery.snapshot)? != binding.snapshot_sha256
        {
            return Err(
                "identity import derived recovery content does not match its journal binding"
                    .to_string(),
            );
        }
        validate_raw_vector_operation_rows(&recovery.raw_operation_rows)?;
        Ok(GuardedIdentityImportDerivedRecovery {
            recovery,
            _guard: guarded,
        })
    }

    fn cleanup_uncommitted_import_recovery_files(&self, journal: &IdentityImportJournal) {
        if let Ok(path) = self.identity_import_recovery_path(journal) {
            let _ = remove_durable_recovery_file(&path, "identity import recovery artifact");
        }
        if let Ok(evidence) = parse_identity_import_recovery_evidence(&journal.relocations_json) {
            if let Ok(path) = self.identity_import_derived_recovery_path(&evidence.derived) {
                let _ = remove_durable_recovery_file(
                    &path,
                    "identity import derived recovery artifact",
                );
            }
        }
    }

    pub(super) fn reconcile_identity_import_recovery_artifacts_unlocked(
        &self,
    ) -> Result<usize, String> {
        let state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no app-state parent")?;
        let state_root = absolute_lexical_path(state_root, "identity import recovery root")?;
        let mut live_paths = BTreeSet::new();
        if let Some(journal) = self.get_one_unlocked::<IdentityImportJournal>(
            IDENTITY_IMPORT_JOURNAL_TABLE,
            IDENTITY_IMPORT_JOURNAL_ID,
        )? {
            validate_identity_import_journal(&journal)?;
            live_paths.insert(self.identity_import_recovery_path(&journal)?);
            let evidence = parse_identity_import_recovery_evidence(&journal.relocations_json)?;
            live_paths.insert(self.identity_import_derived_recovery_path(&evidence.derived)?);
        }
        let mut removed = 0usize;
        let entries = fs::read_dir(&state_root).map_err(|error| {
            format!(
                "enumerate identity import recovery artifacts {}: {error}",
                state_root.display()
            )
        })?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("enumerate identity import recovery artifact: {error}"))?;
            let Some(leaf) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let app_owned = leaf.starts_with(IDENTITY_IMPORT_RECOVERY_PREFIX)
                && (leaf.ends_with(".facial-identity.json")
                    || leaf.ends_with(IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX));
            if !app_owned || live_paths.contains(&entry.path()) {
                continue;
            }
            if remove_durable_recovery_file(
                &entry.path(),
                "orphan identity import recovery artifact",
            )? {
                removed = removed
                    .checked_add(1)
                    .ok_or("identity import orphan removal count overflow")?;
            }
        }
        Ok(removed)
    }

    fn restore_exchange_rows_unlocked(&self, rows: &[OwnedPortableRow]) -> Result<(), String> {
        let current = stored_graph_rows(&self.collect_stored_identity_graph_unlocked()?)?;
        if row_map(&current) == row_map(rows) {
            return Ok(());
        }
        let target = row_map(rows);
        let deletes = current
            .into_iter()
            .filter(|row| !target.contains_key(&(row.table.clone(), row.stable_id.clone())))
            .collect::<Vec<_>>();
        let execution = self.next_exchange_execution_unlocked()?;
        let mut upserts = rows.to_vec();
        upserts.push(OwnedPortableRow {
            table: EXECUTION_TABLE.to_string(),
            stable_id: "global".to_string(),
            value: serde_json::to_value(&execution).map_err(|error| error.to_string())?,
        });
        self.commit_owned_exchange_rows_unlocked(&upserts, &deletes)?;
        self.publish_exchange_revisions_to_cache(&execution);
        Ok(())
    }

    fn capture_exchange_derived_snapshot_unlocked(
        &self,
    ) -> Result<ExchangeDerivedSnapshot, String> {
        self.preflight_exchange_derived_snapshot_unlocked()?;
        let mut trusted_search_rows =
            self.list_unlocked::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)?;
        trusted_search_rows.sort_by(|left, right| left.membership_id.cmp(&right.membership_id));
        let trusted_index_build =
            self.get_one_unlocked::<TrustedIndexBuildReceipt>(TRUSTED_INDEX_BUILD_TABLE, "global")?;
        let mut calibrations = self.list_unlocked::<CalibrationActivation>(CALIBRATION_TABLE)?;
        calibrations.sort_by(|left, right| {
            left.calibration_generation
                .cmp(&right.calibration_generation)
        });
        Ok(ExchangeDerivedSnapshot {
            trusted_search_rows,
            trusted_index_build,
            calibrations,
        })
    }

    fn preflight_exchange_derived_snapshot_unlocked(&self) -> Result<(), String> {
        let mut after_membership_id = String::new();
        // Object field names, brackets, commas, and the optional singleton are
        // charged once; each paged entity below also pays one delimiter byte.
        let mut charged_bytes = charge_derived_recovery_bytes(0, 1024)?;
        loop {
            let db = self.database();
            let page_after = after_membership_id.clone();
            let page: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(format!(
                        "SELECT * OMIT id FROM {TRUSTED_SEARCH_TABLE} WHERE membership_id > $after_id ORDER BY membership_id ASC LIMIT {IDENTITY_IMPORT_DERIVED_PREFLIGHT_PAGE};"
                    ))
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("preflight identity import derived recovery: {error}"))?;
                response.take(0).map_err(|error| {
                    format!("decode identity import derived recovery preflight: {error}")
                })
            })?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for row in page {
                let membership_id = row
                    .get("membership_id")
                    .and_then(Value::as_str)
                    .ok_or("trusted search recovery row lacks membership_id")?;
                if membership_id <= after_membership_id.as_str() {
                    return Err(
                        "identity import derived recovery preflight did not advance".to_string()
                    );
                }
                let row_bytes = serde_json::to_vec(&row)
                    .map_err(|error| format!("size identity import derived row: {error}"))?
                    .len();
                charged_bytes = charge_derived_recovery_bytes(
                    charged_bytes,
                    row_bytes
                        .checked_add(1)
                        .ok_or("identity import derived recovery byte count overflow")?,
                )?;
                after_membership_id.clear();
                after_membership_id.push_str(membership_id);
            }
            if page_len < IDENTITY_IMPORT_DERIVED_PREFLIGHT_PAGE {
                break;
            }
        }
        if let Some(build) = self.get_one_unlocked::<Value>(TRUSTED_INDEX_BUILD_TABLE, "global")? {
            let bytes = serde_json::to_vec(&build)
                .map_err(|error| format!("size trusted-index recovery receipt: {error}"))?
                .len();
            charged_bytes = charge_derived_recovery_bytes(
                charged_bytes,
                bytes
                    .checked_add(1)
                    .ok_or("identity import derived recovery byte count overflow")?,
            )?;
        }
        let mut after_calibration_generation = String::new();
        loop {
            let db = self.database();
            let page_after = after_calibration_generation.clone();
            let page: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(format!(
                        "SELECT * OMIT id FROM {CALIBRATION_TABLE} WHERE calibration_generation > $after_id ORDER BY calibration_generation ASC LIMIT {IDENTITY_IMPORT_DERIVED_PREFLIGHT_PAGE};"
                    ))
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("preflight calibration recovery rows: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode calibration recovery preflight: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for row in page {
                let calibration_generation = row
                    .get("calibration_generation")
                    .and_then(Value::as_str)
                    .ok_or("calibration recovery row lacks calibration_generation")?;
                if calibration_generation <= after_calibration_generation.as_str() {
                    return Err(
                        "identity import calibration recovery preflight did not advance"
                            .to_string(),
                    );
                }
                let row_bytes = serde_json::to_vec(&row)
                    .map_err(|error| format!("size calibration recovery row: {error}"))?
                    .len();
                charged_bytes = charge_derived_recovery_bytes(
                    charged_bytes,
                    row_bytes
                        .checked_add(1)
                        .ok_or("identity import derived recovery byte count overflow")?,
                )?;
                after_calibration_generation.clear();
                after_calibration_generation.push_str(calibration_generation);
            }
            if page_len < IDENTITY_IMPORT_DERIVED_PREFLIGHT_PAGE {
                break;
            }
        }
        Ok(())
    }

    fn restore_exchange_derived_snapshot_unlocked(
        &self,
        snapshot: &ExchangeDerivedSnapshot,
    ) -> Result<usize, String> {
        self.replace_trusted_search_index_unlocked(&snapshot.trusted_search_rows)?;

        let build_value = snapshot
            .trusted_index_build
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| error.to_string())?;
        match build_value {
            Some(value) => self.transactional_upserts_deletes_unlocked(
                &[(TRUSTED_INDEX_BUILD_TABLE, "global", value)],
                &[],
            )?,
            None => self.transactional_upserts_deletes_unlocked(
                &[],
                &[(TRUSTED_INDEX_BUILD_TABLE, "global")],
            )?,
        }

        let current_calibrations =
            self.list_unlocked::<CalibrationActivation>(CALIBRATION_TABLE)?;
        let target_ids = snapshot
            .calibrations
            .iter()
            .map(|row| row.calibration_generation.as_str())
            .collect::<BTreeSet<_>>();
        let calibration_deletes = current_calibrations
            .iter()
            .filter(|row| !target_ids.contains(row.calibration_generation.as_str()))
            .map(|row| (CALIBRATION_TABLE, row.calibration_generation.as_str()))
            .collect::<Vec<_>>();
        let calibration_values = snapshot
            .calibrations
            .iter()
            .map(|row| {
                Ok((
                    CALIBRATION_TABLE,
                    row.calibration_generation.as_str(),
                    serde_json::to_value(row).map_err(|error| error.to_string())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.transactional_upserts_deletes_unlocked(&calibration_values, &calibration_deletes)?;

        let observed = self.capture_exchange_derived_snapshot_unlocked()?;
        if observed.trusted_search_rows != snapshot.trusted_search_rows
            || observed.trusted_index_build != snapshot.trusted_index_build
            || observed.calibrations != snapshot.calibrations
        {
            return Err("identity rollback did not restore exact derived Match state".to_string());
        }
        Ok(snapshot.trusted_search_rows.len())
    }

    fn reactivate_rollback_calibrations_unlocked(&self) -> Result<(), String> {
        let mut candidates = self
            .list_unlocked::<CalibrationActivation>(CALIBRATION_TABLE)?
            .into_iter()
            .filter(|row| {
                !row.active
                    && row.invalidation_reason.as_deref()
                        == Some("trusted_search_reconciled:was_active")
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(());
        }
        if candidates.len() != 1 {
            return Err(
                "multiple formerly-active strict calibrations cannot be recovered".to_string(),
            );
        }
        let mut activation = candidates.pop().expect("one calibration candidate");
        if activation.activation_integrity_digest
            != calibration_activation_integrity_digest(&activation)
        {
            return Err("stored calibration activation integrity is invalid".to_string());
        }
        if activation.verifier_verdict != "pass"
            || activation.independent_review_verdict != "pass"
            || !activation.wp084_runtime_ready
            || !activation.wp087_release_ready
        {
            return Err(
                "stored calibration verification/readiness evidence is incomplete".to_string(),
            );
        }
        validate_sha256("calibration envelope hash", &activation.envelope_hash)?;
        validate_sha256(
            "calibration trusted-index digest",
            &activation.trusted_index_build_digest,
        )?;
        validate_sha256(
            "calibration gallery-members digest",
            &activation.gallery_members_digest,
        )?;
        let active_generation = self
            .active_model_generation_unlocked()?
            .ok_or("no active Match model generation remains after rollback")?;
        if active_generation != activation.model_generation {
            return Err(
                "calibration model generation is not the active validated generation".to_string(),
            );
        }
        let generation: ModelGeneration = self.require_unlocked(
            GENERATION_TABLE,
            &activation.model_generation,
            "rollback calibration model generation",
        )?;
        if !generation.validated || generation.state != "active" {
            return Err(
                "rollback calibration model generation is not active and validated".to_string(),
            );
        }
        let build: TrustedIndexBuildReceipt = self.require_unlocked(
            TRUSTED_INDEX_BUILD_TABLE,
            "global",
            "rollback trusted-index build receipt",
        )?;
        if build.build_digest != activation.trusted_index_build_digest
            || build.source_row_count != self.count_unlocked_exchange(TRUSTED_SEARCH_TABLE)?
            || build.engine_version != STRICT_ANN_ENGINE_VERSION
            || build.build_seed != 0
            || build.build_order != STRICT_ANN_BUILD_ORDER
        {
            return Err(
                "rollback trusted-index build receipt does not match calibration".to_string(),
            );
        }
        if activation.runtime_configuration_digest
            != strict_runtime_configuration_digest(activation.candidate_k, activation.rerank_k)
        {
            return Err(
                "rollback runtime configuration digest does not match calibration".to_string(),
            );
        }
        if self.trusted_gallery_members_digest_unlocked()? != activation.gallery_members_digest {
            return Err(
                "rollback gallery composition digest does not match calibration".to_string(),
            );
        }
        self.validate_gallery_envelope_unlocked(
            &activation.model_generation,
            activation.people_max,
            activation.looks_per_person_max,
            activation.templates_per_look_max,
            activation.total_templates_max,
        )?;

        activation.active = true;
        activation.invalidation_reason = None;
        activation.updated_at = now();
        activation.activation_integrity_digest =
            calibration_activation_integrity_digest(&activation);
        self.transactional_upserts_deletes_unlocked(
            &[((
                CALIBRATION_TABLE,
                activation.calibration_generation.as_str(),
                serde_json::to_value(&activation).map_err(|error| error.to_string())?,
            ))],
            &[],
        )
    }

    fn recover_failed_import_unlocked(
        &self,
        rows: &[OwnedPortableRow],
        derived: &ExchangeDerivedSnapshot,
    ) -> Result<(), String> {
        self.invalidate_match_caches_for_pending_import_unlocked();
        let mut failures = Vec::new();
        if let Err(error) = self.restore_exchange_rows_unlocked(rows) {
            failures.push(format!("portable rows: {error}"));
        }
        if let Err(error) = self.restore_exchange_derived_snapshot_unlocked(derived) {
            failures.push(format!("derived Match state: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            self.invalidate_match_caches_for_pending_import_unlocked();
            Err(failures.join("; "))
        }
    }

    fn next_exchange_execution_unlocked(&self) -> Result<PersistedExecutionState, String> {
        let mut execution = self.execution_state_unlocked()?;
        execution.revision = execution
            .revision
            .checked_add(1)
            .ok_or("Match execution revision overflow during identity exchange")?;
        execution.identity_revision = execution
            .identity_revision
            .checked_add(1)
            .ok_or("Match identity revision overflow during identity exchange")?;
        execution.catalog_revision = execution
            .catalog_revision
            .checked_add(1)
            .ok_or("Match catalog revision overflow during identity exchange")?;
        execution.updated_at = now();
        Ok(execution)
    }

    fn publish_exchange_revisions_to_cache(&self, execution: &PersistedExecutionState) {
        let mut caches = self.cache_write_recover();
        caches.identity_revision = execution.identity_revision;
        caches.catalog_revision = execution.catalog_revision;
        caches.projections.clear();
        caches.autocomplete.valid = false;
    }

    fn invalidate_match_caches_for_pending_import_unlocked(&self) {
        let mut caches = self.cache_write_recover();
        *caches = MatchCaches::default();
    }
}

fn validate_identity_import_journal(journal: &IdentityImportJournal) -> Result<(), String> {
    if journal.journal_id != IDENTITY_IMPORT_JOURNAL_ID
        || journal.version != IDENTITY_IMPORT_JOURNAL_VERSION
    {
        return Err("unsupported identity import recovery journal".to_string());
    }
    validate_sha256("identity import journal plan token", &journal.plan_token)?;
    validate_sha256(
        "identity import journal pre-state digest",
        &journal.pre_state_sha256,
    )?;
    validate_sha256(
        "identity import journal recovery file hash",
        &journal.recovery_file_sha256,
    )?;
    validate_sha256(
        "identity import journal recovery content hash",
        &journal.recovery_content_sha256,
    )?;
    validate_text(
        "identity import journal recovery path",
        &journal.recovery_path,
    )?;
    validate_text("identity import journal creation time", &journal.created_at)?;
    validate_text("identity import journal update time", &journal.updated_at)?;
    if !matches!(
        journal.phase.as_str(),
        "portable_committed"
            | "derived_reconciled"
            | "completion_verified"
            | "rollback_started"
            | "rollback_verified"
    ) {
        return Err("identity import recovery journal has an invalid phase".to_string());
    }
    parse_identity_import_recovery_evidence(&journal.relocations_json)?;
    Ok(())
}

fn parse_identity_import_recovery_evidence(
    relocations_json: &str,
) -> Result<IdentityImportRecoveryEvidence, String> {
    if relocations_json.len() > IDENTITY_BUNDLE_MAX_STRING_BYTES {
        return Err("identity import recovery root evidence exceeds its bounded size".to_string());
    }
    let evidence: IdentityImportRecoveryEvidence = serde_json::from_str(relocations_json)
        .map_err(|error| format!("decode identity import recovery root evidence: {error}"))?;
    if evidence.roots.len() > IDENTITY_BUNDLE_MAX_ENTITIES {
        return Err("identity import recovery root evidence has too many entries".to_string());
    }
    for (root_id, path) in &evidence.roots {
        validate_text("identity import recovery root ID", root_id)?;
        validate_text("identity import recovery root path", path)?;
    }
    if evidence.persisted_suggestion_provenance_ids.len() > IDENTITY_BUNDLE_MAX_ENTITIES {
        return Err(
            "identity import recovery provenance evidence has too many entries".to_string(),
        );
    }
    unique_ids(
        "identity import recovery persisted SuggestionSourceProvenance",
        evidence
            .persisted_suggestion_provenance_ids
            .iter()
            .map(String::as_str),
    )?;
    validate_text(
        "identity import derived recovery path",
        &evidence.derived.canonical_path,
    )?;
    validate_sha256(
        "identity import derived recovery file hash",
        &evidence.derived.file_sha256,
    )?;
    validate_sha256(
        "identity import derived recovery snapshot hash",
        &evidence.derived.snapshot_sha256,
    )?;
    if evidence.derived.canonical_bytes == 0
        || evidence.derived.canonical_bytes > IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES
    {
        return Err("identity import derived recovery byte binding is invalid".to_string());
    }
    Ok(evidence)
}

fn cleanup_recovery_path(path: &Path) {
    let _ = remove_durable_recovery_file(path, "uncommitted Match recovery artifact");
}

fn export_preview(bundle: &IdentityBundleV1) -> Result<IdentityBundleExportPreview, String> {
    let bytes = canonical_bundle_bytes(bundle)?;
    let graph = &bundle.graph;
    Ok(IdentityBundleExportPreview {
        format: bundle.manifest.format.clone(),
        version: bundle.manifest.version,
        content_sha256: bundle.manifest.content_sha256.clone(),
        canonical_bytes: bytes.len(),
        entity_count: graph_entity_count(graph),
        table_counts: graph_table_counts(graph),
        excluded_payloads: vec![
            "face_embeddings".to_string(),
            "cached_crops".to_string(),
            "trusted_search_vectors".to_string(),
            "people_projections".to_string(),
            "jobs_and_runtime_execution".to_string(),
            "calibration_evidence_artifacts".to_string(),
        ],
        limits: IdentityBundleLimits::default(),
    })
}

fn bundle_content_sha256(graph: &IdentityBundleGraph) -> Result<String, String> {
    let bytes = serde_json::to_vec(&BundleHashMaterial {
        format: IDENTITY_BUNDLE_FORMAT,
        version: IDENTITY_BUNDLE_VERSION,
        schema_version: MATCH_SCHEMA_VERSION,
        schema_generation: MATCH_SCHEMA_GENERATION,
        graph,
    })
    .map_err(|error| format!("serialize identity bundle content: {error}"))?;
    Ok(sha256_bytes(&bytes))
}

fn canonical_bundle_bytes(bundle: &IdentityBundleV1) -> Result<Vec<u8>, String> {
    serde_json::to_vec(bundle)
        .map_err(|error| format!("serialize canonical identity bundle: {error}"))
}

fn preflight_canonical_identity_bundle_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > IDENTITY_BUNDLE_MAX_BYTES {
        return Err(format!(
            "canonical identity bundle is {} bytes; limit is {IDENTITY_BUNDLE_MAX_BYTES}",
            bytes.len()
        ));
    }
    preflight_identity_bundle_json(bytes)
}

fn exchange_derived_snapshot_digest(snapshot: &ExchangeDerivedSnapshot) -> Result<String, String> {
    bounded_json_sha256(
        snapshot,
        IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES,
        "derived Match rollback fence",
    )
}

fn legacy_suggestion_history(
    operations: &[MatchOperation],
    indexed_operations: &BTreeMap<String, IndexedCorrectionEnvelope>,
) -> Result<LegacySuggestionHistory, String> {
    let mut history = LegacySuggestionHistory::default();
    for operation in operations {
        let Ok(created_at) = chrono::DateTime::parse_from_rfc3339(&operation.created_at) else {
            continue;
        };
        let Some(indexed) = indexed_operations.get(&operation.operation_id) else {
            continue;
        };
        for person in indexed.envelope.rows.iter().filter_map(|row| {
            (row.table == CorrectionTable::Person)
                .then_some(row.before.as_ref())
                .flatten()
                .and_then(|value| serde_json::from_value::<Person>(value.clone()).ok())
        }) {
            history
                .people
                .entry(person.person_id.clone())
                .or_default()
                .push((created_at, person));
        }
        if !matches!(
            operation.kind.as_str(),
            "correction_delete_face_analysis" | "correction_batch_delete_face_analysis"
        ) {
            continue;
        }
        let faces = indexed
            .envelope
            .rows
            .iter()
            .filter_map(|row| {
                (row.table == CorrectionTable::Face)
                    .then_some(row.before.as_ref())
                    .flatten()
                    .and_then(|value| serde_json::from_value::<FaceObservation>(value.clone()).ok())
            })
            .map(|face| (face.face_id.clone(), face))
            .collect::<BTreeMap<_, _>>();
        for embedding in indexed.envelope.rows.iter().filter_map(|row| {
            (row.table == CorrectionTable::Embedding)
                .then_some(row.before.as_ref())
                .flatten()
                .and_then(|value| serde_json::from_value::<FaceEmbedding>(value.clone()).ok())
        }) {
            let Some(face) = faces.get(&embedding.face_id) else {
                continue;
            };
            history
                .face_embeddings
                .entry((face.face_id.clone(), embedding.embedding_id.clone()))
                .or_default()
                .push((created_at, face.clone(), embedding));
        }
    }
    Ok(history)
}

fn charge_legacy_provenance_work(work: &mut usize, additional: usize) -> Result<(), String> {
    *work = work
        .checked_add(additional)
        .ok_or("legacy suggestion provenance work overflow")?;
    enforce_limit(
        "legacy suggestion provenance work",
        *work,
        IDENTITY_BUNDLE_MAX_REFERENCES,
    )
}

fn build_legacy_suggestion_lookup(
    stored: &StoredIdentityGraph,
    history: &LegacySuggestionHistory,
    work: &mut usize,
) -> Result<LegacySuggestionLookup, String> {
    let mut current_faces = BTreeMap::new();
    for face in &stored.faces {
        charge_legacy_provenance_work(work, 1)?;
        if current_faces
            .insert(face.face_id.clone(), face.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate current Face {} during legacy provenance indexing",
                face.face_id
            ));
        }
    }
    let mut current_people = BTreeMap::new();
    for person in &stored.people {
        charge_legacy_provenance_work(work, 1)?;
        if current_people
            .insert(person.person_id.clone(), person.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate current Person {} during legacy provenance indexing",
                person.person_id
            ));
        }
    }
    let mut historical_face_embeddings = BTreeMap::new();
    for anchors in history.face_embeddings.values() {
        for anchor in anchors {
            charge_legacy_provenance_work(work, 1)?;
            let (_, face, embedding) = anchor;
            historical_face_embeddings
                .entry(LegacyFaceEmbeddingAnchorKey {
                    face_id: face.face_id.clone(),
                    embedding_id: embedding.embedding_id.clone(),
                    face_revision: face.face_revision,
                    media_fingerprint: face.media_fingerprint.clone(),
                    model_generation: embedding.model_generation.clone(),
                    job_id: embedding.job_id.clone(),
                })
                .or_insert_with(Vec::new)
                .push(anchor.clone());
        }
    }
    charge_legacy_provenance_work(
        work,
        history.people.values().try_fold(0usize, |total, rows| {
            total
                .checked_add(rows.len())
                .ok_or("legacy suggestion provenance work overflow")
        })?,
    )?;
    Ok(LegacySuggestionLookup {
        current_faces,
        current_people,
        historical_face_embeddings,
        historical_people: history.people.clone(),
    })
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    label: &'static str,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "JSON size overflow")
        })?;
        if next > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} exceeds {} bytes", self.label, self.limit),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json_bytes<T: Serialize>(
    value: &T,
    limit: usize,
    label: &'static str,
) -> Result<Vec<u8>, String> {
    let mut writer = BoundedJsonWriter {
        bytes: Vec::new(),
        limit,
        label,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| format!("serialize {label}: {error}"))?;
    Ok(writer.bytes)
}

struct BoundedJsonHashWriter {
    hasher: Sha256,
    written: usize,
    limit: usize,
    label: &'static str,
}

impl Write for BoundedJsonHashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.written.checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "JSON size overflow")
        })?;
        if next > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} exceeds {} bytes", self.label, self.limit),
            ));
        }
        self.hasher.update(bytes);
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json_sha256<T: Serialize>(
    value: &T,
    limit: usize,
    label: &'static str,
) -> Result<String, String> {
    let mut writer = BoundedJsonHashWriter {
        hasher: Sha256::new(),
        written: 0,
        limit,
        label,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| format!("serialize {label}: {error}"))?;
    Ok(format!("{:x}", writer.hasher.finalize()))
}

fn enforce_canonical_bundle_size(bundle: &IdentityBundleV1) -> Result<(), String> {
    let bytes = canonical_bundle_bytes(bundle)?;
    if bytes.len() > IDENTITY_BUNDLE_MAX_BYTES {
        return Err(format!(
            "canonical identity bundle is {} bytes; limit is {IDENTITY_BUNDLE_MAX_BYTES}",
            bytes.len()
        ));
    }
    Ok(())
}

struct JsonPreflightBudget {
    nodes: usize,
    string_bytes: usize,
    max_nodes: usize,
    max_collection: usize,
}

struct JsonPreflightSeed<'a> {
    budget: &'a mut JsonPreflightBudget,
    depth: usize,
}

struct JsonPreflightVisitor<'a> {
    budget: &'a mut JsonPreflightBudget,
    depth: usize,
}

impl JsonPreflightBudget {
    fn add_string<E: serde::de::Error>(&mut self, value: &str) -> Result<(), E> {
        if value.len() > IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES {
            return Err(E::custom(
                "identity bundle string exceeds the single-string limit",
            ));
        }
        self.string_bytes = self
            .string_bytes
            .checked_add(value.len())
            .ok_or_else(|| E::custom("identity bundle aggregate string bytes overflow"))?;
        if self.string_bytes > IDENTITY_BUNDLE_MAX_STRING_BYTES {
            return Err(E::custom(
                "identity bundle aggregate string bytes exceed the limit",
            ));
        }
        Ok(())
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for JsonPreflightSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > IDENTITY_BUNDLE_MAX_NESTING_DEPTH {
            return Err(serde::de::Error::custom(
                "identity bundle nesting exceeds the limit",
            ));
        }
        self.budget.nodes = self
            .budget
            .nodes
            .checked_add(1)
            .ok_or_else(|| serde::de::Error::custom("identity bundle node count overflow"))?;
        if self.budget.nodes > self.budget.max_nodes {
            return Err(serde::de::Error::custom(
                "identity bundle token count exceeds the JSON token-work limit",
            ));
        }
        deserializer.deserialize_any(JsonPreflightVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

impl<'de> serde::de::Visitor<'de> for JsonPreflightVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded identity bundle JSON")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_some<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::de::DeserializeSeed::deserialize(
            JsonPreflightSeed {
                budget: self.budget,
                depth: self.depth + 1,
            },
            deserializer,
        )
    }
    fn visit_str<E>(self, value: &str) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        self.budget.add_string(value)
    }
    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        self.budget.add_string(value)
    }
    fn visit_string<E>(self, value: String) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        self.budget.add_string(&value)
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut count = 0usize;
        while sequence
            .next_element_seed(JsonPreflightSeed {
                budget: self.budget,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            count += 1;
            if count > self.budget.max_collection {
                return Err(serde::de::Error::custom(
                    "identity bundle collection exceeds the entity limit",
                ));
            }
        }
        Ok(())
    }
    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut count = 0usize;
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.depth + 1 > IDENTITY_BUNDLE_MAX_NESTING_DEPTH {
                return Err(serde::de::Error::custom(
                    "identity bundle nesting exceeds the limit",
                ));
            }
            self.budget.nodes =
                self.budget.nodes.checked_add(1).ok_or_else(|| {
                    serde::de::Error::custom("identity bundle node count overflow")
                })?;
            if self.budget.nodes > self.budget.max_nodes {
                return Err(serde::de::Error::custom(
                    "identity bundle token count exceeds the JSON token-work limit",
                ));
            }
            self.budget.add_string(&key)?;
            if !keys.insert(key) {
                return Err(serde::de::Error::custom(
                    "identity bundle JSON contains a duplicate object key",
                ));
            }
            count += 1;
            if count > self.budget.max_collection {
                return Err(serde::de::Error::custom(
                    "identity bundle object exceeds the entity limit",
                ));
            }
            map.next_value_seed(JsonPreflightSeed {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}

pub(super) fn preflight_identity_bundle_json(bytes: &[u8]) -> Result<(), String> {
    preflight_identity_bundle_json_with_limits(
        bytes,
        IDENTITY_BUNDLE_MAX_JSON_TOKENS,
        IDENTITY_BUNDLE_MAX_ENTITIES,
    )
}

fn preflight_identity_bundle_json_with_limits(
    bytes: &[u8],
    max_nodes: usize,
    max_collection: usize,
) -> Result<(), String> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut budget = JsonPreflightBudget {
        nodes: 0,
        string_bytes: 0,
        max_nodes,
        max_collection,
    };
    serde::de::DeserializeSeed::deserialize(
        JsonPreflightSeed {
            budget: &mut budget,
            depth: 1,
        },
        &mut deserializer,
    )
    .map_err(|error| format!("preflight identity bundle JSON: {error}"))?;
    deserializer
        .end()
        .map_err(|error| format!("preflight identity bundle JSON: {error}"))
}

fn parse_identity_bundle_bytes(bytes: &[u8]) -> Result<IdentityBundleV1, String> {
    enforce_limit(
        "identity bundle bytes",
        bytes.len(),
        IDENTITY_BUNDLE_MAX_BYTES.min(IDENTITY_BUNDLE_MAX_DECODED_BYTES),
    )?;
    reject_archive_or_compression_magic(bytes)?;
    preflight_identity_bundle_json(bytes)?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("decode identity bundle JSON: {error}"))?;
    let mut string_bytes = 0usize;
    let mut max_depth = 0usize;
    inspect_json_limits(&value, 1, &mut max_depth, &mut string_bytes)?;
    let bundle: IdentityBundleV1 = serde_json::from_value(value)
        .map_err(|error| format!("decode typed identity bundle: {error}"))?;
    let canonical = canonical_bundle_bytes(&bundle)?;
    if canonical != bytes {
        return Err(
            "identity bundle is not canonical JSON or contains duplicate/unknown fields"
                .to_string(),
        );
    }
    validate_bundle(&bundle)?;
    Ok(bundle)
}

fn validate_bundle(bundle: &IdentityBundleV1) -> Result<(), String> {
    if bundle.manifest.format != IDENTITY_BUNDLE_FORMAT {
        return Err("unsupported identity bundle format".to_string());
    }
    if bundle.manifest.version != IDENTITY_BUNDLE_VERSION {
        return Err(format!(
            "unsupported identity bundle version {}; supported version is {}",
            bundle.manifest.version, IDENTITY_BUNDLE_VERSION
        ));
    }
    if bundle.manifest.schema_version != MATCH_SCHEMA_VERSION
        || bundle.manifest.schema_generation != MATCH_SCHEMA_GENERATION
    {
        return Err("identity bundle schema does not match this Match runtime".to_string());
    }
    validate_sha256(
        "identity bundle content hash",
        &bundle.manifest.content_sha256,
    )?;
    let expected = bundle_content_sha256(&bundle.graph)?;
    if expected != bundle.manifest.content_sha256 {
        return Err("identity bundle content hash mismatch".to_string());
    }
    validate_identity_graph(&bundle.graph)?;
    enforce_canonical_bundle_size(bundle)
}

fn reject_vector_field(value: &Value, context: &str) -> Result<(), String> {
    match value {
        Value::Array(values) => {
            for value in values {
                reject_vector_field(value, context)?;
            }
        }
        Value::Object(values) => {
            if values.contains_key("vector") {
                return Err(format!(
                    "{context} contains a prohibited embedding vector payload"
                ));
            }
            for value in values.values() {
                reject_vector_field(value, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_portable_graph_has_no_vectors(graph: &IdentityBundleGraph) -> Result<(), String> {
    let graph_value = serde_json::to_value(graph)
        .map_err(|error| format!("inspect portable identity graph: {error}"))?;
    reject_vector_field(&graph_value, "portable identity graph")?;
    for operation in &graph.operations {
        for (field, text) in [
            ("before_json", operation.before_json.as_str()),
            ("after_json", operation.after_json.as_str()),
        ] {
            let value: Value = serde_json::from_str(text).map_err(|error| {
                format!(
                    "operation {} {field} is invalid JSON while checking vector exclusion: {error}",
                    operation.operation_id
                )
            })?;
            reject_vector_field(
                &value,
                &format!("operation {} {field}", operation.operation_id),
            )?;
        }
    }
    Ok(())
}

fn inspect_json_limits(
    value: &Value,
    depth: usize,
    max_depth: &mut usize,
    string_bytes: &mut usize,
) -> Result<(), String> {
    if depth > IDENTITY_BUNDLE_MAX_NESTING_DEPTH {
        return Err(format!(
            "identity bundle nesting exceeds {IDENTITY_BUNDLE_MAX_NESTING_DEPTH}"
        ));
    }
    *max_depth = (*max_depth).max(depth);
    match value {
        Value::String(text) => add_string_bytes(text, string_bytes)?,
        Value::Array(values) => {
            for value in values {
                inspect_json_limits(value, depth + 1, max_depth, string_bytes)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                add_string_bytes(key, string_bytes)?;
                inspect_json_limits(value, depth + 1, max_depth, string_bytes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn add_string_bytes(text: &str, total: &mut usize) -> Result<(), String> {
    if text.len() > IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES {
        return Err(format!(
            "identity bundle string exceeds {IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES} bytes"
        ));
    }
    *total = total
        .checked_add(text.len())
        .ok_or_else(|| "identity bundle string-byte count overflow".to_string())?;
    if *total > IDENTITY_BUNDLE_MAX_STRING_BYTES {
        return Err(format!(
            "identity bundle strings exceed {IDENTITY_BUNDLE_MAX_STRING_BYTES} bytes"
        ));
    }
    Ok(())
}

fn reject_archive_or_compression_magic(bytes: &[u8]) -> Result<(), String> {
    const MAGICS: [&[u8]; 7] = [
        b"PK\x03\x04",
        b"\x1f\x8b",
        b"7z\xbc\xaf\x27\x1c",
        b"Rar!\x1a\x07",
        b"BZh",
        b"\xfd7zXZ\x00",
        b"\x28\xb5\x2f\xfd",
    ];
    if MAGICS.iter().any(|magic| bytes.starts_with(magic)) {
        return Err("compressed or archive identity bundles are unsupported".to_string());
    }
    if bytes.first().copied() != Some(b'{') {
        return Err("identity bundle must be canonical plain JSON".to_string());
    }
    Ok(())
}

fn graph_entity_count(graph: &IdentityBundleGraph) -> usize {
    graph.people.len()
        + graph.video_observations.len()
        + graph.review_context.len()
        + graph.media_context.len()
        + graph.looks.len()
        + graph.trusted_template_sets.len()
        + graph.trusted_memberships.len()
        + graph.faces.len()
        + graph.assignments.len()
        + graph.suggestions.len()
        + graph.constraints.len()
        + graph.dispositions.len()
        + graph.operations.len()
        + graph.correction_media_operations.len()
        + graph.suggestion_source_provenance.len()
        + graph.roots.len()
        + graph.model_generations.len()
}

fn enforce_limit(label: &str, actual: usize, limit: usize) -> Result<(), String> {
    if actual > limit {
        Err(format!("{label} is {actual}; limit is {limit}"))
    } else {
        Ok(())
    }
}

fn graph_table_counts(graph: &IdentityBundleGraph) -> BTreeMap<String, usize> {
    BTreeMap::from([
        (
            super::context::MEDIA_CONTEXT_TABLE.to_string(),
            graph.media_context.len(),
        ),
        (
            super::video::VIDEO_OBSERVATION_TABLE.to_string(),
            graph.video_observations.len(),
        ),
        (
            super::context::CONTEXT_TABLE.to_string(),
            graph.review_context.len(),
        ),
        (PERSON_TABLE.to_string(), graph.people.len()),
        (LOOK_TABLE.to_string(), graph.looks.len()),
        (
            TEMPLATE_SET_TABLE.to_string(),
            graph.trusted_template_sets.len(),
        ),
        (
            TRUSTED_MEMBER_TABLE.to_string(),
            graph.trusted_memberships.len(),
        ),
        (FACE_TABLE.to_string(), graph.faces.len()),
        (ASSIGNMENT_TABLE.to_string(), graph.assignments.len()),
        (SUGGESTION_TABLE.to_string(), graph.suggestions.len()),
        (CONSTRAINT_TABLE.to_string(), graph.constraints.len()),
        (FACE_DISPOSITION_TABLE.to_string(), graph.dispositions.len()),
        (OPERATION_TABLE.to_string(), graph.operations.len()),
        (
            CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
            graph.correction_media_operations.len(),
        ),
        (
            SUGGESTION_SOURCE_PROVENANCE_TABLE.to_string(),
            graph.suggestion_source_provenance.len(),
        ),
        (ROOT_CONFIG_TABLE.to_string(), graph.roots.len()),
        (GENERATION_TABLE.to_string(), graph.model_generations.len()),
    ])
}

fn validate_identity_graph(graph: &IdentityBundleGraph) -> Result<usize, String> {
    validate_portable_graph_has_no_vectors(graph)?;
    let entities = graph_entity_count(graph);
    enforce_limit(
        "identity bundle entity count",
        entities,
        IDENTITY_BUNDLE_MAX_ENTITIES,
    )?;
    let people = unique_ids(
        "Person",
        graph.people.iter().map(|row| row.person_id.as_str()),
    )?;
    let looks = unique_ids("Look", graph.looks.iter().map(|row| row.look_id.as_str()))?;
    let sets = unique_ids(
        "TrustedTemplateSet",
        graph
            .trusted_template_sets
            .iter()
            .map(|row| row.set_id.as_str()),
    )?;
    unique_ids(
        "TrustedTemplateMembership",
        graph
            .trusted_memberships
            .iter()
            .map(|row| row.membership_id.as_str()),
    )?;
    let faces = unique_ids(
        "FaceObservation",
        graph.faces.iter().map(|row| row.face_id.as_str()),
    )?;
    unique_ids(
        "Assignment",
        graph
            .assignments
            .iter()
            .map(|row| row.assignment_id.as_str()),
    )?;
    unique_ids(
        "Suggestion",
        graph
            .suggestions
            .iter()
            .map(|row| row.suggestion_id.as_str()),
    )?;
    unique_ids(
        "CannotLinkConstraint",
        graph
            .constraints
            .iter()
            .map(|row| row.constraint_id.as_str()),
    )?;
    unique_ids(
        "FaceDisposition",
        graph.dispositions.iter().map(|row| row.face_id.as_str()),
    )?;
    let operations = unique_ids(
        "MatchOperation",
        graph.operations.iter().map(|row| row.operation_id.as_str()),
    )?;
    let operation_by_id = graph
        .operations
        .iter()
        .map(|row| (row.operation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut correction_by_operation = BTreeMap::new();
    let mut correction_effect_work = 0usize;
    let mut suggestion_provenance_anchor_work = 0usize;
    for operation in &graph.operations {
        if let Some(indexed) = index_correction_envelope(operation)? {
            correction_effect_work = correction_effect_work
                .checked_add(indexed.envelope.rows.len())
                .ok_or_else(|| "identity correction semantic work overflow".to_string())?;
            enforce_limit(
                "identity correction semantic work",
                correction_effect_work,
                IDENTITY_BUNDLE_MAX_REFERENCES,
            )?;
            correction_by_operation.insert(operation.operation_id.as_str(), indexed);
        }
    }
    let mut undo_restored_strict_assignments = BTreeMap::<String, Vec<Assignment>>::new();
    let mut undo_restored_suggestions = BTreeMap::<String, Vec<Suggestion>>::new();
    for operation in &graph.operations {
        if operation.kind != "undo_correction" {
            continue;
        }
        let indexed = correction_by_operation
            .get(operation.operation_id.as_str())
            .expect("undo operation was indexed above");
        for row in &indexed.envelope.rows {
            let Some(after) = row.after.as_ref() else {
                continue;
            };
            match row.table {
                CorrectionTable::Assignment => {
                    let assignment: Assignment = decode_strict_typed_value(
                        after,
                        "undo-restored strict Assignment evidence",
                    )?;
                    if assignment.state == AssignmentState::CommittedStrictAutomatic.as_str() {
                        undo_restored_strict_assignments
                            .entry(assignment.assignment_id.clone())
                            .or_default()
                            .push(assignment);
                    }
                }
                CorrectionTable::Suggestion => {
                    let suggestion: Suggestion =
                        decode_strict_typed_value(after, "undo-restored Suggestion evidence")?;
                    undo_restored_suggestions
                        .entry(suggestion.suggestion_id.clone())
                        .or_default()
                        .push(suggestion);
                }
                _ => {}
            }
        }
    }
    unique_ids(
        "CorrectionMediaOperation",
        graph
            .correction_media_operations
            .iter()
            .map(|row| row.mapping_id.as_str()),
    )?;
    unique_ids(
        "SuggestionSourceProvenance",
        graph
            .suggestion_source_provenance
            .iter()
            .map(|row| row.provenance_id.as_str()),
    )?;
    unique_ids(
        "MatchIndexRoot",
        graph.roots.iter().map(|row| row.root_id.as_str()),
    )?;
    let generations = unique_ids(
        "ModelGeneration",
        graph
            .model_generations
            .iter()
            .map(|row| row.generation.as_str()),
    )?;

    let look_by_id = graph
        .looks
        .iter()
        .map(|row| (row.look_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let set_by_id = graph
        .trusted_template_sets
        .iter()
        .map(|row| (row.set_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let assignment_by_face = graph
        .assignments
        .iter()
        .map(|row| (row.face_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let person_by_id = graph
        .people
        .iter()
        .map(|row| (row.person_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let face_by_id = graph
        .faces
        .iter()
        .map(|row| (row.face_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let portable_face_media_evidence = portable_face_media_evidence(graph)?;
    let generation_by_id = graph
        .model_generations
        .iter()
        .map(|row| (row.generation.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut references = 0usize;
    unique_ids(
        "VideoObservation",
        graph
            .video_observations
            .iter()
            .map(|r| r.observation_id.as_str()),
    )?;
    unique_ids(
        "ReviewContext",
        graph.review_context.iter().map(|r| r.context_id.as_str()),
    )?;
    let face_rows: BTreeMap<_, _> = graph
        .faces
        .iter()
        .map(|f| (f.face_id.as_str(), f))
        .collect();
    let mut video_tracks = BTreeMap::<String, Vec<StoredVideoObservation>>::new();
    let mut video_namespaces = BTreeMap::<(&str, &str, u32), Option<String>>::new();
    let mut video_namespace_owners = BTreeMap::<(&str, String), &str>::new();
    for row in &graph.video_observations {
        let observation = row.observation()?;
        let source = (
            row.media_key.as_str(),
            canonical_media_sha256(&row.media_fingerprint)
                .ok_or("invalid portable video fingerprint")?,
            observation.stream_index,
        );
        if video_namespaces
            .get(&source)
            .is_some_and(|namespace| namespace != &observation.identity_namespace)
        {
            return Err("portable video stream has inconsistent identity namespaces".into());
        }
        video_namespaces.insert(source, observation.identity_namespace.clone());
        if let Some(namespace) = observation.identity_namespace.as_ref() {
            let owner = (source.1, namespace.clone());
            if video_namespace_owners
                .get(&owner)
                .is_some_and(|media_key| *media_key != row.media_key)
            {
                return Err(
                    "portable video identity namespace belongs to multiple media sources".into(),
                );
            }
            video_namespace_owners.insert(owner, row.media_key.as_str());
        }
        let face = face_rows
            .get(row.face_id.as_str())
            .ok_or("portable video observation references missing Face")?;
        if face.media_key != row.media_key
            || face.media_fingerprint != row.media_fingerprint
            || observation.detection.bounds.as_slice() != face.bounds_normalized.as_slice()
        {
            return Err("portable video Face evidence mismatch".into());
        }
        references = references
            .checked_add(1)
            .ok_or("video reference count overflow")?;
        video_tracks
            .entry(row.track_id.clone())
            .or_default()
            .push(row.clone());
    }
    for rows in video_tracks.into_values() {
        super::video::video_track_snapshot(rows)?;
    }
    let mut media_context_keys = BTreeSet::new();
    for row in &graph.media_context {
        row.validate()?;
        validate_exchange_media_key(&row.media_key)?;
        if !media_context_keys.insert(&row.media_key) {
            return Err("duplicate canonical media context".into());
        }
    }
    for row in &graph.review_context {
        if !faces.contains(row.face_id.as_str())
            || !people.contains(row.person_id.as_str())
            || row.payload.len() > 8192
            || row.context_id
                != format!(
                    "{:x}",
                    Sha256::digest(format!("{}\0{}", row.face_id, row.person_id))
                )
        {
            return Err("portable review context has invalid graph reference or bounds".into());
        }
        let context = serde_json::from_str(&row.payload).map_err(|e| e.to_string())?;
        crate::match_context::ContextReviewCandidate {
            person_id: row.person_id.clone(),
            visual_similarity: 0.0,
            context,
        }
        .validate()?;
        references = references
            .checked_add(2)
            .ok_or("context reference count overflow")?;
    }
    let mut suggestion_provenance_by_operation =
        BTreeMap::<&str, Vec<&SuggestionSourceProvenance>>::new();
    let mut suggestion_provenance_pairs = BTreeSet::new();
    for provenance in &graph.suggestion_source_provenance {
        require_media_sha256(
            "SuggestionSourceProvenance media fingerprint",
            &provenance.media_fingerprint,
        )?;
        validate_exchange_media_key(&provenance.media_key)?;
        let embedding_created_at = parse_bounded_rfc3339_instant(
            "SuggestionSourceProvenance embedding_created_at is not RFC3339",
            &provenance.embedding_created_at,
        )?;
        let suggestion_created_at = parse_bounded_rfc3339_instant(
            "SuggestionSourceProvenance suggestion_created_at is not RFC3339",
            &provenance.suggestion_created_at,
        )?;
        let operation_created_at = parse_bounded_rfc3339_instant(
            "SuggestionSourceProvenance operation_created_at is not RFC3339",
            &provenance.operation_created_at,
        )?;
        if embedding_created_at > suggestion_created_at
            || suggestion_created_at > operation_created_at
        {
            return Err(format!(
                "SuggestionSourceProvenance {} has invalid timestamp chronology",
                provenance.provenance_id
            ));
        }
        references = references
            .checked_add(3)
            .ok_or_else(|| "identity reference count overflow".to_string())?;
        enforce_limit(
            "identity references",
            references,
            IDENTITY_BUNDLE_MAX_REFERENCES,
        )?;
        let owner = operation_by_id
            .get(provenance.operation_id.as_str())
            .copied()
            .ok_or_else(|| {
                format!(
                    "SuggestionSourceProvenance {} references an absent operation",
                    provenance.provenance_id
                )
            })?;
        if !matches!(
            owner.kind.as_str(),
            "correction_same"
                | "correction_batch_same"
                | "correction_different"
                | "correction_batch_different"
                | "correction_change_person"
                | "correction_batch_change_person"
        ) {
            return Err(format!(
                "SuggestionSourceProvenance {} has an unsupported owner kind",
                provenance.provenance_id
            ));
        }
        if provenance.provenance_id != suggestion_source_provenance_id(provenance)
            || !suggestion_provenance_pairs.insert((
                provenance.operation_id.as_str(),
                provenance.suggestion_id.as_str(),
            ))
        {
            return Err(format!(
                "SuggestionSourceProvenance {} is noncanonical or duplicated",
                provenance.provenance_id
            ));
        }
        suggestion_provenance_by_operation
            .entry(provenance.operation_id.as_str())
            .or_default()
            .push(provenance);
    }
    let constraint_by_pair = graph
        .constraints
        .iter()
        .map(|row| ((row.face_id.as_str(), row.person_id.as_str()), row))
        .collect::<BTreeMap<_, _>>();
    let mut trusted_set_summaries = BTreeMap::<&str, (usize, BTreeSet<&str>)>::new();
    for membership in &graph.trusted_memberships {
        let summary = trusted_set_summaries
            .entry(membership.set_id.as_str())
            .or_default();
        summary.0 += 1;
        summary.1.insert(membership.pose_bucket.as_str());
    }
    let confirmed_cover_media = graph
        .assignments
        .iter()
        .filter(|assignment| assignment.state == "operator_confirmed")
        .filter_map(|assignment| {
            face_by_id
                .get(assignment.face_id.as_str())
                .map(|face| (assignment.person_id.as_str(), face.media_key.as_str()))
        })
        .collect::<BTreeSet<_>>();
    for person in &graph.people {
        validate_text("bundle PersonId", &person.person_id)?;
        validate_text("bundle Person name", &person.name)?;
        for alias in &person.aliases {
            validate_text("bundle Person alias", alias)?;
        }
        if let Some(media_key) = &person.cover_media_key {
            validate_exchange_media_key(media_key)?;
            if !confirmed_cover_media.contains(&(person.person_id.as_str(), media_key.as_str())) {
                return Err(format!(
                    "Person {} cover must reference operator-confirmed media in that Person gallery",
                    person.person_id
                ));
            }
        }
    }
    for look in &graph.looks {
        validate_text("bundle LookId", &look.look_id)?;
        require_ref("Look Person", &look.person_id, &people, &mut references)?;
        validate_text("bundle Look name", &look.name)?;
    }
    for set in &graph.trusted_template_sets {
        validate_text("bundle TrustedTemplateSetId", &set.set_id)?;
        require_ref(
            "TrustedTemplateSet Look",
            &set.look_id,
            &looks,
            &mut references,
        )?;
        validate_text("bundle TrustedTemplateSet name", &set.name)?;
    }
    for face in &graph.faces {
        validate_face(face)?;
        require_media_sha256("portable Face media fingerprint", &face.media_fingerprint)?;
        if face.schema_generation != MATCH_SCHEMA_GENERATION {
            return Err(format!(
                "FaceObservation {} has incompatible schema generation",
                face.face_id
            ));
        }
    }
    for assignment in &graph.assignments {
        validate_text("bundle AssignmentId", &assignment.assignment_id)?;
        if assignment.assignment_id != assignment.face_id {
            return Err(format!(
                "Assignment {} stable ID must equal its FaceId",
                assignment.assignment_id
            ));
        }
        require_ref(
            "Assignment Face",
            &assignment.face_id,
            &faces,
            &mut references,
        )?;
        require_ref(
            "Assignment Person",
            &assignment.person_id,
            &people,
            &mut references,
        )?;
        validate_exchange_media_key(&assignment.media_key)?;
        let assignment_created = parse_bounded_rfc3339_instant(
            "bundle Assignment created_at is not RFC3339",
            &assignment.created_at,
        )?;
        let assignment_updated = parse_bounded_rfc3339_instant(
            "bundle Assignment updated_at is not RFC3339",
            &assignment.updated_at,
        )?;
        if assignment_created > assignment_updated {
            return Err(format!(
                "Assignment {} has invalid timestamp chronology",
                assignment.assignment_id
            ));
        }
        let face = face_by_id
            .get(assignment.face_id.as_str())
            .copied()
            .expect("assignment Face reference was checked above");
        let person = person_by_id
            .get(assignment.person_id.as_str())
            .copied()
            .expect("assignment Person reference was checked above");
        let revisions_are_current = face.face_revision == assignment.face_revision
            && person.revision == assignment.person_revision;
        let is_proven_stale_strict_undo = assignment.state
            == AssignmentState::CommittedStrictAutomatic.as_str()
            && assignment.face_revision <= face.face_revision
            && assignment.person_revision <= person.revision
            && undo_restored_strict_assignments
                .get(&assignment.assignment_id)
                .is_some_and(|restored| restored.iter().any(|row| row == assignment));
        if face.media_key != assignment.media_key
            || (!revisions_are_current && !is_proven_stale_strict_undo)
        {
            return Err(format!(
                "Assignment {} has stale Face/Person revision or media provenance",
                assignment.assignment_id
            ));
        }
        validate_text("assignment provenance", &assignment.provenance)?;
        match assignment.look_id.as_deref() {
            Some(look_id) => {
                require_ref("Assignment Look", look_id, &looks, &mut references)?;
                let look = look_by_id.get(look_id).copied().unwrap();
                if look.person_id != assignment.person_id || assignment.placement != "look" {
                    return Err(format!(
                        "Assignment {} has invalid explicit Look membership",
                        assignment.assignment_id
                    ));
                }
            }
            None if assignment.placement == "unsorted" => {}
            None => {
                return Err(format!(
                    "Assignment {} without a Look must be Unsorted",
                    assignment.assignment_id
                ))
            }
        }
        match assignment.state.as_str() {
            "operator_confirmed" => {
                if !assignment.locked
                    || assignment.model_generation.is_some()
                    || assignment.calibration_generation.is_some()
                    || assignment.envelope_hash.is_some()
                {
                    return Err(format!(
                        "Assignment {} would alter operator-confirmed evidence semantics",
                        assignment.assignment_id
                    ));
                }
            }
            "committed_strict_automatic" => {
                if assignment.locked
                    || assignment.model_generation.is_none()
                    || assignment.calibration_generation.is_none()
                    || assignment.envelope_hash.is_none()
                {
                    return Err(format!(
                        "Assignment {} lost strict-automatic generation provenance",
                        assignment.assignment_id
                    ));
                }
                let generation = assignment.model_generation.as_deref().unwrap();
                require_ref(
                    "Assignment model generation",
                    generation,
                    &generations,
                    &mut references,
                )?;
                let generation = generation_by_id
                    .get(generation)
                    .copied()
                    .expect("strict assignment generation reference was checked above");
                if !generation.validated
                    || !matches!(generation.state.as_str(), "usable" | "active")
                    || assignment.provenance != "strict_recognition_v1"
                {
                    return Err(format!(
                        "Assignment {} has invalid strict-recognition provenance or generation",
                        assignment.assignment_id
                    ));
                }
                validate_text(
                    "strict assignment calibration generation",
                    assignment.calibration_generation.as_deref().unwrap(),
                )?;
                validate_sha256(
                    "strict assignment envelope hash",
                    assignment.envelope_hash.as_deref().unwrap(),
                )?;
            }
            "suggestion" => {
                return Err(
                    "suggestions must remain separate from committed assignments".to_string(),
                )
            }
            _ => {
                return Err(format!(
                    "Assignment {} has an unknown evidence state",
                    assignment.assignment_id
                ))
            }
        }
        require_ref(
            "Assignment operation",
            &assignment.operation_id,
            &operations,
            &mut references,
        )?;
        validate_live_assignment_owner(
            assignment,
            operation_by_id
                .get(assignment.operation_id.as_str())
                .copied()
                .expect("Assignment operation reference was checked above"),
            correction_by_operation.get(assignment.operation_id.as_str()),
        )?;
    }
    for suggestion in &graph.suggestions {
        parse_bounded_rfc3339_instant(
            "bundle Suggestion created_at is not RFC3339",
            &suggestion.created_at,
        )?;
        require_media_sha256(
            "portable Suggestion media fingerprint",
            &suggestion.media_fingerprint,
        )?;
        validate_text("bundle SuggestionId", &suggestion.suggestion_id)?;
        require_ref(
            "Suggestion Face",
            &suggestion.face_id,
            &faces,
            &mut references,
        )?;
        require_ref(
            "Suggestion Person",
            &suggestion.candidate_person_id,
            &people,
            &mut references,
        )?;
        require_ref(
            "Suggestion model generation",
            &suggestion.model_generation,
            &generations,
            &mut references,
        )?;
        let face = face_by_id
            .get(suggestion.face_id.as_str())
            .copied()
            .expect("Suggestion Face reference was checked above");
        let person = person_by_id
            .get(suggestion.candidate_person_id.as_str())
            .copied()
            .expect("Suggestion Person reference was checked above");
        let generation = generation_by_id
            .get(suggestion.model_generation.as_str())
            .copied()
            .expect("Suggestion generation reference was checked above");
        if suggestion.suggestion_id
            != suggestion_id(&suggestion.face_id, &suggestion.candidate_person_id)
        {
            return Err(format!(
                "Suggestion {} is not the canonical Face/Person logical ID",
                suggestion.suggestion_id
            ));
        }
        if assignment_by_face.contains_key(suggestion.face_id.as_str()) {
            return Err(format!(
                "Suggestion {} conflicts with a committed assignment for its FaceId",
                suggestion.suggestion_id
            ));
        }
        let revisions_and_fingerprint_are_current = face.face_revision == suggestion.face_revision
            && face.media_fingerprint == suggestion.media_fingerprint
            && person.revision == suggestion.person_revision;
        let is_proven_stale_undo = suggestion.face_revision <= face.face_revision
            && suggestion.person_revision <= person.revision
            && undo_restored_suggestions
                .get(&suggestion.suggestion_id)
                .is_some_and(|restored| restored.iter().any(|row| row == suggestion));
        if !suggestion.similarity.is_finite()
            || (!revisions_and_fingerprint_are_current && !is_proven_stale_undo)
            || !generation.validated
            || !matches!(generation.state.as_str(), "usable" | "active")
        {
            return Err(format!(
                "Suggestion {} has stale revision, fingerprint, score, or model provenance",
                suggestion.suggestion_id
            ));
        }
        validate_text("Suggestion JobId provenance", &suggestion.job_id)?;
        match (
            suggestion.calibration_generation.as_deref(),
            suggestion.envelope_hash.as_deref(),
        ) {
            (None, None) => {}
            (Some(calibration), Some(envelope)) => {
                validate_text("Suggestion calibration generation", calibration)?;
                validate_sha256("Suggestion envelope hash", envelope)?;
            }
            _ => {
                return Err(format!(
                    "Suggestion {} has incomplete calibration provenance",
                    suggestion.suggestion_id
                ))
            }
        }
        if constraint_by_pair.contains_key(&(
            suggestion.face_id.as_str(),
            suggestion.candidate_person_id.as_str(),
        )) {
            return Err(format!(
                "Suggestion {} targets a cannot-linked Face/Person pair",
                suggestion.suggestion_id
            ));
        }
    }
    for membership in &graph.trusted_memberships {
        require_ref(
            "Trusted membership set",
            &membership.set_id,
            &sets,
            &mut references,
        )?;
        require_ref(
            "Trusted membership Look",
            &membership.look_id,
            &looks,
            &mut references,
        )?;
        require_ref(
            "Trusted membership Face",
            &membership.face_id,
            &faces,
            &mut references,
        )?;
        require_ref(
            "Trusted membership model generation",
            &membership.model_generation,
            &generations,
            &mut references,
        )?;
        require_ref(
            "Trusted membership operation",
            &membership.operation_id,
            &operations,
            &mut references,
        )?;
        validate_live_typed_effect_owner(
            operation_by_id
                .get(membership.operation_id.as_str())
                .copied()
                .expect("trusted membership operation reference was checked above"),
            correction_by_operation.get(membership.operation_id.as_str()),
            CorrectionTable::TrustedMember,
            &membership.membership_id,
            membership,
            "authorize_trusted_reference",
        )?;
        let set = set_by_id.get(membership.set_id.as_str()).copied().unwrap();
        let face = face_by_id
            .get(membership.face_id.as_str())
            .copied()
            .expect("trusted Face reference was checked above");
        let generation = generation_by_id
            .get(membership.model_generation.as_str())
            .copied()
            .expect("trusted generation reference was checked above");
        let assignment = assignment_by_face
            .get(membership.face_id.as_str())
            .copied()
            .ok_or_else(|| {
                format!(
                    "Trusted membership {} has no committed assignment",
                    membership.membership_id
                )
            })?;
        let (set_membership_count, set_pose_buckets) = trusted_set_summaries
            .get(membership.set_id.as_str())
            .expect("trusted membership summary was built above");
        let diversity_passed = *set_membership_count == 1 || set_pose_buckets.len() > 1;
        if membership.membership_id != trusted_member_id(&membership.set_id, &membership.face_id)
            || !membership.authorized
            || !membership.alignment_valid
            || !membership.quality_passed
            || !membership.pose_passed
            || !membership.diversity_passed
            || membership.diversity_passed != diversity_passed
            || set.look_id != membership.look_id
            || assignment.state != "operator_confirmed"
            || assignment.look_id.as_deref() != Some(membership.look_id.as_str())
            || !assignment.locked
            || face.face_revision != membership.face_revision
            || face.media_fingerprint != membership.media_fingerprint
            || !face.alignment_valid
            || membership.alignment_valid != face.alignment_valid
            || !face.quality.is_finite()
            || !membership.quality_score.is_finite()
            || !membership.quality_threshold.is_finite()
            || membership.quality_score.to_bits() != face.quality.to_bits()
            || membership.quality_threshold.to_bits() != TRUSTED_MIN_QUALITY.to_bits()
            || face.quality < membership.quality_threshold
            || membership.pose_bucket != face.pose_bucket
            || !is_real_yaw_bucket(&face.pose_bucket)
            || membership.policy_version != TRUSTED_POLICY_VERSION
            || membership.embedding_id
                != embedding_id(&membership.face_id, &membership.model_generation)
            || !generation.validated
            || generation.state != "active"
        {
            return Err(format!("Trusted membership {} does not match the live trusted-reference authorization evidence contract", membership.membership_id));
        }
        validate_text("trusted membership provenance", &membership.provenance)?;
    }
    for constraint in &graph.constraints {
        require_ref(
            "Constraint Face",
            &constraint.face_id,
            &faces,
            &mut references,
        )?;
        if constraint.constraint_id != cannot_link_id(&constraint.face_id, &constraint.person_id)
            || !constraint.operator_owned
        {
            return Err(format!(
                "CannotLinkConstraint {} is not canonical operator-owned evidence",
                constraint.constraint_id
            ));
        }
        if assignment_by_face
            .get(constraint.face_id.as_str())
            .is_some_and(|assignment| assignment.person_id == constraint.person_id)
        {
            return Err(format!(
                "CannotLinkConstraint {} conflicts with a committed assignment",
                constraint.constraint_id
            ));
        }
        require_ref(
            "Constraint Person",
            &constraint.person_id,
            &people,
            &mut references,
        )?;
        require_ref(
            "Constraint operation",
            &constraint.operation_id,
            &operations,
            &mut references,
        )?;
        validate_live_typed_effect_owner(
            operation_by_id
                .get(constraint.operation_id.as_str())
                .copied()
                .expect("constraint operation reference was checked above"),
            correction_by_operation.get(constraint.operation_id.as_str()),
            CorrectionTable::Constraint,
            &constraint.constraint_id,
            constraint,
            "different",
        )?;
    }
    for disposition in &graph.dispositions {
        require_ref(
            "Disposition Face",
            &disposition.face_id,
            &faces,
            &mut references,
        )?;
        require_ref(
            "Disposition operation",
            &disposition.operation_id,
            &operations,
            &mut references,
        )?;
        validate_live_typed_effect_owner(
            operation_by_id
                .get(disposition.operation_id.as_str())
                .copied()
                .expect("disposition operation reference was checked above"),
            correction_by_operation.get(disposition.operation_id.as_str()),
            CorrectionTable::Disposition,
            &disposition.face_id,
            disposition,
            "",
        )?;
        validate_exchange_media_key(&disposition.media_key)?;
        if !matches!(disposition.disposition.as_str(), "ignored" | "not_a_face") {
            return Err(format!(
                "FaceDisposition {} has unknown semantics",
                disposition.face_id
            ));
        }
    }
    for operation in &graph.operations {
        parse_operation_time(operation)?;
        validate_match_operation_indexed(
            operation,
            correction_by_operation.get(operation.operation_id.as_str()),
            &assignment_by_face,
            &operation_by_id,
        )?;
        if let Some(indexed) = correction_by_operation.get(operation.operation_id.as_str()) {
            let provenance_rows = suggestion_provenance_by_operation
                .get(operation.operation_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            validate_portable_correction_media_set(
                operation,
                &indexed.envelope,
                &face_by_id,
                &portable_face_media_evidence,
                provenance_rows,
            )?;
            validate_portable_correction_source_provenance(
                operation,
                indexed,
                &face_by_id,
                &person_by_id,
                &generation_by_id,
                provenance_rows,
                &correction_by_operation,
                &operation_by_id,
                &mut suggestion_provenance_anchor_work,
            )?;
        }
        // These are historical pointers, not live graph edges. WP-084 keeps
        // operations after a Person or face is removed so before/after JSON
        // remains an exact audit and undo record.
        if let Some(face_id) = &operation.face_id {
            validate_text("historical operation FaceId", face_id)?;
        }
        if let Some(person_id) = &operation.person_id {
            validate_text("historical operation PersonId", person_id)?;
        }
        validate_operation_json(
            &operation.operation_id,
            "before_json",
            &operation.before_json,
        )?;
        validate_operation_json(&operation.operation_id, "after_json", &operation.after_json)?;
    }
    validate_durable_operation_lineage(graph, &correction_by_operation)?;
    let mut mappings_by_operation = BTreeMap::<&str, Vec<&PortableCorrectionMediaOperation>>::new();
    let mut canonical_fingerprint_by_media = BTreeMap::<&str, String>::new();
    for (media_key, media_fingerprint) in portable_face_media_evidence.values() {
        let Some(media_fingerprint) = canonical_sha256_fingerprint(media_fingerprint) else {
            continue;
        };
        if let Some(existing) = canonical_fingerprint_by_media
            .insert(media_key.as_str(), media_fingerprint.to_ascii_lowercase())
        {
            if existing != media_fingerprint {
                return Err(format!(
                    "media key {media_key} has conflicting portable Face fingerprints"
                ));
            }
        }
    }
    for mapping in &graph.correction_media_operations {
        validate_text("correction media mapping id", &mapping.mapping_id)?;
        validate_exchange_media_key(&mapping.media_key)?;
        validate_sha256(
            "correction media mapping fingerprint",
            &mapping.media_fingerprint,
        )?;
        if canonical_fingerprint_by_media
            .get(mapping.media_key.as_str())
            .is_some_and(|expected| expected != &mapping.media_fingerprint)
        {
            return Err(format!(
                "CorrectionMediaOperation {} contradicts canonical Face fingerprint evidence",
                mapping.mapping_id
            ));
        }
        require_ref(
            "Correction media operation",
            &mapping.operation_id,
            &operations,
            &mut references,
        )?;
        validate_text("correction media operation kind", &mapping.kind)?;
        let operation = operation_by_id
            .get(mapping.operation_id.as_str())
            .copied()
            .expect("correction mapping operation reference was checked above");
        let correction_history = operation.kind == format!("correction_{}", mapping.kind);
        let direct_history = operation.kind == mapping.kind
            && matches!(
                mapping.kind.as_str(),
                "assign_operator_confirmed"
                    | "assign_committed_strict_automatic"
                    | "different"
                    | "not_sure"
                    | "move_to_look"
            );
        let no_change_correction =
            correction_history && matches!(mapping.kind.as_str(), "not_sure" | "batch_not_sure");
        if mapping.mapping_id
            != exchange_correction_media_mapping_id(&mapping.operation_id, &mapping.media_key)
            || operation.created_at != mapping.created_at
            || (!correction_history && !direct_history)
            || (correction_history && operation.reversible == no_change_correction)
        {
            return Err(format!(
                "CorrectionMediaOperation {} is not the canonical operation/media effect mapping",
                mapping.mapping_id
            ));
        }
        mappings_by_operation
            .entry(mapping.operation_id.as_str())
            .or_default()
            .push(mapping);
    }
    for operation in &graph.operations {
        if operation.kind.starts_with("correction_") {
            let indexed = correction_by_operation
                .get(operation.operation_id.as_str())
                .expect("correction envelope was indexed above");
            let expected = indexed
                .envelope
                .media_keys
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let actual: BTreeSet<&str> = mappings_by_operation
                .get(operation.operation_id.as_str())
                .map(|rows| rows.iter().map(|row| row.media_key.as_str()).collect())
                .unwrap_or_default();
            if actual != expected {
                return Err(format!(
                    "operation {} correction media mapping set contradicts its envelope",
                    operation.operation_id
                ));
            }
        } else if matches!(
            operation.kind.as_str(),
            "assign_operator_confirmed"
                | "assign_committed_strict_automatic"
                | "different"
                | "not_sure"
                | "move_to_look"
        ) {
            let embedded = direct_operation_embedded_media_key(operation)?;
            let current = operation
                .face_id
                .as_deref()
                .and_then(|face_id| portable_face_media_evidence.get(face_id));
            let expected = embedded.clone().or_else(|| {
                operation
                    .face_id
                    .as_deref()
                    .and_then(|face_id| face_by_id.get(face_id).map(|face| face.media_key.clone()))
            });
            let actual = mappings_by_operation
                .get(operation.operation_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if actual.len() != 1
                || expected
                    .as_deref()
                    .is_some_and(|expected| actual[0].media_key != expected)
                || current.is_some_and(|(media_key, fingerprint)| {
                    actual[0].media_key != *media_key
                        || canonical_sha256_fingerprint(fingerprint).is_some_and(|fingerprint| {
                            actual[0].media_fingerprint != fingerprint.to_ascii_lowercase()
                        })
                })
            {
                return Err(format!(
                    "operation {} direct media mapping must contain its one canonical media key and fingerprint",
                    operation.operation_id
                ));
            }
        }
    }
    for root in &graph.roots {
        validate_text("bundle root id", &root.root_id)?;
        validate_portable_path(&root.portable_path)?;
        for exclusion in &root.exclusions {
            validate_root_exclusion(exclusion)?;
        }
    }
    for generation in &graph.model_generations {
        validate_text("model generation", &generation.generation)?;
        validate_text("model generation state", &generation.state)?;
    }
    if references > IDENTITY_BUNDLE_MAX_REFERENCES {
        return Err(format!(
            "identity bundle has {references} references; limit is {IDENTITY_BUNDLE_MAX_REFERENCES}"
        ));
    }
    Ok(references)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExchangeCorrectionDeltaEnvelope {
    version: u32,
    kind: String,
    rows: Vec<CorrectionRowDelta>,
    face_ids: Vec<String>,
    media_keys: Vec<String>,
    identity_changed: bool,
    catalog_changed: bool,
}

fn bool_is_false(value: &bool) -> bool {
    !*value
}

/// Portable correction history retains the independently verifiable embedding
/// metadata but never carries the biometric vector. `vector_omitted` makes the
/// projection explicit so an imported undo can retire/regenerate derived rows
/// instead of fabricating evidence that was not transported.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct CorrectionFaceEmbedding {
    embedding_id: String,
    face_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "bool_is_false")]
    vector_omitted: bool,
    model_generation: String,
    schema_generation: String,
    media_fingerprint: String,
    face_revision: u64,
    job_id: String,
    active: bool,
    created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct CorrectionTrustedSearchEmbedding {
    membership_id: String,
    person_id: String,
    look_id: String,
    face_id: String,
    embedding_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "bool_is_false")]
    vector_omitted: bool,
    model_generation: String,
    created_at: String,
}

fn decode_correction_embedding(
    value: &Value,
    context: &str,
) -> Result<CorrectionFaceEmbedding, String> {
    let row: CorrectionFaceEmbedding = decode_strict_typed_value(value, context)?;
    if row.vector.is_some() == row.vector_omitted {
        return Err(format!(
            "{context} must contain either a local vector or an explicit portable omission"
        ));
    }
    Ok(row)
}

fn decode_correction_trusted_search(
    value: &Value,
    context: &str,
) -> Result<CorrectionTrustedSearchEmbedding, String> {
    let row: CorrectionTrustedSearchEmbedding = decode_strict_typed_value(value, context)?;
    if row.vector.is_some() == row.vector_omitted {
        return Err(format!(
            "{context} must contain either a local vector or an explicit portable omission"
        ));
    }
    Ok(row)
}

fn project_correction_value(
    table: &CorrectionTable,
    value: &mut Option<Value>,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    match table {
        CorrectionTable::Embedding => {
            let mut row = decode_correction_embedding(value, "portable correction Embedding")?;
            row.vector = None;
            row.vector_omitted = true;
            *value = serde_json::to_value(row).map_err(|error| error.to_string())?;
        }
        CorrectionTable::TrustedSearch => {
            let mut row =
                decode_correction_trusted_search(value, "portable correction TrustedSearch")?;
            row.vector = None;
            row.vector_omitted = true;
            *value = serde_json::to_value(row).map_err(|error| error.to_string())?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn project_match_operation_for_portable(
    operation: &MatchOperation,
) -> Result<MatchOperation, String> {
    if operation.kind != "undo_correction" && !operation.kind.starts_with("correction_") {
        return Ok(operation.clone());
    }
    let mut projected = operation.clone();
    if operation.kind.starts_with("correction_") {
        let mut before: Vec<(CorrectionTable, String, Option<Value>)> =
            decode_operation_field(operation, "before_json")?;
        for (table, _, value) in &mut before {
            project_correction_value(table, value)?;
        }
        projected.before_json = serde_json::to_string(&before)
            .map_err(|error| format!("serialize portable correction before_json: {error}"))?;
    }
    let mut envelope: ExchangeCorrectionDeltaEnvelope =
        decode_operation_field(operation, "after_json")?;
    for row in &mut envelope.rows {
        project_correction_value(&row.table, &mut row.before)?;
        project_correction_value(&row.table, &mut row.after)?;
    }
    projected.after_json = serde_json::to_string(&envelope)
        .map_err(|error| format!("serialize portable correction after_json: {error}"))?;
    Ok(projected)
}

fn direct_operation_embedded_media_key(
    operation: &MatchOperation,
) -> Result<Option<String>, String> {
    match operation.kind.as_str() {
        "assign_operator_confirmed" | "assign_committed_strict_automatic" => {
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            Ok(Some(after.media_key))
        }
        "move_to_look" => {
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            Ok(Some(after.media_key))
        }
        "different" => {
            let before: Option<Assignment> =
                decode_strict_operation_field(operation, "before_json")?;
            Ok(before.map(|assignment| assignment.media_key))
        }
        "not_sure" => {
            let (assignment, _): (Option<Assignment>, Option<CannotLinkConstraint>) =
                decode_strict_operation_field(operation, "before_json")?;
            Ok(assignment.map(|assignment| assignment.media_key))
        }
        _ => Ok(None),
    }
}

struct IndexedCorrectionEnvelope {
    envelope: ExchangeCorrectionDeltaEnvelope,
    rows: BTreeMap<(String, String), usize>,
}

impl IndexedCorrectionEnvelope {
    fn row(&self, table: &CorrectionTable, stable_id: &str) -> Option<&CorrectionRowDelta> {
        self.rows
            .get(&(
                correction_table_key(table).to_string(),
                stable_id.to_string(),
            ))
            .and_then(|index| self.envelope.rows.get(*index))
    }
}

fn correction_table_key(table: &CorrectionTable) -> &'static str {
    match table {
        CorrectionTable::VideoObservation => "video_observation",
        CorrectionTable::Person => "person",
        CorrectionTable::Look => "look",
        CorrectionTable::TemplateSet => "template_set",
        CorrectionTable::Face => "face",
        CorrectionTable::Embedding => "embedding",
        CorrectionTable::Assignment => "assignment",
        CorrectionTable::Constraint => "constraint",
        CorrectionTable::TrustedMember => "trusted_member",
        CorrectionTable::TrustedSearch => "trusted_search",
        CorrectionTable::Disposition => "disposition",
        CorrectionTable::Suggestion => "suggestion",
    }
}

fn index_correction_envelope(
    operation: &MatchOperation,
) -> Result<Option<IndexedCorrectionEnvelope>, String> {
    if operation.kind != "undo_correction" && !operation.kind.starts_with("correction_") {
        return Ok(None);
    }
    let envelope: ExchangeCorrectionDeltaEnvelope =
        decode_operation_field(operation, "after_json")?;
    let mut rows = BTreeMap::new();
    for (index, row) in envelope.rows.iter().enumerate() {
        let key = (
            correction_table_key(&row.table).to_string(),
            row.stable_id.clone(),
        );
        if rows.insert(key, index).is_some() {
            return Err(format!(
                "operation {} repeats a correction row effect",
                operation.operation_id
            ));
        }
    }
    Ok(Some(IndexedCorrectionEnvelope { envelope, rows }))
}

pub(super) fn validate_match_operation(
    operation: &MatchOperation,
    graph: &IdentityBundleGraph,
) -> Result<(), String> {
    validate_match_operations_indexed(
        std::slice::from_ref(operation),
        &graph.assignments,
        &graph.operations,
    )
}

pub(super) fn validate_match_operations(graph: &IdentityBundleGraph) -> Result<(), String> {
    validate_match_operations_indexed(&graph.operations, &graph.assignments, &graph.operations)
}

fn validate_match_operations_indexed(
    operations: &[MatchOperation],
    assignments: &[Assignment],
    operation_history: &[MatchOperation],
) -> Result<(), String> {
    let assignment_by_face = assignments
        .iter()
        .map(|row| (row.face_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let operation_by_id = operation_history
        .iter()
        .map(|row| (row.operation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut correction_envelopes = BTreeMap::new();
    for operation in operations {
        if let Some(indexed) = index_correction_envelope(operation)? {
            correction_envelopes.insert(operation.operation_id.as_str(), indexed);
        }
    }
    for operation in operations {
        validate_match_operation_indexed(
            operation,
            correction_envelopes.get(operation.operation_id.as_str()),
            &assignment_by_face,
            &operation_by_id,
        )?;
    }
    Ok(())
}

#[derive(Clone)]
struct DurableOperationTransition {
    table: CorrectionTable,
    stable_id: String,
    operation_id: String,
    before: Option<Value>,
    after: Option<Value>,
    created_at_instant: chrono::DateTime<chrono::FixedOffset>,
}

fn parse_operation_time(
    operation: &MatchOperation,
) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    parse_bounded_rfc3339_instant(
        &format!(
            "operation {} has invalid RFC3339 creation time",
            operation.operation_id
        ),
        &operation.created_at,
    )
}

fn parse_bounded_rfc3339_instant(
    label: &str,
    value: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    let parsed =
        chrono::DateTime::parse_from_rfc3339(value).map_err(|error| format!("{label}: {error}"))?;
    let ceiling = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(
            IDENTITY_BUNDLE_MAX_CLOCK_SKEW_SECONDS,
        ))
        .ok_or_else(|| "identity bundle clock-skew ceiling overflow".to_string())?;
    if parsed > ceiling {
        return Err(format!(
            "{label}: timestamp exceeds now plus the {IDENTITY_BUNDLE_MAX_CLOCK_SKEW_SECONDS}-second clock-skew allowance"
        ));
    }
    Ok(parsed)
}

fn operation_precedes(
    earlier_time: &chrono::DateTime<chrono::FixedOffset>,
    earlier_id: &str,
    later_time: &chrono::DateTime<chrono::FixedOffset>,
    later_id: &str,
) -> bool {
    earlier_time < later_time || (earlier_time == later_time && earlier_id < later_id)
}

fn exact_owner_edge_precedes(
    producer_time: &chrono::DateTime<chrono::FixedOffset>,
    consumer_time: &chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    // The consumer's typed `before` value names the producer operation as its
    // exact durable owner. That explicit edge is stronger causal evidence than
    // UUIDv4 lexical order when two commits share Match's millisecond clock.
    producer_time <= consumer_time
}

fn durable_assignment_value_is_confirmed(value: Option<&Value>) -> Result<bool, String> {
    value
        .map(|value| {
            decode_strict_typed_value::<Assignment>(value, "durable Assignment lineage")
                .map(|assignment| assignment.state == "operator_confirmed")
        })
        .transpose()
        .map(|value| value.unwrap_or(false))
}

fn durable_transition_is_relevant(row: &CorrectionRowDelta) -> Result<bool, String> {
    match row.table {
        CorrectionTable::Assignment => {
            Ok(durable_assignment_value_is_confirmed(row.before.as_ref())?
                || durable_assignment_value_is_confirmed(row.after.as_ref())?)
        }
        CorrectionTable::Constraint
        | CorrectionTable::TrustedMember
        | CorrectionTable::Disposition => Ok(true),
        _ => Ok(false),
    }
}

fn push_durable_transition(
    transitions: &mut Vec<DurableOperationTransition>,
    transition: DurableOperationTransition,
) -> Result<(), String> {
    if transitions.len() >= IDENTITY_BUNDLE_MAX_REFERENCES {
        return Err(format!(
            "durable operation lineage exceeds bounded limit of {IDENTITY_BUNDLE_MAX_REFERENCES}"
        ));
    }
    transitions.push(transition);
    Ok(())
}

fn durable_value_owner(table: &CorrectionTable, value: &Value) -> Result<String, String> {
    match table {
        CorrectionTable::Assignment => Ok(decode_strict_typed_value::<Assignment>(
            value,
            "durable Assignment lineage owner",
        )?
        .operation_id),
        CorrectionTable::Constraint => Ok(decode_strict_typed_value::<CannotLinkConstraint>(
            value,
            "durable Constraint lineage owner",
        )?
        .operation_id),
        CorrectionTable::TrustedMember => {
            Ok(decode_strict_typed_value::<TrustedTemplateMembership>(
                value,
                "durable TrustedMember lineage owner",
            )?
            .operation_id)
        }
        CorrectionTable::Disposition => Ok(decode_strict_typed_value::<FaceDisposition>(
            value,
            "durable Disposition lineage owner",
        )?
        .operation_id),
        _ => Err("unsupported durable operation lineage table".to_string()),
    }
}

fn durable_lineage_values_equal(
    table: &CorrectionTable,
    earlier: &Value,
    later: &Value,
) -> Result<bool, String> {
    match table {
        CorrectionTable::Assignment => {
            let earlier: Assignment =
                decode_strict_typed_value(earlier, "durable Assignment lineage source")?;
            let later: Assignment =
                decode_strict_typed_value(later, "durable Assignment lineage target")?;
            Ok(earlier.assignment_id == later.assignment_id
                && earlier.face_id == later.face_id
                && earlier.person_id == later.person_id
                && earlier.media_key == later.media_key
                && earlier.look_id == later.look_id
                && earlier.placement == later.placement
                && earlier.state == later.state
                && earlier.provenance == later.provenance
                && earlier.locked == later.locked
                && earlier.model_generation == later.model_generation
                && earlier.calibration_generation == later.calibration_generation
                && earlier.envelope_hash == later.envelope_hash
                && earlier.operation_id == later.operation_id
                && earlier.created_at == later.created_at
                && earlier.face_revision <= later.face_revision
                && earlier.person_revision <= later.person_revision)
        }
        CorrectionTable::Constraint => {
            correction_row_values_semantically_equal(table, earlier, later)
        }
        CorrectionTable::TrustedMember => {
            let earlier: TrustedTemplateMembership =
                decode_strict_typed_value(earlier, "durable TrustedMember lineage source")?;
            let later: TrustedTemplateMembership =
                decode_strict_typed_value(later, "durable TrustedMember lineage target")?;
            Ok(earlier.membership_id == later.membership_id
                && earlier.set_id == later.set_id
                && earlier.look_id == later.look_id
                && earlier.face_id == later.face_id
                && earlier.authorized == later.authorized
                && earlier.alignment_valid == later.alignment_valid
                && earlier.quality_passed == later.quality_passed
                && earlier.pose_passed == later.pose_passed
                && earlier.diversity_passed == later.diversity_passed
                && earlier.provenance == later.provenance
                && earlier.model_generation == later.model_generation
                && earlier.embedding_id == later.embedding_id
                && earlier.media_fingerprint == later.media_fingerprint
                && earlier.quality_score == later.quality_score
                && earlier.quality_threshold == later.quality_threshold
                && earlier.pose_bucket == later.pose_bucket
                && earlier.policy_version == later.policy_version
                && earlier.operation_id == later.operation_id
                && earlier.created_at == later.created_at
                && earlier.face_revision <= later.face_revision)
        }
        CorrectionTable::Disposition => {
            let earlier: FaceDisposition =
                decode_strict_typed_value(earlier, "durable Disposition lineage source")?;
            let later: FaceDisposition =
                decode_strict_typed_value(later, "durable Disposition lineage target")?;
            Ok(earlier.face_id == later.face_id
                && earlier.media_key == later.media_key
                && earlier.disposition == later.disposition
                && earlier.operation_id == later.operation_id
                && earlier.created_at == later.created_at
                && earlier.face_revision <= later.face_revision)
        }
        _ => Err("unsupported durable operation lineage table".to_string()),
    }
}

#[derive(Clone)]
struct OwnerlessDurableTransition {
    table: CorrectionTable,
    operation_id: String,
    created_at_instant: chrono::DateTime<chrono::FixedOffset>,
    before: Option<Value>,
    after: Option<Value>,
}

fn ownerless_durable_transition_is_relevant(row: &CorrectionRowDelta) -> Result<bool, String> {
    match row.table {
        CorrectionTable::Person
        | CorrectionTable::Look
        | CorrectionTable::TemplateSet
        | CorrectionTable::VideoObservation => Ok(true),
        CorrectionTable::Face => {
            for value in [row.before.as_ref(), row.after.as_ref()]
                .into_iter()
                .flatten()
            {
                let face: FaceObservation =
                    decode_strict_typed_value(value, "durable Face lineage")?;
                if face.operator_owned {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn ownerless_terminal_matches_monotonic(
    table: &CorrectionTable,
    historical: Option<&Value>,
    live: Option<&Value>,
) -> Result<bool, String> {
    let (Some(historical), Some(live)) = (historical, live) else {
        return Ok(historical.is_none() && live.is_none());
    };
    match table {
        CorrectionTable::VideoObservation => {
            let historical: StoredVideoObservation =
                decode_strict_typed_value(historical, "historical video terminal")?;
            let mut live: StoredVideoObservation =
                decode_strict_typed_value(live, "live video terminal")?;
            if live.revision < historical.revision {
                return Ok(false);
            }
            live.revision = historical.revision;
            Ok(live == historical)
        }
        CorrectionTable::Person => {
            let historical: Person =
                decode_strict_typed_value(historical, "ownerless historical Person terminal")?;
            let live: Person = decode_strict_typed_value(live, "ownerless live Person terminal")?;
            Ok(historical.person_id == live.person_id
                && historical.created_at == live.created_at
                && historical.revision <= live.revision
                && historical.catalog_revision <= live.catalog_revision)
        }
        CorrectionTable::Look => {
            let historical: Look =
                decode_strict_typed_value(historical, "ownerless historical Look terminal")?;
            let live: Look = decode_strict_typed_value(live, "ownerless live Look terminal")?;
            Ok(historical.look_id == live.look_id
                && historical.person_id == live.person_id
                && historical.created_at == live.created_at
                && historical.revision <= live.revision)
        }
        CorrectionTable::TemplateSet => {
            let historical: TrustedTemplateSet =
                decode_strict_typed_value(historical, "ownerless historical TemplateSet terminal")?;
            let live: TrustedTemplateSet =
                decode_strict_typed_value(live, "ownerless live TemplateSet terminal")?;
            Ok(historical.set_id == live.set_id
                && historical.look_id == live.look_id
                && historical.created_at == live.created_at
                && historical.revision <= live.revision)
        }
        CorrectionTable::Face => {
            let historical: FaceObservation =
                decode_strict_typed_value(historical, "ownerless historical Face terminal")?;
            let live: FaceObservation =
                decode_strict_typed_value(live, "ownerless live Face terminal")?;
            Ok(historical.face_id == live.face_id
                && historical.media_key == live.media_key
                && historical.media_fingerprint == live.media_fingerprint
                && historical.source_index == live.source_index
                && historical.operator_owned
                && live.operator_owned
                && historical.created_at == live.created_at
                && historical.face_revision <= live.face_revision)
        }
        _ => same_optional_correction_row_value(table, Some(historical), Some(live)),
    }
}

fn validate_ownerless_durable_operation_lineage(
    graph: &IdentityBundleGraph,
    correction_by_operation: &BTreeMap<&str, IndexedCorrectionEnvelope>,
) -> Result<(), String> {
    let mut transitions = BTreeMap::<(String, String), Vec<OwnerlessDurableTransition>>::new();
    let mut work = 0usize;
    for operation in &graph.operations {
        let Some(indexed) = correction_by_operation.get(operation.operation_id.as_str()) else {
            continue;
        };
        for row in &indexed.envelope.rows {
            if !ownerless_durable_transition_is_relevant(row)? {
                continue;
            }
            work = work
                .checked_add(1)
                .ok_or_else(|| "ownerless durable lineage work overflow".to_string())?;
            enforce_limit(
                "ownerless durable lineage work",
                work,
                IDENTITY_BUNDLE_MAX_REFERENCES,
            )?;
            transitions
                .entry((
                    correction_table_key(&row.table).to_string(),
                    row.stable_id.clone(),
                ))
                .or_default()
                .push(OwnerlessDurableTransition {
                    table: row.table.clone(),
                    operation_id: operation.operation_id.clone(),
                    created_at_instant: parse_operation_time(operation)?,
                    before: row.before.clone(),
                    after: row.after.clone(),
                });
        }
    }

    let mut live = BTreeMap::<(String, String), Value>::new();
    for observation in &graph.video_observations {
        live.insert(
            (
                "video_observation".to_string(),
                observation.observation_id.clone(),
            ),
            serde_json::to_value(observation).map_err(|e| e.to_string())?,
        );
    }
    for person in &graph.people {
        live.insert(
            ("person".to_string(), person.person_id.clone()),
            serde_json::to_value(person).map_err(|error| error.to_string())?,
        );
    }
    for look in &graph.looks {
        live.insert(
            ("look".to_string(), look.look_id.clone()),
            serde_json::to_value(look).map_err(|error| error.to_string())?,
        );
    }
    for set in &graph.trusted_template_sets {
        live.insert(
            ("template_set".to_string(), set.set_id.clone()),
            serde_json::to_value(set).map_err(|error| error.to_string())?,
        );
    }
    for face in graph.faces.iter().filter(|face| face.operator_owned) {
        live.insert(
            ("face".to_string(), face.face_id.clone()),
            serde_json::to_value(face).map_err(|error| error.to_string())?,
        );
    }

    for (key, rows) in &mut transitions {
        rows.sort_by(|left, right| {
            (&left.created_at_instant, &left.operation_id)
                .cmp(&(&right.created_at_instant, &right.operation_id))
        });
        for pair in rows.windows(2) {
            if !ownerless_terminal_matches_monotonic(
                &pair[0].table,
                pair[0].after.as_ref(),
                pair[1].before.as_ref(),
            )? {
                return Err(format!(
                    "operation {} ownerless durable effect is not consumed by operation {}",
                    pair[0].operation_id, pair[1].operation_id
                ));
            }
        }
        let terminal = rows
            .last()
            .expect("ownerless lineage map entries are non-empty");
        if !ownerless_terminal_matches_monotonic(
            &terminal.table,
            terminal.after.as_ref(),
            live.get(key),
        )? {
            return Err(format!(
                "operation {} ownerless durable effect contradicts its live terminal",
                terminal.operation_id
            ));
        }
    }
    Ok(())
}

fn validate_durable_operation_lineage(
    graph: &IdentityBundleGraph,
    correction_by_operation: &BTreeMap<&str, IndexedCorrectionEnvelope>,
) -> Result<(), String> {
    let mut transitions = Vec::new();
    for operation in &graph.operations {
        if let Some(indexed) = correction_by_operation.get(operation.operation_id.as_str()) {
            for row in &indexed.envelope.rows {
                if durable_transition_is_relevant(row)? {
                    push_durable_transition(
                        &mut transitions,
                        DurableOperationTransition {
                            table: row.table.clone(),
                            stable_id: row.stable_id.clone(),
                            operation_id: operation.operation_id.clone(),
                            before: row.before.clone(),
                            after: row.after.clone(),
                            created_at_instant: parse_operation_time(operation)?,
                        },
                    )?;
                }
            }
            continue;
        }
        match operation.kind.as_str() {
            "assign_operator_confirmed" => {
                let before: Option<Assignment> =
                    decode_strict_operation_field(operation, "before_json")?;
                let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
                push_durable_transition(
                    &mut transitions,
                    DurableOperationTransition {
                        table: CorrectionTable::Assignment,
                        stable_id: after.assignment_id.clone(),
                        operation_id: operation.operation_id.clone(),
                        before: before
                            .map(serde_json::to_value)
                            .transpose()
                            .map_err(|error| error.to_string())?,
                        after: Some(
                            serde_json::to_value(after).map_err(|error| error.to_string())?,
                        ),
                        created_at_instant: parse_operation_time(operation)?,
                    },
                )?;
            }
            "move_to_look" => {
                let before: Assignment = decode_strict_operation_field(operation, "before_json")?;
                let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
                if before.state == "operator_confirmed" || after.state == "operator_confirmed" {
                    push_durable_transition(
                        &mut transitions,
                        DurableOperationTransition {
                            table: CorrectionTable::Assignment,
                            stable_id: after.assignment_id.clone(),
                            operation_id: operation.operation_id.clone(),
                            before: Some(
                                serde_json::to_value(before).map_err(|error| error.to_string())?,
                            ),
                            after: Some(
                                serde_json::to_value(after).map_err(|error| error.to_string())?,
                            ),
                            created_at_instant: parse_operation_time(operation)?,
                        },
                    )?;
                }
            }
            "different" => {
                let before: Option<Assignment> =
                    decode_strict_operation_field(operation, "before_json")?;
                if before
                    .as_ref()
                    .is_some_and(|assignment| assignment.state == "operator_confirmed")
                {
                    let before = before.expect("confirmed Assignment was checked above");
                    push_durable_transition(
                        &mut transitions,
                        DurableOperationTransition {
                            table: CorrectionTable::Assignment,
                            stable_id: before.assignment_id.clone(),
                            operation_id: operation.operation_id.clone(),
                            before: Some(
                                serde_json::to_value(before).map_err(|error| error.to_string())?,
                            ),
                            after: None,
                            created_at_instant: parse_operation_time(operation)?,
                        },
                    )?;
                }
                let constraint: CannotLinkConstraint =
                    decode_strict_operation_field(operation, "after_json")?;
                push_durable_transition(
                    &mut transitions,
                    DurableOperationTransition {
                        table: CorrectionTable::Constraint,
                        stable_id: constraint.constraint_id.clone(),
                        operation_id: operation.operation_id.clone(),
                        before: None,
                        after: Some(
                            serde_json::to_value(constraint).map_err(|error| error.to_string())?,
                        ),
                        created_at_instant: parse_operation_time(operation)?,
                    },
                )?;
            }
            "authorize_trusted_reference" => {
                let before: Option<TrustedTemplateMembership> =
                    decode_strict_operation_field(operation, "before_json")?;
                let after: TrustedTemplateMembership =
                    decode_strict_operation_field(operation, "after_json")?;
                push_durable_transition(
                    &mut transitions,
                    DurableOperationTransition {
                        table: CorrectionTable::TrustedMember,
                        stable_id: after.membership_id.clone(),
                        operation_id: operation.operation_id.clone(),
                        before: before
                            .map(serde_json::to_value)
                            .transpose()
                            .map_err(|error| error.to_string())?,
                        after: Some(
                            serde_json::to_value(after).map_err(|error| error.to_string())?,
                        ),
                        created_at_instant: parse_operation_time(operation)?,
                    },
                )?;
            }
            _ => {}
        }
    }

    let mut consumers = BTreeMap::<(String, String, String), usize>::new();
    let mut creation_keys = BTreeSet::<(String, String, String)>::new();
    let mut creations_by_stable = BTreeMap::<(String, String), Vec<usize>>::new();
    for (index, transition) in transitions.iter().enumerate() {
        let table = correction_table_key(&transition.table).to_string();
        if let Some(before) = transition.before.as_ref() {
            let owner = durable_value_owner(&transition.table, before)?;
            if consumers
                .insert((table.clone(), transition.stable_id.clone(), owner), index)
                .is_some()
            {
                return Err(
                    "durable operation lineage has multiple consumers for one effect".to_string(),
                );
            }
        }
        if transition.before.is_none() {
            if let Some(after) = transition.after.as_ref() {
                let owner = durable_value_owner(&transition.table, after)?;
                if owner != transition.operation_id {
                    return Err(format!(
                        "operation {} durable effect is owned by another operation",
                        transition.operation_id
                    ));
                }
                if !creation_keys.insert((table.clone(), transition.stable_id.clone(), owner)) {
                    return Err("durable operation lineage has duplicate creations".to_string());
                }
                creations_by_stable
                    .entry((table, transition.stable_id.clone()))
                    .or_default()
                    .push(index);
            }
        }
    }
    for rows in creations_by_stable.values_mut() {
        rows.sort_by(|left, right| {
            (
                &transitions[*left].created_at_instant,
                &transitions[*left].operation_id,
            )
                .cmp(&(
                    &transitions[*right].created_at_instant,
                    &transitions[*right].operation_id,
                ))
        });
    }

    let mut live = BTreeMap::<(String, String), Value>::new();
    for assignment in &graph.assignments {
        if assignment.state == "operator_confirmed" {
            live.insert(
                (
                    correction_table_key(&CorrectionTable::Assignment).to_string(),
                    assignment.assignment_id.clone(),
                ),
                serde_json::to_value(assignment).map_err(|error| error.to_string())?,
            );
        }
    }
    for constraint in &graph.constraints {
        live.insert(
            (
                correction_table_key(&CorrectionTable::Constraint).to_string(),
                constraint.constraint_id.clone(),
            ),
            serde_json::to_value(constraint).map_err(|error| error.to_string())?,
        );
    }
    for membership in &graph.trusted_memberships {
        live.insert(
            (
                correction_table_key(&CorrectionTable::TrustedMember).to_string(),
                membership.membership_id.clone(),
            ),
            serde_json::to_value(membership).map_err(|error| error.to_string())?,
        );
    }
    for disposition in &graph.dispositions {
        live.insert(
            (
                correction_table_key(&CorrectionTable::Disposition).to_string(),
                disposition.face_id.clone(),
            ),
            serde_json::to_value(disposition).map_err(|error| error.to_string())?,
        );
    }

    let mut successors = vec![None; transitions.len()];
    let mut indegrees = vec![0usize; transitions.len()];
    for (index, transition) in transitions.iter().enumerate() {
        let table = correction_table_key(&transition.table).to_string();
        let live_key = (table.clone(), transition.stable_id.clone());
        let Some(after) = transition.after.as_ref() else {
            if !live.contains_key(&live_key) {
                continue;
            }
            let deleted_owner = transition
                .before
                .as_ref()
                .map(|before| durable_value_owner(&transition.table, before))
                .transpose()?;
            let creation = creations_by_stable
                .get(&live_key)
                .and_then(|rows| {
                    rows.iter().copied().find(|candidate| {
                        deleted_owner.as_deref()
                            != Some(transitions[*candidate].operation_id.as_str())
                            && operation_precedes(
                                &transition.created_at_instant,
                                &transition.operation_id,
                                &transitions[*candidate].created_at_instant,
                                &transitions[*candidate].operation_id,
                            )
                    })
                })
                .ok_or_else(|| {
                    format!(
                        "operation {} durable deletion contradicts its live terminal",
                        transition.operation_id
                    )
                })?;
            if indegrees[creation] != 0 {
                return Err("durable operation lineage has multiple predecessors".to_string());
            }
            successors[index] = Some(creation);
            indegrees[creation] = indegrees[creation]
                .checked_add(1)
                .ok_or_else(|| "durable operation lineage indegree overflow".to_string())?;
            continue;
        };
        let owner = durable_value_owner(&transition.table, after)?;
        if owner != transition.operation_id {
            return Err(format!(
                "operation {} durable effect is owned by another operation",
                transition.operation_id
            ));
        }
        let consumer_key = (table, transition.stable_id.clone(), owner.clone());
        if let Some(next) = consumers.get(&consumer_key).copied() {
            if next == index {
                return Err("durable operation lineage contains a self-edge".to_string());
            }
            let consumer = &transitions[next];
            let before = consumer
                .before
                .as_ref()
                .expect("consumer index only contains before values");
            if !exact_owner_edge_precedes(
                &transition.created_at_instant,
                &consumer.created_at_instant,
            ) || !durable_lineage_values_equal(&transition.table, after, before)?
            {
                return Err(format!(
                    "operation {} durable effect is not causally consumed by operation {}",
                    transition.operation_id, consumer.operation_id
                ));
            }
            successors[index] = Some(next);
            indegrees[next] = indegrees[next]
                .checked_add(1)
                .ok_or_else(|| "durable operation lineage indegree overflow".to_string())?;
            continue;
        }
        let current_value = live.get(&live_key).ok_or_else(|| {
            format!(
                "operation {} durable effect has no typed consumer or live terminal",
                transition.operation_id
            )
        })?;
        if durable_value_owner(&transition.table, current_value)? != owner
            || !durable_lineage_values_equal(&transition.table, after, current_value)?
        {
            return Err(format!(
                "operation {} durable effect contradicts its live terminal",
                transition.operation_id
            ));
        }
    }
    let mut ready = indegrees
        .iter()
        .enumerate()
        .filter_map(|(index, indegree)| (*indegree == 0).then_some(index))
        .collect::<Vec<_>>();
    let mut visited = 0usize;
    while let Some(index) = ready.pop() {
        visited += 1;
        if let Some(next) = successors[index] {
            indegrees[next] -= 1;
            if indegrees[next] == 0 {
                ready.push(next);
            }
        }
    }
    if visited != transitions.len() {
        return Err("durable operation lineage contains a cycle".to_string());
    }
    validate_ownerless_durable_operation_lineage(graph, correction_by_operation)
}

fn validate_match_operation_indexed(
    operation: &MatchOperation,
    indexed: Option<&IndexedCorrectionEnvelope>,
    assignment_by_face: &BTreeMap<&str, &Assignment>,
    operation_by_id: &BTreeMap<&str, &MatchOperation>,
) -> Result<(), String> {
    validate_text("bundle operation kind", &operation.kind)?;
    match operation.kind.as_str() {
        "assign_operator_confirmed" | "assign_committed_strict_automatic" => {
            if !operation.reversible {
                return Err(format!(
                    "operation {} assignment receipt must be reversible",
                    operation.operation_id
                ));
            }
            let before: Option<Assignment> =
                decode_strict_operation_field(operation, "before_json")?;
            if let Some(before) = &before {
                validate_historical_assignment(before, &before.assignment_id)?;
            }
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            validate_historical_assignment(&after, &after.assignment_id)?;
            let expected_state = if operation.kind == "assign_operator_confirmed" {
                "operator_confirmed"
            } else {
                "committed_strict_automatic"
            };
            if before.as_ref().is_some_and(|before| {
                before.assignment_id != after.assignment_id || before.face_id != after.face_id
            }) || after.operation_id != operation.operation_id
                || after.state != expected_state
                || operation.face_id.as_deref() != Some(after.face_id.as_str())
                || operation.person_id.as_deref() != Some(after.person_id.as_str())
                || (expected_state == "operator_confirmed"
                    && (!after.locked
                        || after.model_generation.is_some()
                        || after.calibration_generation.is_some()
                        || after.envelope_hash.is_some()))
                || (expected_state == "committed_strict_automatic"
                    && (after.locked
                        || after.provenance != "strict_recognition_v1"
                        || after.model_generation.is_none()
                        || after.calibration_generation.is_none()
                        || after.envelope_hash.is_none()))
            {
                return Err(format!(
                    "operation {} assignment payload contradicts its typed kind",
                    operation.operation_id
                ));
            }
            if expected_state == "committed_strict_automatic" {
                validate_strict_assignment_chronology(operation, &after)?;
            }
        }
        "different" => validate_direct_different_operation(operation)?,
        "not_sure" => validate_direct_not_sure_operation(operation)?,
        "move_to_look" => validate_direct_move_operation(operation)?,
        "authorize_trusted_reference" => {
            if !operation.reversible {
                return Err(format!(
                    "operation {} trusted authorization must be reversible",
                    operation.operation_id
                ));
            }
            let before: Option<TrustedTemplateMembership> =
                decode_strict_operation_field(operation, "before_json")?;
            if let Some(before) = &before {
                validate_historical_membership(before, &before.membership_id)?;
            }
            let after: TrustedTemplateMembership =
                decode_strict_operation_field(operation, "after_json")?;
            validate_historical_membership(&after, &after.membership_id)?;
            if before.as_ref().is_some_and(|before| {
                before.membership_id != after.membership_id
                    || before.set_id != after.set_id
                    || before.look_id != after.look_id
                    || before.face_id != after.face_id
            }) || after.operation_id != operation.operation_id
                || operation.face_id.as_deref() != Some(after.face_id.as_str())
                || assignment_by_face
                    .get(after.face_id.as_str())
                    .is_some_and(|assignment| {
                        operation.person_id.as_deref() != Some(assignment.person_id.as_str())
                    })
            {
                return Err(format!(
                    "operation {} trusted authorization payload is inconsistent",
                    operation.operation_id
                ));
            }
        }
        "rekey_media_proven_move" => validate_rekey_operation(operation)?,
        "undo_correction" => validate_undo_operation(
            operation,
            operation_by_id,
            &indexed
                .ok_or_else(|| "undo operation has no indexed envelope".to_string())?
                .envelope,
        )?,
        kind if kind.starts_with("correction_") => validate_correction_operation(
            operation,
            &indexed
                .ok_or_else(|| "correction operation has no indexed envelope".to_string())?
                .envelope,
        )?,
        _ => {
            return Err(format!(
                "operation {} has an unknown Match operation kind {}",
                operation.operation_id, operation.kind
            ))
        }
    }
    Ok(())
}

fn validate_live_assignment_owner(
    assignment: &Assignment,
    operation: &MatchOperation,
    indexed: Option<&IndexedCorrectionEnvelope>,
) -> Result<(), String> {
    let reject = || {
        Err(format!(
            "Assignment {} is not authorized by operation kind {}",
            assignment.assignment_id, operation.kind
        ))
    };
    match operation.kind.as_str() {
        "assign_operator_confirmed" if assignment.state == "operator_confirmed" => {
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            if !same_assignment_logical_effect(&after, assignment) {
                return Err(format!(
                    "operation {} contradicts live Assignment evidence ownership",
                    operation.operation_id
                ));
            }
        }
        "assign_committed_strict_automatic" if assignment.state == "committed_strict_automatic" => {
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            if !same_assignment_logical_effect(&after, assignment) {
                return Err(format!(
                    "operation {} contradicts live Assignment evidence ownership",
                    operation.operation_id
                ));
            }
        }
        "move_to_look" => {
            let before: Assignment = decode_strict_operation_field(operation, "before_json")?;
            let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
            if !same_assignment_evidence(&before, &after)
                || !same_assignment_logical_effect(&after, assignment)
            {
                return Err(format!(
                    "operation {} move-to-Look changes Assignment evidence ownership",
                    operation.operation_id
                ));
            }
        }
        "undo_correction" => {
            let delta = indexed
                .ok_or_else(|| {
                    format!(
                        "operation {} has no indexed correction envelope",
                        operation.operation_id
                    )
                })?
                .row(&CorrectionTable::Assignment, &assignment.assignment_id)
                .ok_or_else(|| {
                    format!(
                        "operation {} omits its live Assignment effect {}",
                        operation.operation_id, assignment.assignment_id
                    )
                })?;
            let after = delta.after.as_ref().ok_or_else(|| {
                format!(
                    "operation {} deletes the live Assignment effect {} it claims to own",
                    operation.operation_id, assignment.assignment_id
                )
            })?;
            let recorded: Assignment = decode_strict_typed_value(
                after,
                &format!(
                    "operation {} live Assignment effect {}",
                    operation.operation_id, assignment.assignment_id
                ),
            )?;
            if !same_assignment_logical_effect(&recorded, assignment) {
                return Err(format!(
                    "operation {} contradicts live Assignment logical/evidence ownership",
                    operation.operation_id
                ));
            }
        }
        kind if kind.starts_with("correction_") => {
            let correction_kind = kind
                .strip_prefix("correction_")
                .expect("correction prefix matched above");
            let delta = indexed
                .ok_or_else(|| {
                    format!(
                        "operation {} has no indexed correction envelope",
                        operation.operation_id
                    )
                })?
                .row(&CorrectionTable::Assignment, &assignment.assignment_id)
                .ok_or_else(|| {
                    format!(
                        "operation {} omits the Assignment effect it owns",
                        operation.operation_id
                    )
                })?;
            let recorded: Assignment =
                serde_json::from_value(delta.after.clone().ok_or_else(|| {
                    format!(
                        "operation {} deletes the live Assignment it claims to own",
                        operation.operation_id
                    )
                })?)
                .map_err(|error| format!("decode live Assignment owner effect: {error}"))?;
            if !same_assignment_logical_effect(&recorded, assignment) {
                return Err(format!(
                    "operation {} contradicts live Assignment logical/evidence ownership",
                    operation.operation_id
                ));
            }
            let confirmation_provenance = match correction_kind {
                "change_person" => Some("change_person"),
                "same" => Some("same"),
                "batch_same" => Some("batch_same"),
                "batch_change_person" => Some("batch_change_person"),
                "manual_face_and_assign" => Some("manual_face"),
                "split_to_person" => Some("split_to_person"),
                _ => None,
            };
            if let Some(provenance) = confirmation_provenance {
                if assignment.state != "operator_confirmed"
                    || !assignment.locked
                    || assignment.provenance != provenance
                {
                    return reject();
                }
            } else if matches!(
                correction_kind,
                "move_to_look" | "same_person_new_look" | "merge_people" | "split_person"
            ) {
                let before: Assignment =
                    serde_json::from_value(delta.before.clone().ok_or_else(|| {
                        format!(
                            "operation {} preservation effect omits prior Assignment",
                            operation.operation_id
                        )
                    })?)
                    .map_err(|error| format!("decode prior Assignment owner effect: {error}"))?;
                if !same_assignment_evidence(&before, &recorded) {
                    return Err(format!(
                        "operation {} preservation effect changes Assignment evidence ownership",
                        operation.operation_id
                    ));
                }
            } else {
                return reject();
            }
        }
        _ => return reject(),
    }
    Ok(())
}

fn same_assignment_evidence(left: &Assignment, right: &Assignment) -> bool {
    left.state == right.state
        && left.provenance == right.provenance
        && left.locked == right.locked
        && left.model_generation == right.model_generation
        && left.calibration_generation == right.calibration_generation
        && left.envelope_hash == right.envelope_hash
}

fn same_assignment_logical_effect(left: &Assignment, right: &Assignment) -> bool {
    left.assignment_id == right.assignment_id
        && left.face_id == right.face_id
        && left.person_id == right.person_id
        && left.look_id == right.look_id
        && left.placement == right.placement
        && same_assignment_evidence(left, right)
}

fn validate_live_typed_effect_owner<T>(
    operation: &MatchOperation,
    indexed: Option<&IndexedCorrectionEnvelope>,
    table: CorrectionTable,
    stable_id: &str,
    current: &T,
    direct_kind: &str,
) -> Result<(), String>
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq,
{
    if !direct_kind.is_empty() && operation.kind == direct_kind {
        let recorded: T = decode_strict_operation_field(operation, "after_json")?;
        if &recorded == current {
            return Ok(());
        }
    } else if operation.kind == "undo_correction" || operation.kind.starts_with("correction_") {
        return require_exact_live_delta(
            operation,
            indexed.ok_or_else(|| {
                format!(
                    "operation {} has no indexed correction envelope",
                    operation.operation_id
                )
            })?,
            table,
            stable_id,
            current,
        );
    }
    Err(format!(
        "operation {} kind {} is not authorized to own live typed effect {}",
        operation.operation_id, operation.kind, stable_id
    ))
}

fn decode_operation_field<T: serde::de::DeserializeOwned>(
    operation: &MatchOperation,
    field: &str,
) -> Result<T, String> {
    let text = if field == "before_json" {
        &operation.before_json
    } else {
        &operation.after_json
    };
    preflight_identity_bundle_json(text.as_bytes()).map_err(|error| {
        format!(
            "operation {} {} nested JSON is invalid: {error}",
            operation.operation_id, field
        )
    })?;
    serde_json::from_str(text).map_err(|error| {
        format!(
            "operation {} {} typed payload is invalid: {error}",
            operation.operation_id, field
        )
    })
}

fn decode_strict_operation_field<T>(operation: &MatchOperation, field: &str) -> Result<T, String>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let value: Value = decode_operation_field(operation, field)?;
    decode_strict_typed_value(
        &value,
        &format!("operation {} {}", operation.operation_id, field),
    )
}

fn decode_strict_typed_value<T>(value: &Value, context: &str) -> Result<T, String>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let decoded: T = serde_json::from_value(value.clone())
        .map_err(|error| format!("{context} typed payload is invalid: {error}"))?;
    let canonical = serde_json::to_value(&decoded)
        .map_err(|error| format!("{context} typed payload cannot be canonicalized: {error}"))?;
    if !same_json_shape(value, &canonical) {
        return Err(format!(
            "{context} typed payload contains unsupported fields or shape"
        ));
    }
    Ok(decoded)
}

fn same_json_shape(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    right
                        .get(key)
                        .is_some_and(|right_value| same_json_shape(left_value, right_value))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_json_shape(left, right))
        }
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => true,
        _ => false,
    }
}

fn validate_direct_different_operation(operation: &MatchOperation) -> Result<(), String> {
    if !operation.reversible {
        return Err(format!(
            "operation {} Different receipt must be reversible",
            operation.operation_id
        ));
    }
    let before: Option<Assignment> = decode_strict_operation_field(operation, "before_json")?;
    if let Some(before) = &before {
        validate_historical_assignment(before, &before.assignment_id)?;
    }
    let constraint: CannotLinkConstraint = decode_strict_operation_field(operation, "after_json")?;
    validate_historical_constraint(&constraint, &constraint.constraint_id)?;
    if before.as_ref().is_some_and(|before| {
        before.face_id != constraint.face_id || before.person_id != constraint.person_id
    }) || constraint.operation_id != operation.operation_id
        || operation.face_id.as_deref() != Some(constraint.face_id.as_str())
        || operation.person_id.as_deref() != Some(constraint.person_id.as_str())
    {
        return Err(format!(
            "operation {} Different payload is inconsistent",
            operation.operation_id
        ));
    }
    Ok(())
}

fn validate_direct_not_sure_operation(operation: &MatchOperation) -> Result<(), String> {
    let before: (Option<Assignment>, Option<CannotLinkConstraint>) =
        decode_strict_operation_field(operation, "before_json")?;
    if let Some(assignment) = &before.0 {
        validate_historical_assignment(assignment, &assignment.assignment_id)?;
        if operation.face_id.as_deref() != Some(assignment.face_id.as_str()) {
            return Err(format!(
                "operation {} Not-sure before Assignment targets another FaceId",
                operation.operation_id
            ));
        }
    }
    if let Some(constraint) = &before.1 {
        validate_historical_constraint(constraint, &constraint.constraint_id)?;
        if operation.face_id.as_deref() != Some(constraint.face_id.as_str())
            || operation.person_id.as_deref() != Some(constraint.person_id.as_str())
        {
            return Err(format!(
                "operation {} Not-sure before constraint targets another Face/Person pair",
                operation.operation_id
            ));
        }
    }
    let after: Value = decode_operation_field(operation, "after_json")?;
    if operation.reversible
        || !after.is_null()
        || operation.face_id.is_none()
        || operation.person_id.is_none()
    {
        return Err(format!(
            "operation {} Not-sure must be a typed no-change receipt",
            operation.operation_id
        ));
    }
    Ok(())
}

fn validate_direct_move_operation(operation: &MatchOperation) -> Result<(), String> {
    let before: Assignment = decode_strict_operation_field(operation, "before_json")?;
    let after: Assignment = decode_strict_operation_field(operation, "after_json")?;
    validate_historical_assignment(&before, &before.assignment_id)?;
    validate_historical_assignment(&after, &after.assignment_id)?;
    if !operation.reversible
        || before.face_id != after.face_id
        || before.person_id != after.person_id
        || !same_assignment_evidence(&before, &after)
        || after.operation_id != operation.operation_id
        || after.look_id.is_none()
        || after.placement != "look"
        || operation.face_id.as_deref() != Some(after.face_id.as_str())
        || operation.person_id.as_deref() != Some(after.person_id.as_str())
    {
        return Err(format!(
            "operation {} move-to-Look payload is inconsistent",
            operation.operation_id
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RekeyBeforePayload {
    media_key: String,
    media_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RekeyAfterPayload {
    media_key: String,
    media_fingerprint: String,
    rekeyed_cover_person_ids: Vec<String>,
}

fn validate_rekey_operation(operation: &MatchOperation) -> Result<(), String> {
    let before: RekeyBeforePayload = decode_operation_field(operation, "before_json")?;
    let after: RekeyAfterPayload = decode_operation_field(operation, "after_json")?;
    validate_exchange_media_key(&before.media_key)?;
    validate_exchange_media_key(&after.media_key)?;
    validate_text("rekey media fingerprint", &before.media_fingerprint)?;
    if !operation.reversible
        || operation.face_id.is_some()
        || operation.person_id.is_some()
        || before.media_key == after.media_key
        || before.media_fingerprint != after.media_fingerprint
    {
        return Err(format!(
            "operation {} rekey payload is inconsistent",
            operation.operation_id
        ));
    }
    let mut person_ids = BTreeSet::new();
    for person_id in after.rekeyed_cover_person_ids {
        validate_text("rekey cover PersonId", &person_id)?;
        if !person_ids.insert(person_id.clone()) {
            return Err(format!(
                "operation {} rekey payload repeats PersonId {}",
                operation.operation_id, person_id
            ));
        }
    }
    Ok(())
}

fn validate_undo_operation(
    operation: &MatchOperation,
    operation_by_id: &BTreeMap<&str, &MatchOperation>,
    envelope: &ExchangeCorrectionDeltaEnvelope,
) -> Result<(), String> {
    let before: Value = decode_operation_field(operation, "before_json")?;
    let source_operation_id = before
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "operation {} undo receipt omits source operation_id",
                operation.operation_id
            )
        })?;
    let source = operation_by_id.get(source_operation_id).copied();
    if before.as_object().is_none_or(|object| object.len() != 2)
        || before.get("conflict_rows").and_then(Value::as_u64) != Some(0)
        || operation.reversible
        || operation.operation_id != format!("undo-{source_operation_id}")
        || source.is_none_or(|candidate| {
            !candidate.reversible || !candidate.kind.starts_with("correction_")
        })
    {
        return Err(format!(
            "operation {} undo receipt is not bound to one reversible source operation",
            operation.operation_id
        ));
    }
    validate_correction_envelope(operation, envelope, "undo_correction")?;
    if envelope.rows.is_empty() {
        return Err(format!(
            "operation {} undo receipt has no typed effects",
            operation.operation_id
        ));
    }
    let source = source.expect("undo source existence was checked above");
    let source_indexed = index_correction_envelope(source)?.ok_or_else(|| {
        format!(
            "operation {} undo source has no typed correction envelope",
            operation.operation_id
        )
    })?;
    validate_correction_operation(source, &source_indexed.envelope)?;
    validate_exact_undo_inverse(operation, source, envelope, &source_indexed.envelope)?;
    Ok(())
}

fn validate_exact_undo_inverse(
    undo: &MatchOperation,
    source: &MatchOperation,
    undo_envelope: &ExchangeCorrectionDeltaEnvelope,
    source_envelope: &ExchangeCorrectionDeltaEnvelope,
) -> Result<(), String> {
    if undo.face_id != source.face_id
        || undo.person_id != source.person_id
        || undo_envelope.face_ids != source_envelope.face_ids
        || undo_envelope.media_keys != source_envelope.media_keys
        || undo_envelope.identity_changed != source_envelope.identity_changed
        || undo_envelope.catalog_changed != source_envelope.catalog_changed
    {
        return Err(format!(
            "operation {} undo identity or affected sets contradict source {}",
            undo.operation_id, source.operation_id
        ));
    }

    let source_rows = source_envelope
        .rows
        .iter()
        .map(|row| {
            (
                (correction_table_key(&row.table), row.stable_id.as_str()),
                row,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let undo_rows = undo_envelope
        .rows
        .iter()
        .map(|row| {
            (
                (correction_table_key(&row.table), row.stable_id.as_str()),
                row,
            )
        })
        .collect::<BTreeMap<_, _>>();
    if undo_rows.keys().any(|key| !source_rows.contains_key(key))
        || source_rows.iter().any(|(key, row)| {
            !undo_rows.contains_key(key) && !portable_derived_effect_may_be_excluded(row)
        })
    {
        return Err(format!(
            "operation {} undo row set contradicts source {}",
            undo.operation_id, source.operation_id
        ));
    }

    for (key, source_row) in source_rows {
        let Some(undo_row) = undo_rows.get(&key) else {
            debug_assert!(portable_derived_effect_may_be_excluded(source_row));
            continue;
        };
        if !same_optional_correction_row_value(
            &source_row.table,
            source_row.after.as_ref(),
            undo_row.before.as_ref(),
        )? {
            return Err(format!(
                "operation {} undo before-state does not equal source {} after-state for {}:{}",
                undo.operation_id, source.operation_id, key.0, key.1
            ));
        }
        if !undo_after_matches_source_before(
            &source_row.table,
            source_row.before.as_ref(),
            undo_row.after.as_ref(),
            &undo.operation_id,
        )? {
            return Err(format!(
                "operation {} undo after-state is not the permitted inverse of source {} before-state for {}:{}",
                undo.operation_id, source.operation_id, key.0, key.1
            ));
        }
    }
    Ok(())
}

fn portable_derived_effect_may_be_excluded(row: &CorrectionRowDelta) -> bool {
    matches!(
        row.table,
        CorrectionTable::Embedding | CorrectionTable::TrustedSearch
    ) && row
        .before
        .as_ref()
        .or(row.after.as_ref())
        .is_some_and(|value| {
            value.get("vector").is_none()
                && value.get("vector_omitted").and_then(Value::as_bool) == Some(true)
        })
}

fn same_optional_correction_row_value(
    table: &CorrectionTable,
    left: Option<&Value>,
    right: Option<&Value>,
) -> Result<bool, String> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => correction_row_values_semantically_equal(table, left, right),
        _ => Ok(false),
    }
}

fn undo_after_matches_source_before(
    table: &CorrectionTable,
    source_before: Option<&Value>,
    undo_after: Option<&Value>,
    undo_operation_id: &str,
) -> Result<bool, String> {
    let (Some(source_before), Some(undo_after)) = (source_before, undo_after) else {
        return Ok(source_before.is_none() && undo_after.is_none());
    };
    let context = format!(
        "undo {undo_operation_id} inverse {}",
        correction_table_key(table)
    );
    match table {
        CorrectionTable::Person => {
            let source: Person = decode_strict_typed_value(source_before, &context)?;
            let mut undo: Person = decode_strict_typed_value(undo_after, &context)?;
            undo.revision = source.revision;
            undo.catalog_revision = source.catalog_revision;
            undo.updated_at = source.updated_at.clone();
            Ok(undo == source)
        }
        CorrectionTable::Look => {
            let source: Look = decode_strict_typed_value(source_before, &context)?;
            let mut undo: Look = decode_strict_typed_value(undo_after, &context)?;
            undo.revision = source.revision;
            undo.updated_at = source.updated_at.clone();
            Ok(undo == source)
        }
        CorrectionTable::TemplateSet => {
            same_optional_correction_row_value(table, Some(source_before), Some(undo_after))
        }
        CorrectionTable::VideoObservation => {
            let source: StoredVideoObservation =
                decode_strict_typed_value(source_before, &context)?;
            let mut undo: StoredVideoObservation = decode_strict_typed_value(undo_after, &context)?;
            if undo.revision <= source.revision {
                return Ok(false);
            }
            undo.revision = source.revision;
            Ok(undo == source)
        }
        CorrectionTable::Face => {
            let source: FaceObservation = decode_strict_typed_value(source_before, &context)?;
            let mut undo: FaceObservation = decode_strict_typed_value(undo_after, &context)?;
            undo.face_revision = source.face_revision;
            undo.updated_at = source.updated_at.clone();
            Ok(undo == source)
        }
        CorrectionTable::Assignment => {
            let source: Assignment = decode_strict_typed_value(source_before, &context)?;
            let mut undo: Assignment = decode_strict_typed_value(undo_after, &context)?;
            if source.state == AssignmentState::CommittedStrictAutomatic.as_str() {
                return Ok(undo == source);
            }
            if undo.operation_id != undo_operation_id {
                return Ok(false);
            }
            undo.operation_id = source.operation_id.clone();
            undo.updated_at = source.updated_at.clone();
            undo.face_revision = source.face_revision;
            undo.person_revision = source.person_revision;
            Ok(undo == source)
        }
        CorrectionTable::Constraint => {
            let source: CannotLinkConstraint = decode_strict_typed_value(source_before, &context)?;
            let mut undo: CannotLinkConstraint = decode_strict_typed_value(undo_after, &context)?;
            if undo.operation_id != undo_operation_id {
                return Ok(false);
            }
            undo.operation_id = source.operation_id.clone();
            Ok(undo == source)
        }
        CorrectionTable::Disposition => {
            let source: FaceDisposition = decode_strict_typed_value(source_before, &context)?;
            let mut undo: FaceDisposition = decode_strict_typed_value(undo_after, &context)?;
            if undo.operation_id != undo_operation_id {
                return Ok(false);
            }
            undo.operation_id = source.operation_id.clone();
            undo.face_revision = source.face_revision;
            undo.updated_at = source.updated_at.clone();
            Ok(undo == source)
        }
        CorrectionTable::Embedding => {
            let source = decode_correction_embedding(source_before, &context)?;
            let mut undo = decode_correction_embedding(undo_after, &context)?;
            undo.face_revision = source.face_revision;
            Ok(undo == source)
        }
        CorrectionTable::TrustedMember => {
            let source: TrustedTemplateMembership =
                decode_strict_typed_value(source_before, &context)?;
            let mut undo: TrustedTemplateMembership =
                decode_strict_typed_value(undo_after, &context)?;
            if undo.operation_id != undo_operation_id {
                return Ok(false);
            }
            undo.operation_id = source.operation_id.clone();
            undo.face_revision = source.face_revision;
            Ok(undo == source)
        }
        CorrectionTable::TrustedSearch => {
            same_optional_correction_row_value(table, Some(source_before), Some(undo_after))
        }
        CorrectionTable::Suggestion => {
            let source: Suggestion = decode_strict_typed_value(source_before, &context)?;
            let mut undo: Suggestion = decode_strict_typed_value(undo_after, &context)?;
            undo.face_revision = source.face_revision;
            undo.person_revision = source.person_revision;
            Ok(undo == source)
        }
    }
}

fn validate_correction_operation(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
) -> Result<(), String> {
    let kind = operation
        .kind
        .strip_prefix("correction_")
        .expect("correction prefix was matched by caller");
    if !is_closed_correction_kind(kind) {
        return Err(format!(
            "operation {} has unknown correction kind {}",
            operation.operation_id, kind
        ));
    }
    validate_correction_envelope(operation, envelope, kind)?;
    let no_change = matches!(kind, "not_sure" | "batch_not_sure");
    if no_change {
        let before: Vec<Value> = decode_operation_field(operation, "before_json")?;
        if !before.is_empty()
            || !envelope.rows.is_empty()
            || envelope.identity_changed
            || envelope.catalog_changed
            || operation.reversible
        {
            return Err(format!(
                "operation {} Not-sure correction must record no state effects",
                operation.operation_id
            ));
        }
        if kind == "not_sure"
            && (operation.face_id.as_deref() != envelope.face_ids.first().map(String::as_str)
                || envelope.face_ids.len() != 1
                || operation.person_id.is_none())
        {
            return Err(format!(
                "operation {} Not-sure correction does not bind its Face/Person identity",
                operation.operation_id
            ));
        }
        if kind == "batch_not_sure"
            && (operation.face_id.is_some()
                || operation.person_id.is_none()
                || envelope.face_ids.is_empty())
        {
            return Err(format!(
                "operation {} batch Not-sure correction does not bind its Person and Face set",
                operation.operation_id
            ));
        }
        return Ok(());
    }
    if !operation.reversible || envelope.rows.is_empty() {
        return Err(format!(
            "operation {} correction must contain reversible typed effects",
            operation.operation_id
        ));
    }
    let before: Vec<(CorrectionTable, String, Option<Value>)> =
        decode_operation_field(operation, "before_json")?;
    let expected_before = envelope
        .rows
        .iter()
        .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
        .collect::<Vec<_>>();
    if before != expected_before {
        return Err(format!(
            "operation {} correction before-state does not match its typed delta",
            operation.operation_id
        ));
    }
    if matches!(kind, "video_assign" | "video_track_split") {
        return validate_video_correction_effect(operation, envelope, kind);
    }
    if matches!(kind, "same" | "batch_same") {
        validate_same_correction_effect(operation, envelope, kind)?;
    }
    if matches!(
        kind,
        "different" | "this_is_not" | "batch_different" | "batch_this_is_not"
    ) {
        validate_different_correction_effect(operation, envelope, kind)?;
    } else if !matches!(kind, "same" | "batch_same") {
        validate_other_closed_correction_effect(operation, envelope, kind)?;
    }
    Ok(())
}

fn validate_video_correction_effect(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    kind: &str,
) -> Result<(), String> {
    if !envelope.identity_changed || envelope.catalog_changed || operation.face_id.is_some() {
        return Err("invalid video correction revision or scope".into());
    }
    let faces: BTreeSet<_> = envelope.face_ids.iter().cloned().collect();
    let media: BTreeSet<_> = envelope.media_keys.iter().cloned().collect();
    let mut affected = BTreeSet::new();
    let mut changed = 0usize;
    let mut source_closed = false;
    for row in &envelope.rows {
        if kind == "video_track_split" {
            if row.table != CorrectionTable::VideoObservation {
                return Err("video split changed non-membership truth".into());
            }
            let before: StoredVideoObservation = decode_strict_typed_value(
                row.before.as_ref().ok_or("video split missing before")?,
                "video split before",
            )?;
            let after: StoredVideoObservation = decode_strict_typed_value(
                row.after.as_ref().ok_or("video split missing after")?,
                "video split after",
            )?;
            before.observation()?;
            after.observation()?;
            let mut normalized = after.clone();
            normalized.track_id = before.track_id.clone();
            normalized.revision = before.revision;
            normalized.closed = before.closed;
            source_closed |= before.closed;
            let mut observation = after.observation()?;
            observation.track_id = before.track_id.clone();
            normalized.payload = serde_json::to_string(&observation).map_err(|e| e.to_string())?;
            if normalized != before
                || !after.closed
                || after.revision
                    != before
                        .revision
                        .checked_add(1)
                        .ok_or("video split revision overflow")?
            {
                return Err("video split changed observation provenance".into());
            }
            changed += usize::from(after.track_id != before.track_id);
            if !media.contains(&before.media_key) {
                return Err("video split outside media scope".into());
            }
            affected.insert(before.face_id);
        } else {
            match row.table {
                CorrectionTable::Assignment => {
                    let assignment: Assignment = decode_strict_typed_value(
                        row.after
                            .as_ref()
                            .ok_or("video Assign missing assignment")?,
                        "video Assign",
                    )?;
                    if row.before.is_some()
                        || assignment.operation_id != operation.operation_id
                        || operation.person_id.as_deref() != Some(&assignment.person_id)
                        || assignment.provenance != "video_assign"
                        || assignment.state != "operator_confirmed"
                        || !assignment.locked
                        || assignment.look_id.is_some()
                        || assignment.placement != "unsorted"
                        || assignment.model_generation.is_some()
                        || assignment.calibration_generation.is_some()
                        || assignment.envelope_hash.is_some()
                        || !media.contains(&assignment.media_key)
                    {
                        return Err("invalid explicit video Assign semantics".into());
                    }
                    affected.insert(assignment.face_id);
                }
                CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch => {
                    if row.before.is_none() || row.after.is_some() {
                        return Err(
                            "video Assign may only remove derived/trusted side effects".into()
                        );
                    }
                    let face_id = row
                        .before
                        .as_ref()
                        .and_then(|v| v.get("face_id"))
                        .and_then(Value::as_str)
                        .ok_or("video Assign removal missing FaceId")?;
                    if !faces.contains(face_id) {
                        return Err("video Assign removal outside scope".into());
                    }
                }
                _ => return Err("video Assign changed unsupported truth".into()),
            }
        }
    }
    if affected != faces
        || (kind == "video_track_split"
            && (!source_closed
                || changed == 0
                || changed >= affected.len()
                || operation.person_id.is_some()))
    {
        return Err("video correction does not preserve exact track scope".into());
    }
    Ok(())
}

fn validate_other_closed_correction_effect(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    kind: &str,
) -> Result<(), String> {
    let catalog_kind = matches!(kind, "remove_person" | "merge_people" | "split_person");
    if !envelope.identity_changed || envelope.catalog_changed != catalog_kind {
        return Err(format!(
            "operation {} correction kind {kind} has invalid revision effects",
            operation.operation_id
        ));
    }
    let single_face = matches!(
        kind,
        "change_person"
            | "move_to_look"
            | "same_person_new_look"
            | "remove_assignment"
            | "delete_face_analysis"
            | "manual_face"
            | "manual_face_and_assign"
            | "ignored"
            | "not_a_face"
    );
    if single_face
        && (envelope.face_ids.len() != 1
            || operation.face_id.as_deref() != envelope.face_ids.first().map(String::as_str))
    {
        return Err(format!(
            "operation {} correction kind {kind} does not bind its single Face effect",
            operation.operation_id
        ));
    }
    if !single_face && operation.face_id.is_some() {
        return Err(format!(
            "operation {} correction kind {kind} must use its canonical Face set",
            operation.operation_id
        ));
    }

    let face_set = envelope
        .face_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let media_set = envelope
        .media_keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut effect_face_ids = BTreeSet::new();
    let (removed_looks, removed_template_sets) = if kind == "remove_person" {
        let looks = envelope
            .rows
            .iter()
            .filter(|row| {
                row.table == CorrectionTable::Look && row.before.is_some() && row.after.is_none()
            })
            .map(|row| {
                decode_strict_typed_value::<Look>(
                    row.before.as_ref().unwrap(),
                    "remove-Person deleted Look",
                )
                .map(|look| (look.look_id.clone(), look))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let sets = envelope
            .rows
            .iter()
            .filter(|row| {
                row.table == CorrectionTable::TemplateSet
                    && row.before.is_some()
                    && row.after.is_none()
            })
            .map(|row| {
                decode_strict_typed_value::<TrustedTemplateSet>(
                    row.before.as_ref().unwrap(),
                    "remove-Person deleted TemplateSet",
                )
                .map(|set| (set.set_id.clone(), set))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        (looks, sets)
    } else {
        (BTreeMap::new(), BTreeMap::new())
    };
    let allowed = |table: &CorrectionTable| match kind {
        "change_person" | "batch_change_person" => matches!(
            table,
            CorrectionTable::Assignment
                | CorrectionTable::Constraint
                | CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
        ),
        "move_to_look" | "same_person_new_look" | "split_to_person" => matches!(
            table,
            CorrectionTable::Assignment
                | CorrectionTable::Look
                | CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
        ),
        "remove_assignment" | "batch_remove_assignments" => matches!(
            table,
            CorrectionTable::Assignment
                | CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
        ),
        "ignored" | "not_a_face" | "batch_ignore_face" | "batch_not_a_face" => matches!(
            table,
            CorrectionTable::Disposition
                | CorrectionTable::Embedding
                | CorrectionTable::Assignment
                | CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
        ),
        "delete_face_analysis" | "batch_delete_face_analysis" => matches!(
            table,
            CorrectionTable::Face
                | CorrectionTable::Embedding
                | CorrectionTable::Assignment
                | CorrectionTable::Constraint
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
                | CorrectionTable::Disposition
                | CorrectionTable::Suggestion
        ),
        "manual_face" => matches!(table, CorrectionTable::Face | CorrectionTable::Embedding),
        "manual_face_and_assign" => matches!(
            table,
            CorrectionTable::Face | CorrectionTable::Embedding | CorrectionTable::Assignment
        ),
        "remove_person" => matches!(
            table,
            CorrectionTable::Person
                | CorrectionTable::Look
                | CorrectionTable::TemplateSet
                | CorrectionTable::Assignment
                | CorrectionTable::Constraint
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
                | CorrectionTable::Suggestion
        ),
        "merge_people" => matches!(
            table,
            CorrectionTable::Person
                | CorrectionTable::Look
                | CorrectionTable::Assignment
                | CorrectionTable::Constraint
                | CorrectionTable::TrustedSearch
                | CorrectionTable::Suggestion
        ),
        "split_person" => matches!(
            table,
            CorrectionTable::Person
                | CorrectionTable::Assignment
                | CorrectionTable::Suggestion
                | CorrectionTable::TrustedMember
                | CorrectionTable::TrustedSearch
        ),
        _ => false,
    };

    let mut assignments = 0usize;
    let mut constraints = 0usize;
    let mut dispositions = 0usize;
    let mut faces = 0usize;
    let mut embeddings = 0usize;
    let mut person_creates = 0usize;
    let mut person_deletes = 0usize;
    let mut person_updates = 0usize;
    let mut look_creates = 0usize;
    for row in &envelope.rows {
        if !allowed(&row.table) {
            return Err(format!(
                "operation {} correction kind {kind} contains an unsupported typed row effect",
                operation.operation_id
            ));
        }
        let effect = row.after.as_ref().or(row.before.as_ref()).ok_or_else(|| {
            format!(
                "operation {} has an empty correction effect",
                operation.operation_id
            )
        })?;
        let bound_face = match row.table {
            CorrectionTable::Face => Some(
                decode_strict_typed_value::<FaceObservation>(effect, "correction Face effect")?
                    .face_id,
            ),
            CorrectionTable::Embedding => {
                Some(decode_correction_embedding(effect, "correction Embedding effect")?.face_id)
            }
            CorrectionTable::Assignment => Some(
                decode_strict_typed_value::<Assignment>(effect, "correction Assignment effect")?
                    .face_id,
            ),
            CorrectionTable::Constraint => Some(
                decode_strict_typed_value::<CannotLinkConstraint>(
                    effect,
                    "correction Constraint effect",
                )?
                .face_id,
            ),
            CorrectionTable::TrustedMember => Some(
                decode_strict_typed_value::<TrustedTemplateMembership>(
                    effect,
                    "correction trusted-member effect",
                )?
                .face_id,
            ),
            CorrectionTable::TrustedSearch => Some(
                decode_correction_trusted_search(effect, "correction trusted-search effect")?
                    .face_id,
            ),
            CorrectionTable::Disposition => Some(
                decode_strict_typed_value::<FaceDisposition>(effect, "correction disposition")?
                    .face_id,
            ),
            CorrectionTable::Suggestion => Some(
                decode_strict_typed_value::<Suggestion>(effect, "correction Suggestion effect")?
                    .face_id,
            ),
            _ => None,
        };
        if let Some(face_id) = bound_face {
            if !face_set.contains(face_id.as_str()) {
                return Err(format!(
                    "operation {} correction kind {kind} contains an effect outside its Face set",
                    operation.operation_id
                ));
            }
            effect_face_ids.insert(face_id);
        }
        let bound_media = match row.table {
            CorrectionTable::Face => Some(
                decode_strict_typed_value::<FaceObservation>(effect, "correction Face effect")?
                    .media_key,
            ),
            CorrectionTable::Assignment => Some(
                decode_strict_typed_value::<Assignment>(effect, "correction Assignment effect")?
                    .media_key,
            ),
            CorrectionTable::Disposition => Some(
                decode_strict_typed_value::<FaceDisposition>(effect, "correction disposition")?
                    .media_key,
            ),
            _ => None,
        };
        if let Some(media_key) = bound_media {
            if !media_set.contains(media_key.as_str()) {
                return Err(format!(
                    "operation {} correction kind {kind} contains an effect outside its media set",
                    operation.operation_id
                ));
            }
        }

        match row.table {
            CorrectionTable::Assignment => {
                assignments += 1;
                let before = row
                    .before
                    .as_ref()
                    .map(|value| {
                        decode_strict_typed_value::<Assignment>(
                            value,
                            "correction prior Assignment",
                        )
                    })
                    .transpose()?;
                let after = row
                    .after
                    .as_ref()
                    .map(|value| {
                        decode_strict_typed_value::<Assignment>(
                            value,
                            "correction resulting Assignment",
                        )
                    })
                    .transpose()?;
                if before.as_ref().is_some_and(|value| {
                    value.assignment_id != row.stable_id || value.face_id != row.stable_id
                }) || after.as_ref().is_some_and(|value| {
                    value.assignment_id != row.stable_id
                        || value.face_id != row.stable_id
                        || value.operation_id != operation.operation_id
                }) {
                    return Err(format!(
                        "operation {} correction kind {kind} has invalid Assignment ownership",
                        operation.operation_id
                    ));
                }
                let confirmation_provenance = match kind {
                    "change_person" => Some("change_person"),
                    "batch_change_person" => Some("batch_change_person"),
                    "split_to_person" => Some("split_to_person"),
                    "manual_face_and_assign" => Some("manual_face"),
                    _ => None,
                };
                let merge_derived_invalidation = kind == "merge_people"
                    && after.is_none()
                    && before
                        .as_ref()
                        .is_some_and(|assignment| assignment.state == "committed_strict_automatic");
                if let Some(provenance) = confirmation_provenance {
                    let after = after.as_ref().ok_or_else(|| {
                        format!(
                            "operation {} correction kind {kind} omits its resulting Assignment",
                            operation.operation_id
                        )
                    })?;
                    if after.state != "operator_confirmed"
                        || !after.locked
                        || after.provenance != provenance
                        || operation.person_id.as_deref() != Some(after.person_id.as_str())
                    {
                        return Err(format!(
                            "operation {} correction kind {kind} has invalid confirmed Assignment semantics",
                            operation.operation_id
                        ));
                    }
                    if matches!(kind, "change_person" | "batch_change_person")
                        && (after.look_id.is_some() || after.placement != "unsorted")
                    {
                        return Err(format!(
                            "operation {} correction kind {kind} must place the resulting Assignment in Unsorted",
                            operation.operation_id
                        ));
                    }
                    if matches!(kind, "change_person" | "batch_change_person")
                        && before.as_ref().is_some_and(|prior| {
                            after.face_id != prior.face_id
                                || after.media_key != prior.media_key
                                || after.face_revision != prior.face_revision
                                || after.created_at != prior.created_at
                        })
                    {
                        return Err(format!(
                            "operation {} correction kind {kind} changes Assignment source identity or media provenance",
                            operation.operation_id
                        ));
                    }
                }
                let expected_after_person = matches!(
                    kind,
                    "change_person"
                        | "batch_change_person"
                        | "move_to_look"
                        | "same_person_new_look"
                        | "manual_face_and_assign"
                        | "merge_people"
                        | "split_person"
                        | "split_to_person"
                );
                if expected_after_person
                    && !merge_derived_invalidation
                    && after.as_ref().is_none_or(|assignment| {
                        operation.person_id.as_deref() != Some(assignment.person_id.as_str())
                    })
                {
                    return Err(format!(
                        "operation {} correction kind {kind} does not bind its resulting Assignment Person",
                        operation.operation_id
                    ));
                }
                if matches!(kind, "remove_assignment" | "remove_person")
                    && before.as_ref().is_none_or(|assignment| {
                        operation.person_id.as_deref() != Some(assignment.person_id.as_str())
                    })
                {
                    return Err(format!(
                        "operation {} correction kind {kind} does not bind its prior Assignment Person",
                        operation.operation_id
                    ));
                }
                if matches!(
                    kind,
                    "move_to_look" | "same_person_new_look" | "merge_people" | "split_person"
                ) && !merge_derived_invalidation
                {
                    let (Some(before), Some(after)) = (before.as_ref(), after.as_ref()) else {
                        return Err(format!(
                            "operation {} correction kind {kind} must preserve an existing Assignment",
                            operation.operation_id
                        ));
                    };
                    if !same_assignment_evidence(before, after) {
                        return Err(format!(
                            "operation {} correction kind {kind} changes Assignment evidence provenance",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::Constraint => {
                constraints += 1;
                if matches!(kind, "change_person" | "batch_change_person") {
                    let constraint: CannotLinkConstraint = decode_strict_typed_value(
                        row.after.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} Change-person deletes its source cannot-link",
                                operation.operation_id
                            )
                        })?,
                        "Change-person resulting Constraint",
                    )?;
                    let assignment_row = envelope
                        .rows
                        .iter()
                        .find(|candidate| {
                            candidate.table == CorrectionTable::Assignment
                                && candidate.stable_id == constraint.face_id
                        })
                        .ok_or_else(|| {
                            format!(
                                "operation {} Change-person Constraint lacks its Assignment effect",
                                operation.operation_id
                            )
                        })?;
                    let resulting: Assignment = decode_strict_typed_value(
                        assignment_row.after.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} Change-person Constraint lacks a resulting Assignment",
                                operation.operation_id
                            )
                        })?,
                        "Change-person resulting Assignment",
                    )?;
                    let source_person_id = if let Some(prior) = &assignment_row.before {
                        let prior: Assignment =
                            decode_strict_typed_value(prior, "Change-person source Assignment")?;
                        if prior.face_id != constraint.face_id {
                            return Err(format!(
                                "operation {} Change-person source Assignment targets another Face",
                                operation.operation_id
                            ));
                        }
                        prior.person_id
                    } else if kind == "change_person" {
                        envelope
                            .rows
                            .iter()
                            .filter(|candidate| {
                                candidate.table == CorrectionTable::Suggestion
                                    && candidate.after.is_none()
                            })
                            .filter_map(|candidate| {
                                candidate.before.as_ref().and_then(|value| {
                                    decode_strict_typed_value::<Suggestion>(
                                        value,
                                        "Change-person source Suggestion",
                                    )
                                    .ok()
                                })
                            })
                            .find(|suggestion| {
                                suggestion.face_id == constraint.face_id
                                    && suggestion.candidate_person_id == constraint.person_id
                            })
                            .map(|suggestion| suggestion.candidate_person_id)
                            .ok_or_else(|| {
                                format!(
                                    "operation {} Change-person Constraint lacks a source Assignment or deleted Suggestion",
                                    operation.operation_id
                                )
                            })?
                    } else {
                        return Err(format!(
                            "operation {} batch Change-person requires a prior Assignment for every Face",
                            operation.operation_id
                        ));
                    };
                    if constraint.constraint_id != row.stable_id
                        || constraint.constraint_id
                            != cannot_link_id(&constraint.face_id, &constraint.person_id)
                        || constraint.person_id != source_person_id
                        || constraint.person_id == resulting.person_id
                        || operation.person_id.as_deref() != Some(resulting.person_id.as_str())
                        || constraint.operation_id != operation.operation_id
                        || !constraint.operator_owned
                    {
                        return Err(format!(
                            "operation {} Change-person Constraint does not bind the rejected source Person",
                            operation.operation_id
                        ));
                    }
                } else if kind == "merge_people" {
                    if let Some(after) = &row.after {
                        let constraint: CannotLinkConstraint =
                            decode_strict_typed_value(after, "merge resulting Constraint")?;
                        if operation.person_id.as_deref() != Some(constraint.person_id.as_str())
                            || constraint.operation_id != operation.operation_id
                        {
                            return Err(format!(
                                "operation {} merge Constraint does not bind the destination Person",
                                operation.operation_id
                            ));
                        }
                    }
                } else if kind == "remove_person" {
                    let constraint: CannotLinkConstraint = decode_strict_typed_value(
                        row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} remove-Person Constraint omits its deleted value",
                                operation.operation_id
                            )
                        })?,
                        "remove-Person deleted Constraint",
                    )?;
                    if operation.person_id.as_deref() != Some(constraint.person_id.as_str()) {
                        return Err(format!(
                            "operation {} remove-Person Constraint targets another Person",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::Disposition => {
                dispositions += 1;
                let after = row.after.as_ref().ok_or_else(|| {
                    format!(
                        "operation {} correction kind {kind} deletes its disposition",
                        operation.operation_id
                    )
                })?;
                let disposition: FaceDisposition =
                    decode_strict_typed_value(after, "correction resulting disposition")?;
                let expected = if matches!(kind, "ignored" | "batch_ignore_face") {
                    "ignored"
                } else {
                    "not_a_face"
                };
                if disposition.face_id != row.stable_id
                    || disposition.operation_id != operation.operation_id
                    || disposition.disposition != expected
                {
                    return Err(format!(
                        "operation {} correction kind {kind} has invalid disposition ownership",
                        operation.operation_id
                    ));
                }
            }
            CorrectionTable::Face => {
                faces += 1;
                if matches!(kind, "manual_face" | "manual_face_and_assign") {
                    let face: FaceObservation = decode_strict_typed_value(
                        row.after.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} manual correction deletes its Face",
                                operation.operation_id
                            )
                        })?,
                        "manual correction Face",
                    )?;
                    if row.before.is_some() || face.face_id != row.stable_id || !face.operator_owned
                    {
                        return Err(format!(
                            "operation {} manual correction has invalid Face creation semantics",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::Embedding => {
                embeddings += 1;
                if matches!(kind, "manual_face" | "manual_face_and_assign") {
                    let embedding = decode_correction_embedding(
                        row.after.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} manual correction deletes its Embedding",
                                operation.operation_id
                            )
                        })?,
                        "manual correction Embedding",
                    )?;
                    if row.before.is_some()
                        || embedding.embedding_id != row.stable_id
                        || embedding.job_id != operation.operation_id
                    {
                        return Err(format!(
                            "operation {} manual correction has invalid Embedding creation semantics",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::Person => match (&row.before, &row.after) {
                (None, Some(_)) => person_creates += 1,
                (Some(_), None) => person_deletes += 1,
                (Some(_), Some(_)) => person_updates += 1,
                _ => {}
            },
            CorrectionTable::Look => {
                if row.before.is_none() && row.after.is_some() {
                    look_creates += 1;
                }
                let before = row
                    .before
                    .as_ref()
                    .map(|value| decode_strict_typed_value::<Look>(value, "prior Look effect"))
                    .transpose()?;
                let after = row
                    .after
                    .as_ref()
                    .map(|value| decode_strict_typed_value::<Look>(value, "resulting Look effect"))
                    .transpose()?;
                if kind == "same_person_new_look"
                    && (before.is_some()
                        || after.as_ref().is_none_or(|look| {
                            operation.person_id.as_deref() != Some(look.person_id.as_str())
                        }))
                {
                    return Err(format!(
                        "operation {} Same-person-new-Look effect does not bind its Person",
                        operation.operation_id
                    ));
                }
                if kind == "remove_person"
                    && before.as_ref().is_none_or(|look| {
                        operation.person_id.as_deref() != Some(look.person_id.as_str())
                    })
                {
                    return Err(format!(
                        "operation {} remove-Person Look effect targets another Person",
                        operation.operation_id
                    ));
                }
                if kind == "merge_people"
                    && after.as_ref().is_none_or(|look| {
                        operation.person_id.as_deref() != Some(look.person_id.as_str())
                    })
                {
                    return Err(format!(
                        "operation {} merge Look effect does not target the destination Person",
                        operation.operation_id
                    ));
                }
            }
            CorrectionTable::TemplateSet => {
                if kind == "remove_person" {
                    let set: TrustedTemplateSet = decode_strict_typed_value(
                        row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} remove-Person TemplateSet omits its deleted value",
                                operation.operation_id
                            )
                        })?,
                        "remove-Person deleted TemplateSet",
                    )?;
                    let look = removed_looks.get(&set.look_id).ok_or_else(|| {
                        format!(
                            "operation {} remove-Person TemplateSet lacks its deleted Look closure",
                            operation.operation_id
                        )
                    })?;
                    if set.set_id != row.stable_id
                        || look.look_id != set.look_id
                        || operation.person_id.as_deref() != Some(look.person_id.as_str())
                    {
                        return Err(format!(
                            "operation {} remove-Person TemplateSet has unrelated ownership",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::TrustedMember => {
                if kind == "remove_person" {
                    let member: TrustedTemplateMembership = decode_strict_typed_value(
                        row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} remove-Person TrustedMember omits its deleted value",
                                operation.operation_id
                            )
                        })?,
                        "remove-Person deleted TrustedMember",
                    )?;
                    let set = removed_template_sets.get(&member.set_id).ok_or_else(|| {
                        format!(
                            "operation {} remove-Person TrustedMember lacks its deleted TemplateSet closure",
                            operation.operation_id
                        )
                    })?;
                    let look = removed_looks.get(&member.look_id).ok_or_else(|| {
                        format!(
                            "operation {} remove-Person TrustedMember lacks its deleted Look closure",
                            operation.operation_id
                        )
                    })?;
                    if member.membership_id != row.stable_id
                        || set.set_id != member.set_id
                        || set.look_id != member.look_id
                        || look.look_id != member.look_id
                        || operation.person_id.as_deref() != Some(look.person_id.as_str())
                    {
                        return Err(format!(
                            "operation {} remove-Person TrustedMember has unrelated ownership",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::TrustedSearch => {
                if kind == "remove_person" {
                    let search = decode_correction_trusted_search(
                        row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} remove-Person TrustedSearch omits its deleted value",
                                operation.operation_id
                            )
                        })?,
                        "remove-Person deleted TrustedSearch",
                    )?;
                    if operation.person_id.as_deref() != Some(search.person_id.as_str()) {
                        return Err(format!(
                            "operation {} remove-Person TrustedSearch targets another Person",
                            operation.operation_id
                        ));
                    }
                } else if kind == "merge_people" {
                    let search = decode_correction_trusted_search(
                        row.after.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} merge deletes trusted-search ownership",
                                operation.operation_id
                            )
                        })?,
                        "merge trusted-search effect",
                    )?;
                    if operation.person_id.as_deref() != Some(search.person_id.as_str()) {
                        return Err(format!(
                            "operation {} merge trusted-search effect targets another Person",
                            operation.operation_id
                        ));
                    }
                }
            }
            CorrectionTable::Suggestion => {
                if kind == "remove_person" {
                    let suggestion: Suggestion = decode_strict_typed_value(
                        row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} remove-Person Suggestion omits its deleted value",
                                operation.operation_id
                            )
                        })?,
                        "remove-Person deleted Suggestion",
                    )?;
                    if operation.person_id.as_deref()
                        != Some(suggestion.candidate_person_id.as_str())
                    {
                        return Err(format!(
                            "operation {} remove-Person Suggestion targets another Person",
                            operation.operation_id
                        ));
                    }
                }
            }
            _ => {}
        }

        if matches!(
            kind,
            "remove_assignment"
                | "batch_remove_assignments"
                | "remove_person"
                | "delete_face_analysis"
                | "batch_delete_face_analysis"
        ) && row.after.is_some()
        {
            return Err(format!(
                "operation {} correction kind {kind} must contain deletion-only effects",
                operation.operation_id
            ));
        }
        if (row.table == CorrectionTable::Suggestion
            || row.table == CorrectionTable::TrustedMember
            || (row.table == CorrectionTable::TrustedSearch && kind != "merge_people"))
            && row.after.is_some()
        {
            return Err(format!(
                "operation {} correction kind {kind} has a non-deletion auxiliary effect",
                operation.operation_id
            ));
        }
    }

    if effect_face_ids != envelope.face_ids.iter().cloned().collect::<BTreeSet<_>>() {
        return Err(format!(
            "operation {} correction kind {kind} declared Face set does not exactly match its typed effects",
            operation.operation_id
        ));
    }

    let require_assignments = matches!(
        kind,
        "change_person"
            | "batch_change_person"
            | "move_to_look"
            | "same_person_new_look"
            | "remove_assignment"
            | "batch_remove_assignments"
            | "split_to_person"
            | "split_person"
    );
    let valid_principal = if require_assignments {
        assignments > 0
    } else {
        match kind {
            "ignored" | "not_a_face" | "batch_ignore_face" | "batch_not_a_face" => dispositions > 0,
            "delete_face_analysis" | "batch_delete_face_analysis" => faces > 0,
            "manual_face" => faces == 1 && embeddings <= 1,
            "manual_face_and_assign" => faces == 1 && embeddings <= 1 && assignments == 1,
            "remove_person" => person_deletes == 1,
            "merge_people" => person_deletes == 1 && person_updates == 1,
            _ => true,
        }
    };
    if !valid_principal {
        return Err(format!(
            "operation {} correction kind {kind} lacks its required principal typed effect",
            operation.operation_id
        ));
    }
    if constraints > 0
        && !matches!(
            kind,
            "change_person"
                | "batch_change_person"
                | "remove_person"
                | "merge_people"
                | "delete_face_analysis"
                | "batch_delete_face_analysis"
        )
    {
        return Err(format!(
            "operation {} correction kind {kind} contains an unauthorized constraint effect",
            operation.operation_id
        ));
    }
    if matches!(kind, "change_person" | "batch_change_person") && constraints != assignments {
        return Err(format!(
            "operation {} correction kind {kind} must bind one source cannot-link per changed Assignment",
            operation.operation_id
        ));
    }
    if person_creates > 0 && kind != "split_person" {
        return Err(format!(
            "operation {} correction kind {kind} contains an unauthorized Person creation",
            operation.operation_id
        ));
    }
    if kind == "same_person_new_look" && look_creates != 1 {
        return Err(format!(
            "operation {} Same-person-new-Look correction must create exactly one Look",
            operation.operation_id
        ));
    }
    if matches!(kind, "remove_person" | "merge_people" | "split_person") {
        let bound_person = envelope.rows.iter().find_map(|row| {
            if row.table != CorrectionTable::Person {
                return None;
            }
            let select = match kind {
                "remove_person" => row.before.as_ref().filter(|_| row.after.is_none()),
                "merge_people" => row.after.as_ref().filter(|_| row.before.is_some()),
                "split_person" => row.after.as_ref().filter(|_| row.before.is_none()),
                _ => None,
            }?;
            decode_strict_typed_value::<Person>(select, "correction bound Person")
                .ok()
                .map(|person| person.person_id)
        });
        if operation.person_id != bound_person {
            return Err(format!(
                "operation {} correction kind {kind} does not bind its principal Person",
                operation.operation_id
            ));
        }
    }
    Ok(())
}

fn require_exact_live_delta<T>(
    operation: &MatchOperation,
    indexed: &IndexedCorrectionEnvelope,
    table: CorrectionTable,
    stable_id: &str,
    current: &T,
) -> Result<(), String>
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq,
{
    let delta = indexed.row(&table, stable_id).ok_or_else(|| {
        format!(
            "operation {} omits its live typed row effect {}",
            operation.operation_id, stable_id
        )
    })?;
    let recorded_value = delta.after.as_ref().ok_or_else(|| {
        format!(
            "operation {} deletes the live typed row effect {} it claims to own",
            operation.operation_id, stable_id
        )
    })?;
    let recorded: T = decode_strict_typed_value(
        recorded_value,
        &format!(
            "operation {} live typed row effect {}",
            operation.operation_id, stable_id
        ),
    )?;
    if &recorded != current {
        return Err(format!(
            "operation {} contradicts its live typed row effect {}",
            operation.operation_id, stable_id
        ));
    }
    Ok(())
}

fn validate_correction_envelope(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    expected_kind: &str,
) -> Result<(), String> {
    if envelope.version != 1 || envelope.kind != expected_kind || envelope.rows.len() > 4096 {
        return Err(format!(
            "operation {} has an unsupported, mismatched, or unbounded correction envelope",
            operation.operation_id
        ));
    }
    validate_sorted_unique_texts(
        &envelope.face_ids,
        "correction FaceId",
        &operation.operation_id,
    )?;
    validate_sorted_unique_media_keys(&envelope.media_keys, &operation.operation_id)?;
    let mut row_ids = BTreeSet::new();
    for row in &envelope.rows {
        validate_text("correction row stable ID", &row.stable_id)?;
        let table = serde_json::to_string(&row.table).map_err(|error| error.to_string())?;
        if !row_ids.insert((table, row.stable_id.clone())) {
            return Err(format!(
                "operation {} repeats a correction row effect",
                operation.operation_id
            ));
        }
        if let Some(before) = &row.before {
            validate_correction_row_value(&row.table, &row.stable_id, before)?;
        }
        if let Some(after) = &row.after {
            validate_correction_row_value(&row.table, &row.stable_id, after)?;
        }
        let semantic_no_op = match (&row.before, &row.after) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                correction_row_values_semantically_equal(&row.table, before, after)?
            }
            _ => false,
        };
        if semantic_no_op {
            return Err(format!(
                "operation {} contains a no-op correction row",
                operation.operation_id
            ));
        }
    }
    Ok(())
}

fn validate_sorted_unique_texts(
    values: &[String],
    label: &str,
    operation_id: &str,
) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for value in values {
        validate_text(label, value)?;
        if previous.is_some_and(|previous| previous >= value.as_str()) {
            return Err(format!(
                "operation {operation_id} {label} values are not canonical sorted unique"
            ));
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_sorted_unique_media_keys(values: &[String], operation_id: &str) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for value in values {
        validate_exchange_media_key(value)?;
        if previous.is_some_and(|previous| previous >= value.as_str()) {
            return Err(format!(
                "operation {operation_id} media keys are not canonical sorted unique"
            ));
        }
        previous = Some(value);
    }
    Ok(())
}

fn validate_same_correction_effect(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    kind: &str,
) -> Result<(), String> {
    if !envelope.identity_changed || envelope.catalog_changed {
        return Err(format!(
            "operation {} Same correction has invalid revision effects",
            operation.operation_id
        ));
    }
    let expected_provenance = if kind == "same" { "same" } else { "batch_same" };
    let mut assignments = 0usize;
    let mut effect_face_ids = BTreeSet::new();
    let mut effect_person_ids = BTreeSet::new();
    for row in &envelope.rows {
        if row.table == CorrectionTable::Assignment {
            let after = row.after.as_ref().ok_or_else(|| {
                format!(
                    "operation {} Same correction deletes its Assignment",
                    operation.operation_id
                )
            })?;
            let assignment: Assignment = decode_strict_typed_value(
                after,
                &format!(
                    "operation {} Same Assignment effect",
                    operation.operation_id
                ),
            )?;
            if assignment.state != "operator_confirmed"
                || !assignment.locked
                || assignment.provenance != expected_provenance
                || assignment.operation_id != operation.operation_id
                || assignment.look_id.is_some()
                || assignment.placement != "unsorted"
            {
                return Err(format!(
                    "operation {} Same correction does not create confirmed evidence",
                    operation.operation_id
                ));
            }
            match row.before.as_ref() {
                Some(before) => {
                    let source: Assignment = decode_strict_typed_value(
                        before,
                        &format!(
                            "operation {} Same source Assignment",
                            operation.operation_id
                        ),
                    )?;
                    if source.state != "committed_strict_automatic"
                        || source.face_id != assignment.face_id
                        || source.person_id != assignment.person_id
                        || source.media_key != assignment.media_key
                        || source.face_revision != assignment.face_revision
                        || source.person_revision != assignment.person_revision
                        || source.created_at != assignment.created_at
                    {
                        return Err(format!(
                            "operation {} Same correction lacks producer-equivalent strict source evidence",
                            operation.operation_id
                        ));
                    }
                }
                None => {
                    let suggestions = matching_deleted_suggestions(
                        operation,
                        envelope,
                        &assignment.face_id,
                        &assignment.person_id,
                    )?;
                    if suggestions.len() != 1
                        || suggestions[0].face_revision != assignment.face_revision
                        || suggestions[0].person_revision != assignment.person_revision
                    {
                        return Err(format!(
                            "operation {} Same correction lacks producer-equivalent suggestion source evidence",
                            operation.operation_id
                        ));
                    }
                }
            }
            effect_face_ids.insert(assignment.face_id.clone());
            effect_person_ids.insert(assignment.person_id.clone());
            assignments += 1;
        }
    }
    if assignments == 0 {
        return Err(format!(
            "operation {} Same correction has no Assignment effect",
            operation.operation_id
        ));
    }
    let envelope_face_ids = envelope.face_ids.iter().cloned().collect::<BTreeSet<_>>();
    let batch = kind == "batch_same";
    if effect_face_ids != envelope_face_ids
        || effect_person_ids.len() != 1
        || operation.person_id.as_deref() != effect_person_ids.first().map(String::as_str)
        || (batch && operation.face_id.is_some())
        || (!batch
            && (assignments != 1
                || operation.face_id.as_deref() != effect_face_ids.first().map(String::as_str)))
    {
        return Err(format!(
            "operation {} Same correction identity envelope contradicts its Assignment effects",
            operation.operation_id
        ));
    }
    for row in &envelope.rows {
        match row.table {
            CorrectionTable::Assignment => {}
            CorrectionTable::Suggestion
            | CorrectionTable::TrustedMember
            | CorrectionTable::TrustedSearch => {
                validate_face_bound_auxiliary_deletion(operation, row, &effect_face_ids)?;
            }
            _ => {
                return Err(format!(
                    "operation {} Same correction contains an unsupported typed row effect",
                    operation.operation_id
                ));
            }
        }
    }
    Ok(())
}

fn matching_deleted_suggestions(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    face_id: &str,
    person_id: &str,
) -> Result<Vec<Suggestion>, String> {
    envelope
        .rows
        .iter()
        .filter(|row| row.table == CorrectionTable::Suggestion && row.after.is_none())
        .filter_map(|row| row.before.as_ref().map(|before| (row, before)))
        .map(|(row, before)| {
            decode_strict_typed_value::<Suggestion>(
                before,
                &format!(
                    "operation {} candidate Suggestion deletion",
                    operation.operation_id
                ),
            )
            .map(|suggestion| (row, suggestion))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|(row, suggestion)| {
                    (row.stable_id == suggestion.suggestion_id
                        && suggestion.face_id == face_id
                        && suggestion.candidate_person_id == person_id)
                        .then_some(suggestion)
                })
                .collect()
        })
}

fn validate_historical_suggestion_source_provenance(
    operation: &MatchOperation,
    suggestion: &Suggestion,
    provenance: &SuggestionSourceProvenance,
    expected_face_revision: Option<u64>,
    expected_person_revision: Option<u64>,
    expected_media_fingerprint: Option<&str>,
    generation_by_id: &BTreeMap<&str, &ModelGeneration>,
) -> Result<(), String> {
    require_media_sha256(
        "historical correction Suggestion media fingerprint",
        &suggestion.media_fingerprint,
    )?;
    validate_text("historical correction Suggestion JobId", &suggestion.job_id)?;
    validate_text(
        "historical correction Suggestion model generation",
        &suggestion.model_generation,
    )?;
    let suggestion_created_at = parse_bounded_rfc3339_instant(
        "historical correction Suggestion created_at is not RFC3339",
        &suggestion.created_at,
    )?;
    let generation = generation_by_id
        .get(suggestion.model_generation.as_str())
        .copied()
        .ok_or_else(|| {
            format!(
                "operation {} historical Suggestion model generation is absent",
                operation.operation_id
            )
        })?;
    let calibration_valid = match (
        suggestion.calibration_generation.as_deref(),
        suggestion.envelope_hash.as_deref(),
    ) {
        (None, None) => true,
        (Some(calibration), Some(envelope)) => {
            validate_text("historical Suggestion calibration generation", calibration)?;
            validate_sha256("historical Suggestion envelope hash", envelope)?;
            true
        }
        _ => false,
    };
    if !suggestion.similarity.is_finite()
        || suggestion.suggestion_id
            != suggestion_id(&suggestion.face_id, &suggestion.candidate_person_id)
        || suggestion.face_revision == 0
        || suggestion.person_revision == 0
        || expected_face_revision.is_some_and(|revision| suggestion.face_revision != revision)
        || expected_person_revision.is_some_and(|revision| suggestion.person_revision != revision)
        || expected_media_fingerprint
            .is_some_and(|fingerprint| suggestion.media_fingerprint.as_str() != fingerprint)
        || !generation.validated
        || !matches!(generation.state.as_str(), "usable" | "active")
        || !calibration_valid
    {
        return Err(format!(
            "operation {} historical Suggestion source provenance is forged or incomplete",
            operation.operation_id
        ));
    }
    validate_exchange_media_key(&provenance.media_key)?;
    let embedding_created_at = parse_bounded_rfc3339_instant(
        "SuggestionSourceProvenance embedding creation time is not RFC3339",
        &provenance.embedding_created_at,
    )?;
    let operation_created_at = parse_operation_time(operation)?;
    if provenance.provenance_id != suggestion_source_provenance_id(provenance)
        || provenance.operation_id != operation.operation_id
        || provenance.operation_kind != operation.kind.trim_start_matches("correction_")
        || provenance.operation_created_at != operation.created_at
        || provenance.suggestion_id != suggestion.suggestion_id
        || provenance.face_id != suggestion.face_id
        || provenance.candidate_person_id != suggestion.candidate_person_id
        || provenance.face_revision != suggestion.face_revision
        || provenance.person_revision != suggestion.person_revision
        || provenance.media_fingerprint != suggestion.media_fingerprint
        || provenance.model_generation != suggestion.model_generation
        || provenance.schema_generation != MATCH_SCHEMA_GENERATION
        || provenance.embedding_id
            != embedding_id(&suggestion.face_id, &suggestion.model_generation)
        || provenance.job_id != suggestion.job_id
        || provenance.suggestion_created_at != suggestion.created_at
        || !f32::from_bits(provenance.similarity_bits).is_finite()
        || provenance.similarity_bits != suggestion.similarity.to_bits()
        || provenance.calibration_generation != suggestion.calibration_generation
        || provenance.envelope_hash != suggestion.envelope_hash
        || embedding_created_at > suggestion_created_at
        || suggestion_created_at > operation_created_at
    {
        return Err(format!(
            "operation {} Suggestion source contradicts its durable provenance entity",
            operation.operation_id
        ));
    }
    Ok(())
}

fn validate_historical_strict_assignment_source_provenance(
    operation: &MatchOperation,
    assignment: &Assignment,
    generation_by_id: &BTreeMap<&str, &ModelGeneration>,
) -> Result<(), String> {
    let generation_id = assignment.model_generation.as_deref().ok_or_else(|| {
        format!(
            "operation {} strict Assignment source omits its model generation",
            operation.operation_id
        )
    })?;
    validate_text(
        "historical strict Assignment model generation",
        generation_id,
    )?;
    validate_strict_assignment_chronology(operation, assignment)?;
    let calibration = assignment
        .calibration_generation
        .as_deref()
        .ok_or_else(|| {
            format!(
                "operation {} strict Assignment source omits calibration provenance",
                operation.operation_id
            )
        })?;
    let envelope = assignment.envelope_hash.as_deref().ok_or_else(|| {
        format!(
            "operation {} strict Assignment source omits envelope provenance",
            operation.operation_id
        )
    })?;
    validate_text("historical strict Assignment calibration", calibration)?;
    validate_sha256("historical strict Assignment envelope", envelope)?;
    let generation = generation_by_id
        .get(generation_id)
        .copied()
        .ok_or_else(|| {
            format!(
                "operation {} strict Assignment source model generation is absent",
                operation.operation_id
            )
        })?;
    if assignment.state != "committed_strict_automatic"
        || assignment.face_revision == 0
        || assignment.person_revision == 0
        || !generation.validated
        || !matches!(generation.state.as_str(), "usable" | "active")
    {
        return Err(format!(
            "operation {} strict Assignment source provenance is forged or incomplete",
            operation.operation_id
        ));
    }
    Ok(())
}

fn parse_rfc3339_instant(
    label: &str,
    value: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    parse_bounded_rfc3339_instant(&format!("{label} is not RFC3339"), value)
}

fn validate_strict_assignment_chronology(
    operation: &MatchOperation,
    assignment: &Assignment,
) -> Result<(), String> {
    let created = parse_rfc3339_instant(
        "historical strict Assignment created_at",
        &assignment.created_at,
    )?;
    let updated = parse_rfc3339_instant(
        "historical strict Assignment updated_at",
        &assignment.updated_at,
    )?;
    let producer = parse_operation_time(operation)?;
    if created > updated || updated > producer {
        return Err(format!(
            "operation {} strict Assignment chronology is invalid or in the future",
            operation.operation_id
        ));
    }
    Ok(())
}

fn exact_suggestion_source_provenance<'a>(
    operation: &MatchOperation,
    suggestion: &Suggestion,
    rows: &'a [&SuggestionSourceProvenance],
) -> Result<&'a SuggestionSourceProvenance, String> {
    let matching = rows
        .iter()
        .copied()
        .filter(|row| row.suggestion_id == suggestion.suggestion_id)
        .collect::<Vec<_>>();
    matching
        .first()
        .copied()
        .filter(|_| matching.len() == 1)
        .ok_or_else(|| {
            format!(
                "operation {} SuggestionSourceProvenance is absent or ambiguous for {}",
                operation.operation_id, suggestion.suggestion_id
            )
        })
}

fn validate_suggestion_source_anchor(
    operation: &MatchOperation,
    provenance: &SuggestionSourceProvenance,
    face_by_id: &BTreeMap<&str, &FaceObservation>,
    person_by_id: &BTreeMap<&str, &Person>,
    correction_by_operation: &BTreeMap<&str, IndexedCorrectionEnvelope>,
    operation_by_id: &BTreeMap<&str, &MatchOperation>,
    anchor_work: &mut usize,
) -> Result<(), String> {
    let current_face_matches = face_by_id
        .get(provenance.face_id.as_str())
        .is_some_and(|face| {
            face.face_revision == provenance.face_revision
                && face.media_fingerprint == provenance.media_fingerprint
                && face.media_key == provenance.media_key
                && face.schema_generation == provenance.schema_generation
        });
    let current_person_matches = person_by_id
        .get(provenance.candidate_person_id.as_str())
        .is_some_and(|person| person.revision >= provenance.person_revision);

    // The live Face and monotonic same-ID Person are already independent
    // anchors. Do not charge this provenance for scanning unrelated history;
    // the bounded historical fallback is needed only when either live anchor
    // has disappeared or no longer describes the operation-time Face.
    if current_face_matches && current_person_matches {
        return Ok(());
    }

    let source_time =
        chrono::DateTime::parse_from_rfc3339(&operation.created_at).map_err(|error| {
            format!(
                "operation {} has invalid creation time: {error}",
                operation.operation_id
            )
        })?;
    let mut face_embedding_anchors = Vec::new();
    let mut later_person_anchor = false;
    for (later_id, indexed) in correction_by_operation {
        *anchor_work = anchor_work
            .checked_add(1)
            .ok_or_else(|| "SuggestionSourceProvenance anchor work overflow".to_string())?;
        enforce_limit(
            "SuggestionSourceProvenance anchor work",
            *anchor_work,
            IDENTITY_BUNDLE_MAX_REFERENCES,
        )?;
        if *later_id == operation.operation_id {
            continue;
        }
        let Some(later) = operation_by_id.get(later_id).copied() else {
            continue;
        };
        let Ok(later_time) = chrono::DateTime::parse_from_rfc3339(&later.created_at) else {
            continue;
        };
        // Equal millisecond timestamps are allowed only with the exact typed
        // before-snapshot below; the distinct deletion operation supplies the
        // causal edge when wall-clock precision cannot order the commits.
        if later_time < source_time {
            continue;
        }
        let row_work = indexed
            .envelope
            .rows
            .len()
            .checked_mul(3)
            .ok_or_else(|| "SuggestionSourceProvenance row work overflow".to_string())?;
        *anchor_work = anchor_work
            .checked_add(row_work)
            .ok_or_else(|| "SuggestionSourceProvenance anchor work overflow".to_string())?;
        enforce_limit(
            "SuggestionSourceProvenance anchor work",
            *anchor_work,
            IDENTITY_BUNDLE_MAX_REFERENCES,
        )?;
        let face = indexed
            .envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Face && row.stable_id == provenance.face_id)
            .filter_map(|row| row.before.as_ref())
            .filter_map(|value| {
                decode_strict_typed_value::<FaceObservation>(
                    value,
                    "historical provenance Face anchor",
                )
                .ok()
            })
            .find(|face| {
                face.face_revision == provenance.face_revision
                    && face.media_fingerprint == provenance.media_fingerprint
                    && face.media_key == provenance.media_key
                    && face.schema_generation == provenance.schema_generation
            });
        let embedding = indexed
            .envelope
            .rows
            .iter()
            .filter(|row| {
                row.table == CorrectionTable::Embedding && row.stable_id == provenance.embedding_id
            })
            .filter_map(|row| row.before.as_ref())
            .filter_map(|value| {
                decode_correction_embedding(value, "historical provenance Embedding anchor").ok()
            })
            .find(|embedding| {
                embedding.embedding_id == provenance.embedding_id
                    && embedding.face_id == provenance.face_id
                    && embedding.face_revision == provenance.face_revision
                    && embedding.media_fingerprint == provenance.media_fingerprint
                    && embedding.model_generation == provenance.model_generation
                    && embedding.schema_generation == provenance.schema_generation
                    && embedding.job_id == provenance.job_id
                    && embedding.created_at == provenance.embedding_created_at
                    && embedding.active
            });
        if matches!(
            later.kind.as_str(),
            "correction_delete_face_analysis" | "correction_batch_delete_face_analysis"
        ) && face.is_some()
            && embedding.is_some()
        {
            face_embedding_anchors.push(later.operation_id.as_str());
        }
        later_person_anchor |= indexed
            .envelope
            .rows
            .iter()
            .filter(|row| {
                row.table == CorrectionTable::Person
                    && row.stable_id == provenance.candidate_person_id
            })
            .filter_map(|row| row.before.as_ref())
            .filter_map(|value| {
                decode_strict_typed_value::<Person>(value, "historical provenance Person anchor")
                    .ok()
            })
            .any(|person| person.revision >= provenance.person_revision);
    }
    if !current_face_matches && face_embedding_anchors.len() != 1 {
        return Err(format!(
            "operation {} SuggestionSourceProvenance lacks a unique current or later Face/Embedding anchor",
            operation.operation_id
        ));
    }
    if !current_person_matches && !later_person_anchor {
        return Err(format!(
            "operation {} SuggestionSourceProvenance lacks a monotonic same-ID current or later Person anchor",
            operation.operation_id
        ));
    }
    Ok(())
}

fn validate_portable_correction_media_set(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    face_by_id: &BTreeMap<&str, &FaceObservation>,
    face_media_evidence: &BTreeMap<String, (String, String)>,
    provenance_rows: &[&SuggestionSourceProvenance],
) -> Result<(), String> {
    let declared = envelope
        .media_keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut anchored = BTreeSet::new();
    for face_id in &envelope.face_ids {
        if let Some(face) = face_by_id.get(face_id.as_str()) {
            anchored.insert(face.media_key.as_str());
        } else if let Some((media_key, _)) = face_media_evidence.get(face_id.as_str()) {
            anchored.insert(media_key.as_str());
        }
    }
    for row in &envelope.rows {
        if !matches!(
            row.table,
            CorrectionTable::Face | CorrectionTable::Assignment | CorrectionTable::Disposition
        ) {
            continue;
        }
        for value in [row.before.as_ref(), row.after.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(media_key) = value.get("media_key").and_then(Value::as_str) {
                validate_exchange_media_key(media_key)?;
                if matches!(
                    row.table,
                    CorrectionTable::Assignment | CorrectionTable::Disposition
                ) {
                    let face_id =
                        value
                            .get("face_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                format!(
                                    "operation {} historical media claim omits FaceId",
                                    operation.operation_id
                                )
                            })?;
                    if let Some((canonical_media_key, _)) = face_media_evidence.get(face_id) {
                        if canonical_media_key != media_key {
                            return Err(format!(
                                "operation {} historical media claim contradicts Face evidence",
                                operation.operation_id
                            ));
                        }
                    }
                }
                anchored.insert(media_key);
            }
        }
    }
    for provenance in provenance_rows {
        validate_exchange_media_key(&provenance.media_key)?;
        anchored.insert(provenance.media_key.as_str());
    }
    if anchored != declared {
        return Err(format!(
            "operation {} correction media set does not exactly match its canonical current or historical evidence",
            operation.operation_id
        ));
    }
    Ok(())
}

fn insert_portable_face_media_evidence(
    evidence: &mut BTreeMap<String, (String, String)>,
    face_id: &str,
    media_key: &str,
    media_fingerprint: &str,
) -> Result<(), String> {
    validate_text("portable Face media evidence FaceId", face_id)?;
    validate_exchange_media_key(media_key)?;
    require_media_sha256(
        "portable Face media evidence fingerprint",
        media_fingerprint,
    )?;
    if let Some((existing_key, existing_fingerprint)) = evidence.get(face_id) {
        if existing_key != media_key || existing_fingerprint != media_fingerprint {
            return Err(format!(
                "Face {face_id} has conflicting current or historical media evidence"
            ));
        }
        return Ok(());
    }
    evidence.insert(
        face_id.to_string(),
        (media_key.to_string(), media_fingerprint.to_string()),
    );
    Ok(())
}

fn portable_face_media_evidence(
    graph: &IdentityBundleGraph,
) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut evidence = BTreeMap::new();
    for face in &graph.faces {
        insert_portable_face_media_evidence(
            &mut evidence,
            &face.face_id,
            &face.media_key,
            &face.media_fingerprint,
        )?;
    }
    for provenance in &graph.suggestion_source_provenance {
        insert_portable_face_media_evidence(
            &mut evidence,
            &provenance.face_id,
            &provenance.media_key,
            &provenance.media_fingerprint,
        )?;
    }
    for operation in &graph.operations {
        let Some(indexed) = index_correction_envelope(operation)? else {
            continue;
        };
        for row in indexed
            .envelope
            .rows
            .iter()
            .filter(|row| row.table == CorrectionTable::Face)
        {
            for value in [row.before.as_ref(), row.after.as_ref()]
                .into_iter()
                .flatten()
            {
                let face: FaceObservation =
                    decode_strict_typed_value(value, "portable historical Face media evidence")?;
                insert_portable_face_media_evidence(
                    &mut evidence,
                    &face.face_id,
                    &face.media_key,
                    &face.media_fingerprint,
                )?;
            }
        }
    }
    Ok(evidence)
}

fn validate_portable_correction_source_provenance(
    operation: &MatchOperation,
    indexed: &IndexedCorrectionEnvelope,
    face_by_id: &BTreeMap<&str, &FaceObservation>,
    person_by_id: &BTreeMap<&str, &Person>,
    generation_by_id: &BTreeMap<&str, &ModelGeneration>,
    provenance_rows: &[&SuggestionSourceProvenance],
    correction_by_operation: &BTreeMap<&str, IndexedCorrectionEnvelope>,
    operation_by_id: &BTreeMap<&str, &MatchOperation>,
    anchor_work: &mut usize,
) -> Result<(), String> {
    let mut used_provenance = BTreeSet::new();
    match operation.kind.as_str() {
        "correction_same" | "correction_batch_same" => {
            for assignment_row in indexed
                .envelope
                .rows
                .iter()
                .filter(|row| row.table == CorrectionTable::Assignment)
            {
                let resulting: Assignment = decode_strict_typed_value(
                    assignment_row.after.as_ref().ok_or_else(|| {
                        format!(
                            "operation {} Same source provenance lacks its result",
                            operation.operation_id
                        )
                    })?,
                    "portable Same resulting Assignment",
                )?;
                if let Some(before) = assignment_row.before.as_ref() {
                    let source: Assignment = decode_strict_typed_value(
                        before,
                        "portable Same strict Assignment source",
                    )?;
                    validate_historical_strict_assignment_source_provenance(
                        operation,
                        &source,
                        generation_by_id,
                    )?;
                } else {
                    let suggestions = matching_deleted_suggestions(
                        operation,
                        &indexed.envelope,
                        &resulting.face_id,
                        &resulting.person_id,
                    )?;
                    let source = suggestions
                        .first()
                        .filter(|_| suggestions.len() == 1)
                        .ok_or_else(|| {
                            format!(
                                "operation {} Same suggestion source is ambiguous or absent",
                                operation.operation_id
                            )
                        })?;
                    let provenance =
                        exact_suggestion_source_provenance(operation, source, provenance_rows)?;
                    used_provenance.insert(provenance.provenance_id.as_str());
                    validate_historical_suggestion_source_provenance(
                        operation,
                        source,
                        provenance,
                        Some(resulting.face_revision),
                        Some(resulting.person_revision),
                        face_by_id
                            .get(resulting.face_id.as_str())
                            .map(|face| face.media_fingerprint.as_str()),
                        generation_by_id,
                    )?;
                }
            }
        }
        "correction_different" | "correction_batch_different" => {
            for constraint_row in indexed
                .envelope
                .rows
                .iter()
                .filter(|row| row.table == CorrectionTable::Constraint)
            {
                let constraint: CannotLinkConstraint = decode_strict_typed_value(
                    constraint_row.after.as_ref().ok_or_else(|| {
                        format!(
                            "operation {} Different source provenance lacks its Constraint",
                            operation.operation_id
                        )
                    })?,
                    "portable Different Constraint",
                )?;
                if let Some(assignment_row) =
                    indexed.row(&CorrectionTable::Assignment, &constraint.face_id)
                {
                    let source: Assignment = decode_strict_typed_value(
                        assignment_row.before.as_ref().ok_or_else(|| {
                            format!(
                                "operation {} Different strict source omits its prior value",
                                operation.operation_id
                            )
                        })?,
                        "portable Different strict Assignment source",
                    )?;
                    validate_historical_strict_assignment_source_provenance(
                        operation,
                        &source,
                        generation_by_id,
                    )?;
                } else {
                    let suggestions = matching_deleted_suggestions(
                        operation,
                        &indexed.envelope,
                        &constraint.face_id,
                        &constraint.person_id,
                    )?;
                    let source = suggestions
                        .first()
                        .filter(|_| suggestions.len() == 1)
                        .ok_or_else(|| {
                            format!(
                                "operation {} Different suggestion source is ambiguous or absent",
                                operation.operation_id
                            )
                        })?;
                    let provenance =
                        exact_suggestion_source_provenance(operation, source, provenance_rows)?;
                    used_provenance.insert(provenance.provenance_id.as_str());
                    validate_historical_suggestion_source_provenance(
                        operation,
                        source,
                        provenance,
                        None,
                        None,
                        None,
                        generation_by_id,
                    )?;
                }
            }
        }
        "correction_change_person" | "correction_batch_change_person" => {
            for constraint_row in indexed
                .envelope
                .rows
                .iter()
                .filter(|row| row.table == CorrectionTable::Constraint)
            {
                let constraint: CannotLinkConstraint = decode_strict_typed_value(
                    constraint_row.after.as_ref().ok_or_else(|| {
                        format!(
                            "operation {} Change-person source Constraint is not durable",
                            operation.operation_id
                        )
                    })?,
                    "portable Change-person source Constraint",
                )?;
                let assignment_row = indexed
                    .row(&CorrectionTable::Assignment, &constraint.face_id)
                    .ok_or_else(|| {
                        format!(
                            "operation {} Change-person source provenance lacks its Assignment",
                            operation.operation_id
                        )
                    })?;
                if let Some(before) = assignment_row.before.as_ref() {
                    let source: Assignment = decode_strict_typed_value(
                        before,
                        "portable Change-person Assignment source",
                    )?;
                    if source.state == "committed_strict_automatic" {
                        validate_historical_strict_assignment_source_provenance(
                            operation,
                            &source,
                            generation_by_id,
                        )?;
                    }
                    continue;
                }
                let resulting: Assignment = decode_strict_typed_value(
                    assignment_row.after.as_ref().ok_or_else(|| {
                        format!(
                            "operation {} Change-person source provenance lacks its result",
                            operation.operation_id
                        )
                    })?,
                    "portable Change-person resulting Assignment",
                )?;
                let suggestions = matching_deleted_suggestions(
                    operation,
                    &indexed.envelope,
                    &constraint.face_id,
                    &constraint.person_id,
                )?;
                let source = suggestions
                    .first()
                    .filter(|_| suggestions.len() == 1)
                    .ok_or_else(|| {
                        format!(
                            "operation {} Change-person suggestion source is ambiguous or absent",
                            operation.operation_id
                        )
                    })?;
                let provenance =
                    exact_suggestion_source_provenance(operation, source, provenance_rows)?;
                used_provenance.insert(provenance.provenance_id.as_str());
                validate_historical_suggestion_source_provenance(
                    operation,
                    source,
                    provenance,
                    Some(resulting.face_revision),
                    None,
                    None,
                    generation_by_id,
                )?;
            }
        }
        _ => {}
    }
    if used_provenance.len() != provenance_rows.len() {
        return Err(format!(
            "operation {} SuggestionSourceProvenance entity set is not exact",
            operation.operation_id
        ));
    }
    for provenance in provenance_rows {
        validate_suggestion_source_anchor(
            operation,
            provenance,
            face_by_id,
            person_by_id,
            correction_by_operation,
            operation_by_id,
            anchor_work,
        )?;
    }
    Ok(())
}

fn validate_different_correction_effect(
    operation: &MatchOperation,
    envelope: &ExchangeCorrectionDeltaEnvelope,
    kind: &str,
) -> Result<(), String> {
    if !envelope.identity_changed || envelope.catalog_changed {
        return Err(format!(
            "operation {} Different correction has invalid revision effects",
            operation.operation_id
        ));
    }
    let mut constraints = 0usize;
    let mut effect_face_ids = BTreeSet::new();
    let mut effect_person_ids = BTreeSet::new();
    let mut effect_pairs = BTreeMap::new();
    for row in &envelope.rows {
        if row.table == CorrectionTable::Assignment && row.after.is_some() {
            return Err(format!(
                "operation {} Different correction creates an Assignment",
                operation.operation_id
            ));
        }
        if row.table == CorrectionTable::Constraint {
            let after = row.after.as_ref().ok_or_else(|| {
                format!(
                    "operation {} Different correction deletes its cannot-link evidence",
                    operation.operation_id
                )
            })?;
            let constraint: CannotLinkConstraint = decode_strict_typed_value(
                after,
                &format!(
                    "operation {} Different constraint effect",
                    operation.operation_id
                ),
            )?;
            if constraint.operation_id != operation.operation_id || !constraint.operator_owned {
                return Err(format!(
                    "operation {} Different correction has unauthoritative cannot-link evidence",
                    operation.operation_id
                ));
            }
            effect_face_ids.insert(constraint.face_id.clone());
            effect_person_ids.insert(constraint.person_id.clone());
            effect_pairs.insert(constraint.face_id.clone(), constraint.person_id.clone());
            constraints += 1;
        }
    }
    if constraints == 0 {
        return Err(format!(
            "operation {} Different correction has no cannot-link effect",
            operation.operation_id
        ));
    }
    let envelope_face_ids = envelope.face_ids.iter().cloned().collect::<BTreeSet<_>>();
    let batch = matches!(kind, "batch_different" | "batch_this_is_not");
    if effect_face_ids != envelope_face_ids
        || effect_person_ids.len() != 1
        || operation.person_id.as_deref() != effect_person_ids.first().map(String::as_str)
        || (batch && operation.face_id.is_some())
        || (!batch
            && (constraints != 1
                || operation.face_id.as_deref() != effect_face_ids.first().map(String::as_str)))
    {
        return Err(format!(
            "operation {} Different correction identity envelope contradicts its constraint effects",
            operation.operation_id
        ));
    }
    let different_requires_unresolved_candidate = matches!(kind, "different" | "batch_different");
    for (face_id, person_id) in &effect_pairs {
        let source_assignment = envelope
            .rows
            .iter()
            .find(|row| row.table == CorrectionTable::Assignment && row.stable_id == *face_id)
            .map(|row| {
                let before = row.before.as_ref().ok_or_else(|| {
                    format!(
                        "operation {} Different source Assignment omits its prior evidence",
                        operation.operation_id
                    )
                })?;
                decode_strict_typed_value::<Assignment>(
                    before,
                    &format!(
                        "operation {} Different source Assignment",
                        operation.operation_id
                    ),
                )
            })
            .transpose()?;
        if let Some(source) = source_assignment {
            let accepted_state = if different_requires_unresolved_candidate {
                source.state == "committed_strict_automatic"
            } else {
                matches!(
                    source.state.as_str(),
                    "committed_strict_automatic" | "operator_confirmed"
                )
            };
            if !accepted_state || source.face_id != *face_id || source.person_id != *person_id {
                return Err(format!(
                    "operation {} Different correction has non-qualifying Assignment source evidence",
                    operation.operation_id
                ));
            }
        } else if different_requires_unresolved_candidate {
            if matching_deleted_suggestions(operation, envelope, face_id, person_id)?.len() != 1 {
                return Err(format!(
                    "operation {} Different correction lacks a qualifying deleted suggestion or strict Assignment",
                    operation.operation_id
                ));
            }
        } else {
            return Err(format!(
                "operation {} This-is-not correction requires a deleted committed Assignment",
                operation.operation_id
            ));
        }
    }
    for row in &envelope.rows {
        match row.table {
            CorrectionTable::Constraint => {}
            CorrectionTable::Assignment => {
                let before = row.before.as_ref().ok_or_else(|| {
                    format!(
                        "operation {} Different correction has an unowned Assignment effect",
                        operation.operation_id
                    )
                })?;
                if row.after.is_some() {
                    return Err(format!(
                        "operation {} Different correction creates an Assignment",
                        operation.operation_id
                    ));
                }
                let assignment: Assignment = decode_strict_typed_value(
                    before,
                    &format!(
                        "operation {} Different Assignment deletion",
                        operation.operation_id
                    ),
                )?;
                if !effect_face_ids.contains(&assignment.face_id)
                    || !effect_person_ids.contains(&assignment.person_id)
                {
                    return Err(format!(
                        "operation {} Different Assignment deletion targets another Face/Person pair",
                        operation.operation_id
                    ));
                }
            }
            CorrectionTable::Suggestion
            | CorrectionTable::TrustedMember
            | CorrectionTable::TrustedSearch => {
                validate_face_bound_auxiliary_deletion(operation, row, &effect_face_ids)?;
            }
            _ => {
                return Err(format!(
                    "operation {} Different correction contains an unsupported typed row effect",
                    operation.operation_id
                ));
            }
        }
    }
    Ok(())
}

fn validate_face_bound_auxiliary_deletion(
    operation: &MatchOperation,
    row: &CorrectionRowDelta,
    effect_face_ids: &BTreeSet<String>,
) -> Result<(), String> {
    let before = row.before.as_ref().ok_or_else(|| {
        format!(
            "operation {} correction auxiliary effect omits its prior row",
            operation.operation_id
        )
    })?;
    if row.after.is_some() {
        return Err(format!(
            "operation {} correction auxiliary effect is not a deletion",
            operation.operation_id
        ));
    }
    let face_id = match row.table {
        CorrectionTable::Suggestion => {
            decode_strict_typed_value::<Suggestion>(
                before,
                &format!(
                    "operation {} correction Suggestion deletion",
                    operation.operation_id
                ),
            )?
            .face_id
        }
        CorrectionTable::TrustedMember => {
            decode_strict_typed_value::<TrustedTemplateMembership>(
                before,
                &format!(
                    "operation {} correction trusted-member deletion",
                    operation.operation_id
                ),
            )?
            .face_id
        }
        CorrectionTable::TrustedSearch => {
            decode_correction_trusted_search(
                before,
                &format!(
                    "operation {} correction trusted-search deletion",
                    operation.operation_id
                ),
            )?
            .face_id
        }
        _ => unreachable!("caller restricts auxiliary correction tables"),
    };
    if !effect_face_ids.contains(&face_id) {
        return Err(format!(
            "operation {} correction auxiliary deletion targets another FaceId",
            operation.operation_id
        ));
    }
    Ok(())
}

fn is_closed_correction_kind(kind: &str) -> bool {
    matches!(
        kind,
        "change_person"
            | "same"
            | "move_to_look"
            | "same_person_new_look"
            | "different"
            | "this_is_not"
            | "remove_assignment"
            | "batch_same"
            | "batch_different"
            | "batch_not_sure"
            | "batch_this_is_not"
            | "batch_change_person"
            | "batch_remove_assignments"
            | "batch_ignore_face"
            | "batch_not_a_face"
            | "batch_delete_face_analysis"
            | "delete_face_analysis"
            | "manual_face"
            | "manual_face_and_assign"
            | "remove_person"
            | "merge_people"
            | "split_person"
            | "split_to_person"
            | "ignored"
            | "not_a_face"
            | "not_sure"
            | "video_assign"
            | "video_track_split"
    )
}

fn correction_row_values_semantically_equal(
    table: &CorrectionTable,
    before: &Value,
    after: &Value,
) -> Result<bool, String> {
    macro_rules! equal {
        ($ty:ty, $label:literal) => {{
            let before = decode_strict_typed_value::<$ty>(before, concat!("correction ", $label))?;
            let after = decode_strict_typed_value::<$ty>(after, concat!("correction ", $label))?;
            Ok(before == after)
        }};
    }
    match table {
        CorrectionTable::Person => equal!(Person, "Person"),
        CorrectionTable::Look => equal!(Look, "Look"),
        CorrectionTable::TemplateSet => equal!(TrustedTemplateSet, "TrustedTemplateSet"),
        CorrectionTable::Face => equal!(FaceObservation, "FaceObservation"),
        CorrectionTable::Embedding => {
            let before = decode_correction_embedding(before, "correction FaceEmbedding")?;
            let after = decode_correction_embedding(after, "correction FaceEmbedding")?;
            Ok(before == after)
        }
        CorrectionTable::Assignment => equal!(Assignment, "Assignment"),
        CorrectionTable::Constraint => equal!(CannotLinkConstraint, "CannotLinkConstraint"),
        CorrectionTable::TrustedMember => {
            equal!(TrustedTemplateMembership, "TrustedTemplateMembership")
        }
        CorrectionTable::TrustedSearch => {
            let before =
                decode_correction_trusted_search(before, "correction TrustedSearchEmbedding")?;
            let after =
                decode_correction_trusted_search(after, "correction TrustedSearchEmbedding")?;
            Ok(before == after)
        }
        CorrectionTable::Disposition => equal!(FaceDisposition, "FaceDisposition"),
        CorrectionTable::Suggestion => equal!(Suggestion, "Suggestion"),
        CorrectionTable::VideoObservation => equal!(StoredVideoObservation, "VideoObservation"),
    }
}

fn validate_correction_row_value(
    table: &CorrectionTable,
    stable_id: &str,
    value: &Value,
) -> Result<(), String> {
    macro_rules! decode {
        ($ty:ty, $label:literal) => {
            decode_strict_typed_value::<$ty>(value, concat!("correction ", $label))?
        };
    }
    match table {
        CorrectionTable::Person => {
            let row = decode!(Person, "Person");
            if row.person_id != stable_id {
                return Err("correction Person stable ID mismatch".to_string());
            }
        }
        CorrectionTable::Look => {
            let row = decode!(Look, "Look");
            if row.look_id != stable_id {
                return Err("correction Look stable ID mismatch".to_string());
            }
        }
        CorrectionTable::TemplateSet => {
            let row = decode!(TrustedTemplateSet, "TrustedTemplateSet");
            if row.set_id != stable_id {
                return Err("correction TrustedTemplateSet stable ID mismatch".to_string());
            }
        }
        CorrectionTable::Face => {
            let row = decode!(FaceObservation, "FaceObservation");
            if row.face_id != stable_id {
                return Err("correction FaceObservation stable ID mismatch".to_string());
            }
            validate_face(&row)?;
            require_media_sha256(
                "historical portable Face media fingerprint",
                &row.media_fingerprint,
            )?;
        }
        CorrectionTable::Embedding => {
            let row = decode_correction_embedding(value, "correction FaceEmbedding")?;
            if row.embedding_id != stable_id
                || row.embedding_id != embedding_id(&row.face_id, &row.model_generation)
            {
                return Err("correction FaceEmbedding canonical ID mismatch".to_string());
            }
            if let Some(vector) = &row.vector {
                validate_vector(vector)?;
            }
            require_media_sha256(
                "historical portable Embedding media fingerprint",
                &row.media_fingerprint,
            )?;
            parse_bounded_rfc3339_instant(
                "historical portable Embedding created_at is not RFC3339",
                &row.created_at,
            )?;
        }
        CorrectionTable::Assignment => {
            let row = decode!(Assignment, "Assignment");
            validate_historical_assignment(&row, stable_id)?;
        }
        CorrectionTable::Constraint => {
            let row = decode!(CannotLinkConstraint, "CannotLinkConstraint");
            validate_historical_constraint(&row, stable_id)?;
        }
        CorrectionTable::TrustedMember => {
            let row = decode!(TrustedTemplateMembership, "TrustedTemplateMembership");
            validate_historical_membership(&row, stable_id)?;
        }
        CorrectionTable::TrustedSearch => {
            let row = decode_correction_trusted_search(value, "correction TrustedSearchEmbedding")?;
            if row.membership_id != stable_id {
                return Err("correction TrustedSearch stable ID mismatch".to_string());
            }
            if let Some(vector) = &row.vector {
                validate_vector(vector)?;
            }
        }
        CorrectionTable::Disposition => {
            let row = decode!(FaceDisposition, "FaceDisposition");
            if row.face_id != stable_id
                || !matches!(row.disposition.as_str(), "ignored" | "not_a_face")
            {
                return Err("correction FaceDisposition typed effect mismatch".to_string());
            }
        }
        CorrectionTable::Suggestion => {
            let row = decode!(Suggestion, "Suggestion");
            if row.suggestion_id != stable_id
                || row.suggestion_id != suggestion_id(&row.face_id, &row.candidate_person_id)
                || !row.similarity.is_finite()
            {
                return Err("correction Suggestion canonical effect mismatch".to_string());
            }
            require_media_sha256(
                "historical portable Suggestion media fingerprint",
                &row.media_fingerprint,
            )?;
            parse_bounded_rfc3339_instant(
                "historical portable Suggestion created_at is not RFC3339",
                &row.created_at,
            )?;
        }
        CorrectionTable::VideoObservation => {
            let row = decode!(StoredVideoObservation, "VideoObservation");
            if row.observation_id != stable_id {
                return Err("video correction stable ID mismatch".into());
            }
            row.observation()?;
        }
    }
    Ok(())
}

fn validate_historical_assignment(row: &Assignment, stable_id: &str) -> Result<(), String> {
    if row.assignment_id != stable_id
        || row.assignment_id != row.face_id
        || !matches!(
            row.state.as_str(),
            "operator_confirmed" | "committed_strict_automatic"
        )
        || (row.look_id.is_some() && row.placement != "look")
        || (row.look_id.is_none() && row.placement != "unsorted")
    {
        return Err(format!(
            "historical Assignment {} has invalid typed identity or placement",
            row.assignment_id
        ));
    }
    validate_exchange_media_key(&row.media_key)?;
    validate_text("historical assignment provenance", &row.provenance)?;
    match row.state.as_str() {
        "operator_confirmed"
            if row.locked
                && row.model_generation.is_none()
                && row.calibration_generation.is_none()
                && row.envelope_hash.is_none() => {}
        "committed_strict_automatic"
            if !row.locked
                && row.provenance == "strict_recognition_v1"
                && row.model_generation.is_some()
                && row.calibration_generation.is_some()
                && row.envelope_hash.is_some() =>
        {
            validate_text(
                "historical strict calibration generation",
                row.calibration_generation.as_deref().unwrap(),
            )?;
            validate_sha256(
                "historical strict envelope hash",
                row.envelope_hash.as_deref().unwrap(),
            )?;
        }
        _ => {
            return Err(format!(
                "historical Assignment {} contradicts its evidence state",
                row.assignment_id
            ))
        }
    }
    Ok(())
}

fn validate_historical_constraint(
    row: &CannotLinkConstraint,
    stable_id: &str,
) -> Result<(), String> {
    if row.constraint_id != stable_id
        || row.constraint_id != cannot_link_id(&row.face_id, &row.person_id)
        || !row.operator_owned
    {
        return Err(format!(
            "historical CannotLinkConstraint {} is not canonical operator evidence",
            row.constraint_id
        ));
    }
    Ok(())
}

fn validate_historical_membership(
    row: &TrustedTemplateMembership,
    stable_id: &str,
) -> Result<(), String> {
    if row.membership_id != stable_id
        || row.membership_id != trusted_member_id(&row.set_id, &row.face_id)
        || row.embedding_id != embedding_id(&row.face_id, &row.model_generation)
        || !row.quality_score.is_finite()
        || !row.quality_threshold.is_finite()
    {
        return Err(format!(
            "historical TrustedTemplateMembership {} has non-canonical evidence identity",
            row.membership_id
        ));
    }
    validate_text("historical trusted provenance", &row.provenance)?;
    Ok(())
}

fn exchange_correction_media_mapping_id(operation_id: &str, media_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(operation_id.as_bytes());
    hash.update([0]);
    hash.update(media_key.as_bytes());
    format!("correction-media-{:x}", hash.finalize())
}

fn validate_operation_json(operation_id: &str, field: &str, text: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("operation {operation_id} {field} is invalid: {error}"))?;
    let mut depth = 0;
    let mut string_bytes = 0;
    inspect_json_limits(&value, 1, &mut depth, &mut string_bytes)
        .map_err(|error| format!("operation {operation_id} {field}: {error}"))
}

fn validate_relocated_identity_graph(graph: &IdentityBundleGraph) -> Result<usize, String> {
    for root in &graph.roots {
        validate_relocation_root(&root.portable_path)?;
    }
    let mut portable = graph.clone();
    for root in &mut portable.roots {
        root.portable_path = portable_root_token(&root.root_id);
    }
    validate_identity_graph(&portable)
}

fn unique_ids<'a>(
    label: &str,
    ids: impl Iterator<Item = &'a str>,
) -> Result<BTreeSet<String>, String> {
    let mut seen = BTreeSet::new();
    for id in ids {
        validate_text(label, id)?;
        if !seen.insert(id.to_string()) {
            return Err(format!("duplicate {label} stable ID {id}"));
        }
    }
    Ok(seen)
}

fn require_ref(
    label: &str,
    id: &str,
    ids: &BTreeSet<String>,
    references: &mut usize,
) -> Result<(), String> {
    *references = references
        .checked_add(1)
        .ok_or_else(|| "identity reference count overflow".to_string())?;
    if *references > IDENTITY_BUNDLE_MAX_REFERENCES {
        return Err(format!(
            "identity references exceed {IDENTITY_BUNDLE_MAX_REFERENCES}"
        ));
    }
    if ids.contains(id) {
        Ok(())
    } else {
        Err(format!("dangling {label} reference {id}"))
    }
}

fn portable_root_token(root_id: &str) -> String {
    format!("roots/{}", &sha256_bytes(root_id.as_bytes())[..24])
}

fn validate_portable_path(value: &str) -> Result<(), String> {
    validate_relative_path_text("portable root path", value)?;
    if !value.starts_with("roots/") {
        return Err("portable root path must use the roots/ namespace".to_string());
    }
    Ok(())
}

fn validate_root_exclusion(value: &str) -> Result<(), String> {
    validate_relative_path_text("root exclusion", value)?;
    for component in value.split('/') {
        validate_portable_file_leaf("root exclusion component", std::ffi::OsStr::new(component))?;
    }
    Ok(())
}

fn validate_relative_path_text(label: &str, value: &str) -> Result<(), String> {
    validate_text(label, value)?;
    let path = Path::new(value);
    if path.is_absolute()
        || value.contains('\\')
        || value.contains(':')
        || value.starts_with('/')
        || value.ends_with('/')
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
        || value.split('/').any(str::is_empty)
    {
        return Err(format!("{label} must be canonical relative slash form"));
    }
    Ok(())
}

/// Accept only a portable, normal Win32 filename component.  Applying this
/// contract on every platform keeps an exchange path from changing meaning
/// when the same bundle is later handled on Windows (notably NTFS ADS and DOS
/// device aliases).
fn validate_portable_file_leaf(label: &str, leaf: &std::ffi::OsStr) -> Result<(), String> {
    let value = leaf
        .to_str()
        .ok_or_else(|| format!("{label} filename must be valid UTF-8"))?;
    validate_text(label, value)?;
    if matches!(value, "." | "..")
        || value.starts_with(' ')
        || value.ends_with([' ', '.'])
        || value.chars().any(|character| {
            character <= '\u{1f}'
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return Err(format!(
            "{label} filename is not a portable normal Win32 leaf"
        ));
    }
    let device_stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let numbered_device = ["COM", "LPT"].into_iter().any(|prefix| {
        device_stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if matches!(
        device_stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered_device
    {
        return Err(format!(
            "{label} filename uses a reserved Win32 device name"
        ));
    }
    Ok(())
}

fn validate_exchange_media_key(value: &str) -> Result<(), String> {
    super::validate_media_key(value)?;
    for component in value.split('/') {
        validate_portable_file_leaf(
            "portable media-key component",
            std::ffi::OsStr::new(component),
        )?;
    }
    Ok(())
}

fn validate_relocation_root(value: &str) -> Result<(), String> {
    canonical_relocation_root(value).map(|_| ())
}

fn canonical_relocation_root(value: &str) -> Result<String, String> {
    validate_text("relocation root", value)?;
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("relocation root must be an absolute canonical host path".to_string());
    }
    let guard = guard_existing_directory_chain(path, "relocation root")?;
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("canonicalize relocation root: {error}"))?;
    if normalized_host_path(&canonical) != normalized_host_path(path) {
        return Err("relocation root must already be in canonical host-path form".to_string());
    }
    guard.revalidate("relocation root")?;
    // Preserve the portable logical RootId, but persist exactly the same host
    // spelling as configure_index_root. Workers compare it to fs::canonicalize
    // without display normalization (notably the Windows extended-path prefix).
    Ok(canonical.to_string_lossy().into_owned())
}

#[derive(Debug)]
struct RelocatedMediaEvidence {
    media_key: String,
    path: PathBuf,
    guard: std::sync::Arc<File>,
    file_len: u64,
    observed_sha256: String,
}

#[derive(Debug, Default)]
struct RelocationVerificationBudget {
    hashed_bytes: u64,
}

impl RelocationVerificationBudget {
    fn authorize_file(&mut self, media_key: &str, bytes: u64) -> Result<(), String> {
        if bytes > IDENTITY_RELOCATION_MAX_FILE_BYTES {
            return Err(format!(
                "relocated media {media_key} is {bytes} bytes; fingerprint verification file limit is {IDENTITY_RELOCATION_MAX_FILE_BYTES}"
            ));
        }
        self.hashed_bytes = self
            .hashed_bytes
            .checked_add(bytes)
            .ok_or_else(|| "media relocation fingerprint byte work overflow".to_string())?;
        if self.hashed_bytes > IDENTITY_RELOCATION_MAX_TOTAL_BYTES {
            return Err(format!(
                "media relocation fingerprint verification exceeds aggregate byte limit {IDENTITY_RELOCATION_MAX_TOTAL_BYTES}"
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RelocatedMediaResolution {
    unresolved: Vec<String>,
    content_mismatches: Vec<String>,
    evidence: Vec<RelocatedMediaEvidence>,
}

fn unresolved_relocated_media_keys(graph: &IdentityBundleGraph) -> Result<Vec<String>, String> {
    let resolution = resolve_relocated_media(graph)?;
    let mut unresolved = resolution.unresolved;
    unresolved.extend(resolution.content_mismatches);
    unresolved.sort();
    Ok(unresolved)
}

#[derive(Debug)]
enum RelocatedMediaProbe {
    Unresolved,
    ContentMismatch,
    Resolved(RelocatedMediaEvidence),
}

fn resolve_relocated_media(
    graph: &IdentityBundleGraph,
) -> Result<RelocatedMediaResolution, String> {
    let mut fingerprints = BTreeMap::<String, String>::new();
    let mut referenced = BTreeSet::new();
    // Operator context may intentionally describe an older fingerprint. Preserve
    // it as metadata, never as current identity/media-resolution evidence.
    for (_, (media_key, media_fingerprint)) in portable_face_media_evidence(graph)? {
        let media_fingerprint =
            require_media_sha256("portable Face media fingerprint", &media_fingerprint)?
                .to_string();
        if let Some(existing) = fingerprints.insert(media_key.clone(), media_fingerprint.clone()) {
            if existing != media_fingerprint {
                return Err(format!(
                    "media key {media_key} has conflicting current/historical fingerprints in the identity bundle"
                ));
            }
        }
        referenced.insert(media_key);
    }
    for person in &graph.people {
        if let Some(media_key) = &person.cover_media_key {
            validate_exchange_media_key(media_key)?;
            referenced.insert(media_key.clone());
        }
    }
    for mapping in &graph.correction_media_operations {
        validate_exchange_media_key(&mapping.media_key)?;
        validate_sha256(
            "correction media mapping fingerprint",
            &mapping.media_fingerprint,
        )?;
        if let Some(existing) =
            fingerprints.insert(mapping.media_key.clone(), mapping.media_fingerprint.clone())
        {
            if existing != mapping.media_fingerprint {
                return Err(format!(
                    "media key {} has conflicting correction mapping fingerprints in the identity bundle",
                    mapping.media_key
                ));
            }
        }
        referenced.insert(mapping.media_key.clone());
    }
    for provenance in &graph.suggestion_source_provenance {
        validate_exchange_media_key(&provenance.media_key)?;
        let fingerprint = require_media_sha256(
            "SuggestionSourceProvenance media fingerprint",
            &provenance.media_fingerprint,
        )?;
        if let Some(existing) =
            fingerprints.insert(provenance.media_key.clone(), fingerprint.to_string())
        {
            if existing != fingerprint {
                return Err(format!(
                    "media key {} has conflicting current/historical fingerprints in the identity bundle",
                    provenance.media_key
                ));
            }
        }
        referenced.insert(provenance.media_key.clone());
    }
    if referenced.len() > IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES {
        return Err(format!(
            "media relocation requires {} retained evidence handles; limit is {IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES}",
            referenced.len()
        ));
    }

    let mut work = 0usize;
    let mut budget = RelocationVerificationBudget::default();
    let mut unresolved = Vec::new();
    let mut content_mismatches = Vec::new();
    let mut evidence = Vec::new();
    for media_key in referenced {
        let mut regular_matches = 0usize;
        let mut mismatch_count = 0usize;
        let mut resolved = None;
        let expected_fingerprint = fingerprints.get(&media_key).ok_or_else(|| {
            format!("media key {media_key} has no canonical SHA-256 fingerprint evidence")
        })?;
        for root in &graph.roots {
            work = work
                .checked_add(1)
                .ok_or_else(|| "media relocation resolution work overflow".to_string())?;
            if work > IDENTITY_BUNDLE_MAX_REFERENCES {
                return Err(format!(
                    "media relocation resolution exceeds {IDENTITY_BUNDLE_MAX_REFERENCES} root/key checks"
                ));
            }
            match relocated_media_evidence(
                Path::new(&root.portable_path),
                &media_key,
                expected_fingerprint,
                &mut budget,
            )? {
                RelocatedMediaProbe::Resolved(candidate) => {
                    regular_matches += 1;
                    if resolved.is_none() {
                        resolved = Some(candidate);
                    }
                    if regular_matches > 1 {
                        break;
                    }
                }
                RelocatedMediaProbe::ContentMismatch => mismatch_count += 1,
                RelocatedMediaProbe::Unresolved => {}
            }
        }
        // Zero matches is missing/linked/non-regular unless at least one
        // regular candidate was hashed and proved to contain different bytes.
        // More than one match is ambiguous: a portable MediaKey must resolve
        // to exactly one explicit relocated root before import may mutate.
        if regular_matches != 1 {
            if regular_matches == 0 && mismatch_count > 0 {
                content_mismatches.push(media_key);
            } else {
                unresolved.push(media_key);
            }
        } else if let Some(resolved) = resolved {
            evidence.push(resolved);
        }
    }
    Ok(RelocatedMediaResolution {
        unresolved,
        content_mismatches,
        evidence,
    })
}

fn relocated_media_evidence(
    root: &Path,
    media_key: &str,
    expected_fingerprint: &str,
    budget: &mut RelocationVerificationBudget,
) -> Result<RelocatedMediaProbe, String> {
    validate_exchange_media_key(media_key)?;
    for component in media_key.split('/') {
        validate_portable_file_leaf("relocated media component", std::ffi::OsStr::new(component))?;
    }
    if !root.is_absolute() {
        return Ok(RelocatedMediaProbe::Unresolved);
    }
    let candidate = media_key
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component));
    let Some(parent) = candidate.parent() else {
        return Ok(RelocatedMediaProbe::Unresolved);
    };
    let Some(leaf) = candidate.file_name() else {
        return Ok(RelocatedMediaProbe::Unresolved);
    };
    validate_portable_file_leaf("relocated media", leaf)?;
    let guard = match guard_existing_directory_chain(parent, "relocated media directory") {
        Ok(guard) => guard,
        Err(_) => return Ok(RelocatedMediaProbe::Unresolved),
    };
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(RelocatedMediaProbe::Unresolved),
    };
    if metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&metadata)
        || !metadata.is_file()
    {
        return Ok(RelocatedMediaProbe::Unresolved);
    }
    let mut first = match open_guarded_regular_leaf(&guard, leaf, "relocated media") {
        Ok(file) => file,
        Err(_) => return Ok(RelocatedMediaProbe::Unresolved),
    };
    let second = match open_guarded_regular_leaf(&guard, leaf, "relocated media identity probe") {
        Ok(file) => file,
        Err(_) => return Ok(RelocatedMediaProbe::Unresolved),
    };
    let first_metadata = first
        .metadata()
        .map_err(|error| format!("inspect relocated media handle: {error}"))?;
    if !first_metadata.is_file() || !same_open_file_identity(&first, &second)? {
        return Ok(RelocatedMediaProbe::Unresolved);
    }
    validate_sha256("relocated media expected fingerprint", expected_fingerprint)?;
    let observed_sha256 =
        hash_open_relocated_media(&mut first, media_key, first_metadata.len(), budget)?;
    guard.revalidate("relocated media directory")?;
    if expected_fingerprint != observed_sha256 {
        return Ok(RelocatedMediaProbe::ContentMismatch);
    }
    Ok(RelocatedMediaProbe::Resolved(RelocatedMediaEvidence {
        media_key: media_key.to_string(),
        path: candidate,
        guard: std::sync::Arc::new(first),
        file_len: first_metadata.len(),
        observed_sha256,
    }))
}

fn hash_open_relocated_media(
    file: &mut File,
    media_key: &str,
    expected_len: u64,
    budget: &mut RelocationVerificationBudget,
) -> Result<String, String> {
    budget.authorize_file(media_key, expected_len)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek relocated media for fingerprint proof: {error}"))?;
    let mut hasher = Sha256::new();
    let mut observed_len = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("hash relocated media fingerprint: {error}"))?;
        if read == 0 {
            break;
        }
        observed_len = observed_len
            .checked_add(u64::try_from(read).expect("buffer read length fits u64"))
            .ok_or_else(|| "relocated media fingerprint length overflow".to_string())?;
        if observed_len > expected_len {
            return Err(format!(
                "relocated media {media_key} changed length during fingerprint verification"
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if observed_len != expected_len {
        return Err(format!(
            "relocated media {media_key} changed length during fingerprint verification"
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn revalidate_relocated_media_evidence(evidence: &[RelocatedMediaEvidence]) -> Result<(), String> {
    let mut budget = RelocationVerificationBudget::default();
    for item in evidence {
        let parent = item
            .path
            .parent()
            .ok_or_else(|| format!("relocated media {} has no parent", item.media_key))?;
        let leaf = item
            .path
            .file_name()
            .ok_or_else(|| format!("relocated media {} has no filename", item.media_key))?;
        let directory = guard_existing_directory_chain(parent, "relocated media revalidation")?;
        let mut observed =
            open_guarded_regular_leaf(&directory, leaf, "relocated media revalidation")?;
        let metadata = observed
            .metadata()
            .map_err(|error| format!("inspect relocated media revalidation handle: {error}"))?;
        if !metadata.is_file()
            || metadata.len() != item.file_len
            || !same_open_file_identity(item.guard.as_ref(), &observed)?
        {
            return Err(format!(
                "relocated media {} changed path or file identity",
                item.media_key
            ));
        }
        let observed_hash =
            hash_open_relocated_media(&mut observed, &item.media_key, item.file_len, &mut budget)?;
        if observed_hash != item.observed_sha256 {
            return Err(format!(
                "relocated media {} changed fingerprint",
                item.media_key
            ));
        }
        directory.revalidate("relocated media revalidation")?;
    }
    Ok(())
}

fn canonical_sha256_fingerprint(value: &str) -> Option<&str> {
    canonical_media_sha256(value)
}

fn require_media_sha256<'a>(label: &str, value: &'a str) -> Result<&'a str, String> {
    canonical_media_sha256(value)
        .ok_or_else(|| format!("{label} must contain a lowercase SHA-256 digest"))
}

fn normalized_host_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        text = format!("\\\\{rest}");
    } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
        text = rest.to_string();
    }
    while text.len() > 3 && text.ends_with('\\') {
        text.pop();
    }
    if cfg!(windows) {
        text.make_ascii_lowercase();
    }
    text
}

fn absolute_lexical_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let raw = path.as_os_str().to_string_lossy();
    #[cfg(windows)]
    let has_dot_segment = raw
        .split(['\\', '/'])
        .any(|segment| matches!(segment, "." | ".."));
    #[cfg(not(windows))]
    let has_dot_segment = raw.split('/').any(|segment| matches!(segment, "." | ".."));
    if has_dot_segment
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("{label} must not contain . or .. components"));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| format!("resolve current directory for {label}: {error}"))?
            .join(path))
    }
}

struct ExistingDirectoryChainGuard {
    absolute_path: PathBuf,
    entries: Vec<(PathBuf, File)>,
}

impl ExistingDirectoryChainGuard {
    fn parent_handle(&self) -> Result<&File, String> {
        self.entries
            .last()
            .map(|(_, handle)| handle)
            .ok_or_else(|| "directory guard has no retained handle".to_string())
    }

    fn revalidate(&self, label: &str) -> Result<(), String> {
        for (path, retained) in &self.entries {
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("reinspect {label} {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink()
                || metadata_is_reparse_point(&metadata)
                || !metadata.is_dir()
            {
                return Err(format!(
                    "{label} ancestor {} changed type or became a link",
                    path.display()
                ));
            }
            let reopened = open_directory_no_links(path, label)?;
            if !same_open_file_identity(retained, &reopened)? {
                return Err(format!(
                    "{label} ancestor {} changed identity",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn sync_guarded_directory(guard: &ExistingDirectoryChainGuard, label: &str) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    guard.revalidate(label)?;
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(&guard.absolute_path).map_err(|error| {
        format!(
            "open {label} for durable directory sync {}: {error}",
            guard.absolute_path.display()
        )
    })?;
    if !same_open_file_identity(guard.parent_handle()?, &directory)? {
        return Err(format!("{label} changed identity before directory sync"));
    }
    if unsafe { FlushFileBuffers(directory.as_raw_handle() as _) } == 0 {
        return Err(format!(
            "sync {label} {}: {}",
            guard.absolute_path.display(),
            std::io::Error::last_os_error()
        ));
    }
    guard.revalidate(label)
}

#[cfg(not(windows))]
fn sync_guarded_directory(guard: &ExistingDirectoryChainGuard, label: &str) -> Result<(), String> {
    guard.revalidate(label)?;
    guard
        .parent_handle()?
        .sync_all()
        .map_err(|error| format!("sync {label} {}: {error}", guard.absolute_path.display()))?;
    guard.revalidate(label)
}

#[cfg(unix)]
fn create_recovery_directory_guarded(
    parent: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let leaf = CString::new(leaf.as_bytes())
        .map_err(|_| format!("{label} filename contains an embedded NUL"))?;
    let created =
        unsafe { libc::mkdirat(parent.parent_handle()?.as_raw_fd(), leaf.as_ptr(), 0o700) };
    if created != 0 {
        return Err(format!(
            "create descriptor-confined {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_recovery_directory_guarded(
    parent: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<(), String> {
    fs::create_dir(parent.absolute_path.join(leaf)).map_err(|error| {
        format!(
            "create handle-guarded {label} {}: {error}",
            parent.absolute_path.join(leaf).display()
        )
    })
}

#[cfg(unix)]
fn remove_recovery_leaf_guarded(
    directory: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    _retained: &File,
    label: &str,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let leaf = CString::new(leaf.as_bytes())
        .map_err(|_| format!("{label} filename contains an embedded NUL"))?;
    let removed =
        unsafe { libc::unlinkat(directory.parent_handle()?.as_raw_fd(), leaf.as_ptr(), 0) };
    if removed != 0 {
        return Err(format!(
            "remove descriptor-confined {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn remove_recovery_leaf_guarded(
    directory: &ExistingDirectoryChainGuard,
    _leaf: &std::ffi::OsStr,
    retained: &File,
    label: &str,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    directory.revalidate(&format!("{label} directory before handle deletion"))?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let removed = unsafe {
        SetFileInformationByHandle(
            retained.as_raw_handle() as _,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>())
                .expect("FILE_DISPOSITION_INFO size fits u32"),
        )
    };
    if removed == 0 {
        return Err(format!(
            "remove handle-confined {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn remove_recovery_leaf_guarded(
    directory: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    _retained: &File,
    label: &str,
) -> Result<(), String> {
    let path = directory.absolute_path.join(leaf);
    fs::remove_file(&path).map_err(|error| format!("remove {label} {}: {error}", path.display()))
}

fn sync_recovery_directory_parent(
    parent: &ExistingDirectoryChainGuard,
    label: &str,
) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_NEXT_RECOVERY_DIRECTORY_PARENT_SYNC.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(format!("injected {label} directory-sync failure"));
    }
    sync_guarded_directory(parent, label)
}

/// Create one app-owned recovery directory only beneath an already guarded
/// parent, then durably commit the new directory entry before it can contain a
/// destructive-operation predecessor.
pub(super) fn ensure_durable_recovery_directory(path: &Path, label: &str) -> Result<(), String> {
    let absolute = absolute_lexical_path(path, label)?;
    if let Ok(metadata) = fs::symlink_metadata(&absolute) {
        if metadata.file_type().is_symlink()
            || metadata_is_reparse_point(&metadata)
            || !metadata.is_dir()
        {
            return Err(format!(
                "{label} must be a regular directory and not a link"
            ));
        }
        let guard = guard_existing_directory_chain(&absolute, label)?;
        guard.revalidate(label)?;
        let parent = absolute
            .parent()
            .ok_or_else(|| format!("{label} has no parent"))?;
        let parent_guard = guard_existing_directory_chain(parent, &format!("{label} parent"))?;
        sync_recovery_directory_parent(&parent_guard, &format!("{label} existing owning parent"))?;
        return guard.revalidate(&format!("{label} after owning-parent sync"));
    }
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("{label} has no parent"))?;
    let leaf = absolute
        .file_name()
        .ok_or_else(|| format!("{label} has no filename"))?;
    validate_portable_file_leaf(label, leaf)?;
    let parent_guard = guard_existing_directory_chain(parent, &format!("{label} parent"))?;
    parent_guard.revalidate(&format!("{label} parent before creation"))?;
    create_recovery_directory_guarded(&parent_guard, leaf, label)?;
    let created_guard = guard_existing_directory_chain(&absolute, label)?;
    created_guard.revalidate(label)?;
    sync_recovery_directory_parent(&parent_guard, &format!("{label} owning parent"))?;
    created_guard.revalidate(&format!("{label} after owning-parent sync"))
}

/// Remove a fixed app-owned recovery leaf while retaining and revalidating its
/// owning directory, then durably commit the directory-entry transition.
pub(super) fn remove_durable_recovery_file(path: &Path, label: &str) -> Result<bool, String> {
    let absolute = absolute_lexical_path(path, label)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("{label} has no parent"))?;
    let leaf = absolute
        .file_name()
        .ok_or_else(|| format!("{label} has no filename"))?;
    validate_portable_file_leaf(label, leaf)?;
    let directory = guard_existing_directory_chain(parent, &format!("{label} directory"))?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("inspect {label} {}: {error}", absolute.display())),
    };
    if metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&metadata)
        || !metadata.is_file()
    {
        return Err(format!("refusing to remove linked or non-regular {label}"));
    }
    #[cfg(windows)]
    let retained = open_guarded_removable_leaf(&directory, leaf, label)?;
    #[cfg(not(windows))]
    let retained = open_guarded_regular_leaf(&directory, leaf, label)?;
    #[cfg(windows)]
    let probe = open_guarded_removable_leaf(&directory, leaf, label)?;
    #[cfg(not(windows))]
    let probe = open_guarded_regular_leaf(&directory, leaf, label)?;
    if !same_open_file_identity(&retained, &probe)? {
        return Err(format!("{label} changed identity before removal"));
    }
    drop(probe);
    directory.revalidate(&format!("{label} directory before removal"))?;
    remove_recovery_leaf_guarded(&directory, leaf, &retained, label)?;
    drop(retained);
    #[cfg(all(test, windows))]
    if FAIL_NEXT_RECOVERY_REMOVAL_AFTER_DELETE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(format!(
            "injected {label} directory-sync ambiguity after handle deletion"
        ));
    }
    sync_guarded_directory(&directory, &format!("{label} directory after removal"))?;
    Ok(true)
}

#[cfg(windows)]
fn open_directory_no_links(path: &Path, label: &str) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options
        .open(path)
        .map_err(|error| format!("open retained {label} {}: {error}", path.display()))
}

#[cfg(not(windows))]
fn open_directory_no_links(path: &Path, label: &str) -> Result<File, String> {
    File::open(path).map_err(|error| format!("open retained {label} {}: {error}", path.display()))
}

#[cfg(windows)]
fn guard_existing_directory_chain(
    path: &Path,
    label: &str,
) -> Result<ExistingDirectoryChainGuard, String> {
    let absolute_path = absolute_lexical_path(path, label)?;
    let mut probe = PathBuf::new();
    let mut entries = Vec::new();
    for component in absolute_path.components() {
        match component {
            Component::Prefix(_) => probe.push(component.as_os_str()),
            Component::RootDir => {
                probe.push(component.as_os_str());
                entries.push((probe.clone(), open_directory_no_links(&probe, label)?));
            }
            Component::Normal(value) => {
                probe.push(value);
                let metadata = fs::symlink_metadata(&probe).map_err(|error| {
                    format!("inspect {label} ancestor {}: {error}", probe.display())
                })?;
                if metadata.file_type().is_symlink()
                    || metadata_is_reparse_point(&metadata)
                    || !metadata.is_dir()
                {
                    return Err(format!(
                        "{label} must not traverse a symlink, reparse point, or non-directory"
                    ));
                }
                entries.push((probe.clone(), open_directory_no_links(&probe, label)?));
            }
            Component::CurDir | Component::ParentDir => {
                return Err(format!("{label} must not contain . or .. components"))
            }
        }
    }
    let guard = ExistingDirectoryChainGuard {
        absolute_path,
        entries,
    };
    guard.revalidate(label)?;
    Ok(guard)
}

#[cfg(unix)]
fn guard_existing_directory_chain(
    path: &Path,
    label: &str,
) -> Result<ExistingDirectoryChainGuard, String> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let absolute_path = absolute_lexical_path(path, label)?;
    let root_name = CString::new("/").expect("root contains no NUL");
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(format!(
            "open retained {label} root: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut entries = vec![(PathBuf::from("/"), unsafe { File::from_raw_fd(root_fd) })];
    let mut probe = PathBuf::from("/");
    for component in absolute_path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                let name = CString::new(value.as_bytes())
                    .map_err(|_| format!("{label} contains an embedded NUL"))?;
                let parent_fd = entries
                    .last()
                    .expect("root directory guard exists")
                    .1
                    .as_raw_fd();
                let fd = unsafe {
                    libc::openat(
                        parent_fd,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(format!(
                        "open retained {label} component {}: {}",
                        value.to_string_lossy(),
                        std::io::Error::last_os_error()
                    ));
                }
                probe.push(value);
                entries.push((probe.clone(), unsafe { File::from_raw_fd(fd) }));
            }
            Component::CurDir | Component::ParentDir => {
                return Err(format!("{label} must not contain . or .. components"))
            }
            Component::Prefix(_) => {
                return Err(format!("{label} has an unsupported Unix path prefix"))
            }
        }
    }
    let guard = ExistingDirectoryChainGuard {
        absolute_path,
        entries,
    };
    guard.revalidate(label)?;
    Ok(guard)
}

#[cfg(not(any(unix, windows)))]
fn guard_existing_directory_chain(
    path: &Path,
    label: &str,
) -> Result<ExistingDirectoryChainGuard, String> {
    let absolute_path = absolute_lexical_path(path, label)?;
    validate_existing_directory_chain_no_links(&absolute_path)?;
    let handle = open_directory_no_links(&absolute_path, label)?;
    Ok(ExistingDirectoryChainGuard {
        absolute_path: absolute_path.clone(),
        entries: vec![(absolute_path, handle)],
    })
}

#[cfg(windows)]
fn open_guarded_regular_leaf(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
    let path = guard.absolute_path.join(leaf);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
        .open(&path)
        .map_err(|error| format!("open guarded {label} {}: {error}", path.display()))
}

#[cfg(windows)]
fn open_guarded_publication_leaf(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let path = guard.absolute_path.join(leaf);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
        .open(&path)
        .map_err(|error| format!("open guarded {label} {}: {error}", path.display()))
}

#[cfg(windows)]
fn open_guarded_removable_leaf(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let path = guard.absolute_path.join(leaf);
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
        .open(&path)
        .map_err(|error| format!("open removable guarded {label} {}: {error}", path.display()))
}

#[cfg(unix)]
fn open_guarded_regular_leaf(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<File, String> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(leaf.as_bytes())
        .map_err(|_| format!("{label} filename contains an embedded NUL"))?;
    let fd = unsafe {
        libc::openat(
            guard.parent_handle()?.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open guarded {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(any(unix, windows)))]
fn open_guarded_regular_leaf(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    label: &str,
) -> Result<File, String> {
    let path = guard.absolute_path.join(leaf);
    File::open(&path).map_err(|error| format!("open guarded {label} {}: {error}", path.display()))
}

fn stored_to_bundle_graph(
    stored: &StoredIdentityGraph,
    portable_roots: bool,
) -> Result<IdentityBundleGraph, String> {
    let roots = stored
        .roots
        .iter()
        .map(|root| PortableMatchRoot {
            root_id: root.root_id.clone(),
            portable_path: if portable_roots {
                portable_root_token(&root.root_id)
            } else {
                root.path.clone()
            },
            exclusions: root.exclusions.clone(),
            enabled: root.enabled,
            created_at: root.created_at.clone(),
            updated_at: root.updated_at.clone(),
        })
        .collect();
    let operations = if portable_roots {
        stored
            .operations
            .iter()
            .map(project_match_operation_for_portable)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stored.operations.clone()
    };
    let mut graph = IdentityBundleGraph {
        media_context: stored.media_context.clone(),
        video_observations: stored.video_observations.clone(),
        review_context: stored.review_context.clone(),
        people: stored.people.clone(),
        looks: stored.looks.clone(),
        trusted_template_sets: stored.trusted_template_sets.clone(),
        trusted_memberships: stored.trusted_memberships.clone(),
        faces: stored.faces.clone(),
        assignments: stored.assignments.clone(),
        suggestions: stored.suggestions.clone(),
        constraints: stored.constraints.clone(),
        dispositions: stored.dispositions.clone(),
        operations,
        correction_media_operations: stored.correction_media_operations.clone(),
        suggestion_source_provenance: stored.suggestion_source_provenance.clone(),
        roots,
        model_generations: stored.model_generations.clone(),
    };
    sort_bundle_graph(&mut graph);
    Ok(graph)
}

fn raw_vector_operation_rows(
    stored: &StoredIdentityGraph,
) -> Result<Vec<OwnedPortableRow>, String> {
    let mut rows = Vec::new();
    for operation in &stored.operations {
        if project_match_operation_for_portable(operation)? == *operation {
            continue;
        }
        rows.push(OwnedPortableRow {
            table: OPERATION_TABLE.to_string(),
            stable_id: operation.operation_id.clone(),
            value: serde_json::to_value(operation)
                .map_err(|error| format!("serialize raw recovery operation: {error}"))?,
        });
    }
    rows.sort_by(|left, right| left.stable_id.cmp(&right.stable_id));
    validate_raw_vector_operation_rows(&rows)?;
    Ok(rows)
}

fn charge_derived_recovery_bytes(current: usize, additional: usize) -> Result<usize, String> {
    let charged = current
        .checked_add(additional)
        .ok_or("identity import derived recovery byte count overflow")?;
    if charged > IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES {
        return Err(format!(
            "identity import derived recovery exceeds aggregate byte ceiling before materialization: {charged}/{IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES} bytes"
        ));
    }
    Ok(charged)
}

fn preflight_raw_vector_operation_rows_bytes(
    operations: &[MatchOperation],
    mut charged_bytes: usize,
) -> Result<usize, String> {
    for operation in operations {
        if project_match_operation_for_portable(operation)? == *operation {
            continue;
        }
        let row = OwnedPortableRow {
            table: OPERATION_TABLE.to_string(),
            stable_id: operation.operation_id.clone(),
            value: serde_json::to_value(operation)
                .map_err(|error| format!("size raw recovery operation: {error}"))?,
        };
        let row_bytes = serde_json::to_vec(&row)
            .map_err(|error| format!("size raw recovery operation row: {error}"))?
            .len();
        charged_bytes = charge_derived_recovery_bytes(
            charged_bytes,
            row_bytes
                .checked_add(1)
                .ok_or("identity import derived recovery byte count overflow")?,
        )?;
    }
    Ok(charged_bytes)
}

fn validate_raw_vector_operation_rows(rows: &[OwnedPortableRow]) -> Result<(), String> {
    enforce_limit(
        "raw recovery operation count",
        rows.len(),
        IDENTITY_BUNDLE_MAX_ENTITIES,
    )?;
    let mut ids = BTreeSet::new();
    for row in rows {
        let operation: MatchOperation =
            decode_strict_typed_value(&row.value, "raw recovery MatchOperation")?;
        if row.table != OPERATION_TABLE
            || row.stable_id != operation.operation_id
            || !ids.insert(row.stable_id.as_str())
            || project_match_operation_for_portable(&operation)? == operation
        {
            return Err(
                "identity import raw recovery operation is noncanonical or unnecessary".to_string(),
            );
        }
    }
    Ok(())
}

fn restore_raw_vector_operation_rows(
    desired_rows: &mut [OwnedPortableRow],
    raw_rows: &[OwnedPortableRow],
) -> Result<(), String> {
    validate_raw_vector_operation_rows(raw_rows)?;
    let desired_by_id = desired_rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.table == OPERATION_TABLE)
        .map(|(index, row)| (row.stable_id.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for raw in raw_rows {
        let index = desired_by_id.get(&raw.stable_id).copied().ok_or_else(|| {
            format!(
                "raw recovery operation {} is absent from the portable recovery graph",
                raw.stable_id
            )
        })?;
        let operation: MatchOperation =
            decode_strict_typed_value(&raw.value, "raw recovery MatchOperation")?;
        let projected = serde_json::to_value(project_match_operation_for_portable(&operation)?)
            .map_err(|error| format!("serialize projected recovery operation: {error}"))?;
        if projected != desired_rows[index].value {
            return Err(format!(
                "raw recovery operation {} does not project to its portable recovery row",
                raw.stable_id
            ));
        }
        desired_rows[index].value = raw.value.clone();
    }
    Ok(())
}

fn sort_stored_graph(graph: &mut StoredIdentityGraph) {
    graph
        .media_context
        .sort_by(|a, b| a.media_key.cmp(&b.media_key));
    graph
        .video_observations
        .sort_by(|a, b| a.observation_id.cmp(&b.observation_id));
    graph
        .review_context
        .sort_by(|a, b| a.context_id.cmp(&b.context_id));
    graph.people.sort_by(|a, b| a.person_id.cmp(&b.person_id));
    graph.looks.sort_by(|a, b| a.look_id.cmp(&b.look_id));
    graph
        .trusted_template_sets
        .sort_by(|a, b| a.set_id.cmp(&b.set_id));
    graph
        .trusted_memberships
        .sort_by(|a, b| a.membership_id.cmp(&b.membership_id));
    graph.faces.sort_by(|a, b| a.face_id.cmp(&b.face_id));
    graph
        .assignments
        .sort_by(|a, b| a.assignment_id.cmp(&b.assignment_id));
    graph
        .suggestions
        .sort_by(|a, b| a.suggestion_id.cmp(&b.suggestion_id));
    graph
        .constraints
        .sort_by(|a, b| a.constraint_id.cmp(&b.constraint_id));
    graph.dispositions.sort_by(|a, b| a.face_id.cmp(&b.face_id));
    graph
        .operations
        .sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    graph
        .correction_media_operations
        .sort_by(|a, b| a.mapping_id.cmp(&b.mapping_id));
    graph
        .suggestion_source_provenance
        .sort_by(|a, b| a.provenance_id.cmp(&b.provenance_id));
    graph.roots.sort_by(|a, b| a.root_id.cmp(&b.root_id));
    graph
        .model_generations
        .sort_by(|a, b| a.generation.cmp(&b.generation));
}

fn sort_bundle_graph(graph: &mut IdentityBundleGraph) {
    graph
        .media_context
        .sort_by(|a, b| a.media_key.cmp(&b.media_key));
    graph
        .video_observations
        .sort_by(|a, b| a.observation_id.cmp(&b.observation_id));
    graph
        .review_context
        .sort_by(|a, b| a.context_id.cmp(&b.context_id));
    graph.people.sort_by(|a, b| a.person_id.cmp(&b.person_id));
    graph.looks.sort_by(|a, b| a.look_id.cmp(&b.look_id));
    graph
        .trusted_template_sets
        .sort_by(|a, b| a.set_id.cmp(&b.set_id));
    graph
        .trusted_memberships
        .sort_by(|a, b| a.membership_id.cmp(&b.membership_id));
    graph.faces.sort_by(|a, b| a.face_id.cmp(&b.face_id));
    graph
        .assignments
        .sort_by(|a, b| a.assignment_id.cmp(&b.assignment_id));
    graph
        .suggestions
        .sort_by(|a, b| a.suggestion_id.cmp(&b.suggestion_id));
    graph
        .constraints
        .sort_by(|a, b| a.constraint_id.cmp(&b.constraint_id));
    graph.dispositions.sort_by(|a, b| a.face_id.cmp(&b.face_id));
    graph
        .operations
        .sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    graph
        .correction_media_operations
        .sort_by(|a, b| a.mapping_id.cmp(&b.mapping_id));
    graph
        .suggestion_source_provenance
        .sort_by(|a, b| a.provenance_id.cmp(&b.provenance_id));
    graph.roots.sort_by(|a, b| a.root_id.cmp(&b.root_id));
    graph
        .model_generations
        .sort_by(|a, b| a.generation.cmp(&b.generation));
}

fn stored_graph_rows(graph: &StoredIdentityGraph) -> Result<Vec<OwnedPortableRow>, String> {
    let portable = stored_to_bundle_graph(graph, false)?;
    bundle_graph_rows(&portable)
}

/// Comparison view for a portable import. Local correction history retains
/// full vectors for supported undo, while the portable graph deliberately
/// projects those vectors to explicit `vector_omitted` metadata. Comparing
/// this view prevents a self-import from treating that intentional projection
/// as a conflict or from overwriting the richer local operation row.
fn stored_graph_portable_comparison_rows(
    graph: &StoredIdentityGraph,
) -> Result<Vec<OwnedPortableRow>, String> {
    let mut portable = stored_to_bundle_graph(graph, false)?;
    portable.operations = portable
        .operations
        .iter()
        .map(project_match_operation_for_portable)
        .collect::<Result<Vec<_>, _>>()?;
    bundle_graph_rows(&portable)
}

fn bundle_graph_rows(graph: &IdentityBundleGraph) -> Result<Vec<OwnedPortableRow>, String> {
    let mut rows = Vec::with_capacity(graph_entity_count(graph));
    macro_rules! rows {
        ($values:expr, $table:expr, $id_field:ident) => {
            for row in $values {
                rows.push(OwnedPortableRow {
                    table: $table.to_string(),
                    stable_id: row.$id_field.clone(),
                    value: serde_json::to_value(row)
                        .map_err(|error| format!("serialize identity row: {error}"))?,
                });
            }
        };
    }
    rows!(&graph.people, PERSON_TABLE, person_id);
    rows!(
        &graph.media_context,
        super::context::MEDIA_CONTEXT_TABLE,
        media_key
    );
    rows!(
        &graph.video_observations,
        super::video::VIDEO_OBSERVATION_TABLE,
        observation_id
    );
    rows!(
        &graph.review_context,
        super::context::CONTEXT_TABLE,
        context_id
    );
    rows!(&graph.looks, LOOK_TABLE, look_id);
    rows!(&graph.trusted_template_sets, TEMPLATE_SET_TABLE, set_id);
    rows!(
        &graph.trusted_memberships,
        TRUSTED_MEMBER_TABLE,
        membership_id
    );
    rows!(&graph.faces, FACE_TABLE, face_id);
    // Match assignment records are keyed by FaceId, regardless of their
    // mirrored assignment_id field.
    rows!(&graph.assignments, ASSIGNMENT_TABLE, face_id);
    rows!(&graph.suggestions, SUGGESTION_TABLE, suggestion_id);
    rows!(&graph.constraints, CONSTRAINT_TABLE, constraint_id);
    rows!(&graph.dispositions, FACE_DISPOSITION_TABLE, face_id);
    rows!(&graph.operations, OPERATION_TABLE, operation_id);
    rows!(
        &graph.correction_media_operations,
        CORRECTION_MEDIA_OPERATION_TABLE,
        mapping_id
    );
    rows!(
        &graph.suggestion_source_provenance,
        SUGGESTION_SOURCE_PROVENANCE_TABLE,
        provenance_id
    );
    for root in &graph.roots {
        let stored = MatchIndexRoot {
            root_id: root.root_id.clone(),
            path: root.portable_path.clone(),
            exclusions: root.exclusions.clone(),
            enabled: root.enabled,
            created_at: root.created_at.clone(),
            updated_at: root.updated_at.clone(),
        };
        rows.push(OwnedPortableRow {
            table: ROOT_CONFIG_TABLE.to_string(),
            stable_id: root.root_id.clone(),
            value: serde_json::to_value(stored).map_err(|error| error.to_string())?,
        });
    }
    rows!(&graph.model_generations, GENERATION_TABLE, generation);
    rows.sort_by(|left, right| {
        (&left.table, &left.stable_id).cmp(&(&right.table, &right.stable_id))
    });
    Ok(rows)
}

fn row_map(rows: &[OwnedPortableRow]) -> BTreeMap<(String, String), OwnedPortableRow> {
    rows.iter()
        .cloned()
        .map(|row| ((row.table.clone(), row.stable_id.clone()), row))
        .collect()
}

fn portable_rows_digest(rows: &[OwnedPortableRow]) -> Result<String, String> {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| (&a.table, &a.stable_id).cmp(&(&b.table, &b.stable_id)));
    let bytes = serde_json::to_vec(&rows).map_err(|error| error.to_string())?;
    Ok(sha256_bytes(&bytes))
}

fn mutation_rows(
    current: &[OwnedPortableRow],
    desired: &[OwnedPortableRow],
    mode: IdentityImportMode,
) -> Result<(Vec<OwnedPortableRow>, Vec<OwnedPortableRow>, usize, usize), String> {
    let current = row_map(current);
    let desired = row_map(desired);
    let mut upserts = Vec::new();
    let mut created = 0;
    let mut updated = 0;
    for (key, row) in &desired {
        match current.get(key) {
            None => {
                created += 1;
                upserts.push(row.clone());
            }
            Some(existing) if existing.value == row.value => {}
            Some(_) if mode == IdentityImportMode::Replace => {
                updated += 1;
                upserts.push(row.clone());
            }
            Some(_) => return Err("merge mutation contains an unresolved conflict".to_string()),
        }
    }
    let deletes = if mode == IdentityImportMode::Replace {
        current
            .into_iter()
            .filter_map(|(key, row)| (!desired.contains_key(&key)).then_some(row))
            .collect()
    } else {
        Vec::new()
    };
    Ok((upserts, deletes, created, updated))
}

fn rows_equal(
    observed: &[OwnedPortableRow],
    desired: &[OwnedPortableRow],
    mode: IdentityImportMode,
) -> bool {
    let observed = row_map(observed);
    let desired = row_map(desired);
    if mode == IdentityImportMode::Replace {
        observed == desired
    } else {
        desired.iter().all(|(key, row)| {
            observed
                .get(key)
                .is_some_and(|value| value.value == row.value)
        })
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const MWG_RS_NS: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
const ST_AREA_NS: &str = "http://ns.adobe.com/xmp/sType/Area#";
const ST_DIM_NS: &str = "http://ns.adobe.com/xap/1.0/sType/Dimensions#";
const FACIAL_XMP_NS: &str = "https://facial.local/ns/match/1.0/";
const XMP_PREFIX: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:mwg-rs=\"http://www.metadataworkinggroup.com/schemas/regions/\" xmlns:stArea=\"http://ns.adobe.com/xmp/sType/Area#\" xmlns:stDim=\"http://ns.adobe.com/xap/1.0/sType/Dimensions#\" xmlns:facial=\"https://facial.local/ns/match/1.0/\"><rdf:RDF><rdf:Description><mwg-rs:Regions rdf:parseType=\"Resource\">";
const XMP_REGION_LIST_PREFIX: &str = "<mwg-rs:RegionList><rdf:Bag>";
const XMP_SUFFIX: &str =
    "</rdf:Bag></mwg-rs:RegionList></mwg-rs:Regions></rdf:Description></rdf:RDF></x:xmpmeta>";
const XMP_ITEM_PREFIX: &str = "<rdf:li rdf:parseType=\"Resource\">";
const XMP_ITEM_SUFFIX: &str = "</rdf:li>";

fn validate_xmp_sidecar_path(path: &Path, must_exist: bool) -> Result<(), String> {
    if path.extension().and_then(|value| value.to_str()) != Some("xmp") {
        return Err(
            "MWG interoperability target must have the lowercase .xmp sidecar extension"
                .to_string(),
        );
    }
    let leaf = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "MWG XMP sidecar filename must be valid UTF-8".to_string())?;
    validate_text("MWG XMP sidecar filename", leaf)?;
    validate_portable_file_leaf("MWG XMP sidecar", std::ffi::OsStr::new(leaf))?;
    let absolute = absolute_lexical_path(path, "MWG XMP sidecar path")?;
    let parent = absolute
        .parent()
        .ok_or_else(|| "XMP sidecar has no parent directory".to_string())?;
    let guard = guard_existing_directory_chain(parent, "MWG XMP sidecar directory")?;
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse_point(&metadata)
                || !metadata.is_file()
            {
                return Err("MWG XMP sidecar must be a regular non-symlink file".to_string());
            }
            if !must_exist {
                return Err("refusing to overwrite an existing MWG XMP sidecar".to_string());
            }
            fs::canonicalize(&absolute).map_err(|error| {
                format!(
                    "canonicalize MWG XMP sidecar {}: {error}",
                    absolute.display()
                )
            })?;
            let opened = open_guarded_regular_leaf(
                &guard,
                absolute
                    .file_name()
                    .ok_or_else(|| "XMP sidecar has no filename".to_string())?,
                "MWG XMP sidecar",
            )?;
            if !opened
                .metadata()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err("MWG XMP sidecar must be a regular file".to_string());
            }
        }
        Err(error) if must_exist => {
            return Err(format!(
                "inspect MWG XMP sidecar {}: {error}",
                path.display()
            ))
        }
        Err(_) => {}
    }
    guard.revalidate("MWG XMP sidecar directory")?;
    Ok(())
}

fn render_mwg_xmp(
    regions: &[MwgXmpRegion],
    applied_dimensions: Option<[u32; 2]>,
) -> Result<Vec<u8>, String> {
    let estimated = XMP_PREFIX
        .len()
        .saturating_add(XMP_SUFFIX.len())
        .saturating_add(regions.len().saturating_mul(384));
    let mut text = String::with_capacity(estimated.min(IDENTITY_XMP_MAX_SIDECAR_BYTES));
    push_xmp_fragment(&mut text, XMP_PREFIX)?;
    if let Some([width, height]) = applied_dimensions {
        if width == 0 || height == 0 {
            return Err("MWG AppliedToDimensions must be positive".to_string());
        }
        push_xmp_fragment(&mut text, "<mwg-rs:AppliedToDimensions stDim:w=\"")?;
        push_xmp_fragment(&mut text, &width.to_string())?;
        push_xmp_fragment(&mut text, "\" stDim:h=\"")?;
        push_xmp_fragment(&mut text, &height.to_string())?;
        push_xmp_fragment(&mut text, "\" stDim:unit=\"pixel\"/>")?;
    }
    push_xmp_fragment(&mut text, XMP_REGION_LIST_PREFIX)?;
    for region in regions {
        validate_text("MWG region FaceId", &region.face_id)?;
        validate_text("MWG region PersonId", &region.person_id)?;
        validate_text("MWG region name", &region.name)?;
        let [left, top, width, height] = region.bounds_normalized;
        if [left, top, width, height]
            .into_iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || width <= 0.0
            || height <= 0.0
            || left + width > 1.0
            || top + height > 1.0
        {
            return Err(format!(
                "MWG region {} has invalid normalized bounds",
                region.face_id
            ));
        }
        let center_x = left + width / 2.0;
        let center_y = top + height / 2.0;
        push_xmp_fragment(&mut text, XMP_ITEM_PREFIX)?;
        push_xml_element(&mut text, "mwg-rs:Type", "Face")?;
        push_xml_element(&mut text, "mwg-rs:Name", &region.name)?;
        push_xml_element(&mut text, "facial:FaceId", &region.face_id)?;
        push_xml_element(&mut text, "facial:PersonId", &region.person_id)?;
        push_xmp_fragment(&mut text, "<mwg-rs:Area stArea:x=\"")?;
        push_xmp_fragment(&mut text, &canonical_f32(center_x))?;
        push_xmp_fragment(&mut text, "\" stArea:y=\"")?;
        push_xmp_fragment(&mut text, &canonical_f32(center_y))?;
        push_xmp_fragment(&mut text, "\" stArea:w=\"")?;
        push_xmp_fragment(&mut text, &canonical_f32(width))?;
        push_xmp_fragment(&mut text, "\" stArea:h=\"")?;
        push_xmp_fragment(&mut text, &canonical_f32(height))?;
        push_xmp_fragment(&mut text, "\" stArea:unit=\"normalized\"/>")?;
        push_xmp_fragment(&mut text, XMP_ITEM_SUFFIX)?;
    }
    push_xmp_fragment(&mut text, XMP_SUFFIX)?;
    Ok(text.into_bytes())
}

fn push_xmp_fragment(output: &mut String, value: &str) -> Result<(), String> {
    let next = output
        .len()
        .checked_add(value.len())
        .ok_or_else(|| "MWG XMP byte count overflow".to_string())?;
    if next > IDENTITY_XMP_MAX_SIDECAR_BYTES {
        return Err(format!(
            "MWG XMP exceeds {IDENTITY_XMP_MAX_SIDECAR_BYTES} bytes"
        ));
    }
    output.push_str(value);
    Ok(())
}

fn push_xml_element(output: &mut String, tag: &str, value: &str) -> Result<(), String> {
    push_xmp_fragment(output, "<")?;
    push_xmp_fragment(output, tag)?;
    push_xmp_fragment(output, ">")?;
    push_xmp_fragment(output, &xml_escape(value))?;
    push_xmp_fragment(output, "</")?;
    push_xmp_fragment(output, tag)?;
    push_xmp_fragment(output, ">")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn canonical_f32(value: f32) -> String {
    let mut text = format!("{value:.9}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.push('0');
    }
    text
}

struct ParsedMwgXmp {
    regions: Vec<MwgXmpRegion>,
    unsupported: Vec<XmpUnsupportedSemantic>,
}

fn parse_mwg_xmp(bytes: &[u8]) -> Result<ParsedMwgXmp, String> {
    if bytes.len() > IDENTITY_XMP_MAX_SIDECAR_BYTES {
        return Err(format!(
            "MWG XMP exceeds {IDENTITY_XMP_MAX_SIDECAR_BYTES} bytes"
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("MWG XMP is not UTF-8: {error}"))?;
    if text.contains("<!DOCTYPE") || text.contains("<!ENTITY") || text.contains("<![CDATA[") {
        return Err("MWG XMP DTDs, declared entities, and CDATA are unsupported".to_string());
    }
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: IDENTITY_BUNDLE_MAX_REFERENCES as u32,
        entity_resolver: None,
    };
    let document = roxmltree::Document::parse_with_options(text, options)
        .map_err(|error| format!("decode MWG XMP XML: {error}"))?;
    for node in document.descendants() {
        if node.ancestors().count() > IDENTITY_BUNDLE_MAX_NESTING_DEPTH {
            return Err(format!(
                "MWG XMP nesting exceeds {IDENTITY_BUNDLE_MAX_NESTING_DEPTH}"
            ));
        }
        if let Some(pi) = node.pi() {
            if pi.target != "xpacket" {
                return Err(format!(
                    "MWG XMP processing instruction {} is unsupported",
                    pi.target
                ));
            }
        }
    }
    let regions_nodes = document
        .descendants()
        .filter(|node| node.has_tag_name((MWG_RS_NS, "Regions")))
        .collect::<Vec<_>>();
    if regions_nodes.len() != 1 {
        return Err("MWG XMP must contain exactly one mwg-rs:Regions value".to_string());
    }
    let applied_dimensions = element_children(regions_nodes[0], MWG_RS_NS, "AppliedToDimensions");
    if applied_dimensions.len() > 1 {
        return Err("MWG XMP repeats AppliedToDimensions".to_string());
    }
    if let Some(dimensions) = applied_dimensions.first() {
        if dimensions.attribute((ST_DIM_NS, "unit")) != Some("pixel") {
            return Err("MWG XMP AppliedToDimensions unit must be pixel".to_string());
        }
        for field in ["w", "h"] {
            let value = dimensions
                .attribute((ST_DIM_NS, field))
                .ok_or_else(|| format!("MWG XMP AppliedToDimensions omits stDim:{field}"))?
                .parse::<f64>()
                .map_err(|error| {
                    format!("MWG XMP AppliedToDimensions stDim:{field} is invalid: {error}")
                })?;
            if !value.is_finite() || value <= 0.0 || value > u32::MAX as f64 {
                return Err(format!(
                    "MWG XMP AppliedToDimensions stDim:{field} must be a positive finite image dimension"
                ));
            }
        }
    }
    let region_lists = element_children(regions_nodes[0], MWG_RS_NS, "RegionList");
    if region_lists.len() != 1 {
        return Err("MWG XMP Regions must contain exactly one RegionList".to_string());
    }
    let containers = region_lists[0]
        .children()
        .filter(|node| {
            node.is_element()
                && (node.has_tag_name((RDF_NS, "Bag")) || node.has_tag_name((RDF_NS, "Seq")))
        })
        .collect::<Vec<_>>();
    if containers.len() != 1 {
        return Err("MWG XMP RegionList must contain exactly one rdf:Bag or rdf:Seq".to_string());
    }
    let sidecar_sha256 = sha256_bytes(bytes);
    let mut regions = Vec::new();
    let mut unsupported = Vec::new();
    for (node, children, attributes) in [
        (
            regions_nodes[0],
            &[
                (MWG_RS_NS, "AppliedToDimensions"),
                (MWG_RS_NS, "RegionList"),
            ][..],
            &[][..],
        ),
        (
            region_lists[0],
            &[(RDF_NS, "Bag"), (RDF_NS, "Seq")][..],
            &[][..],
        ),
        (containers[0], &[(RDF_NS, "li")][..], &[][..]),
    ] {
        if xmp_has_unprojected_properties(node, children, attributes) {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_structure_property_not_projected".to_string(),
                stable_id: node.tag_name().name().to_string(),
            });
        }
    }
    if let Some(dimensions) = applied_dimensions.first() {
        if xmp_has_unprojected_properties(
            *dimensions,
            &[],
            &[(ST_DIM_NS, "w"), (ST_DIM_NS, "h"), (ST_DIM_NS, "unit")],
        ) {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_structure_property_not_projected".to_string(),
                stable_id: "AppliedToDimensions".to_string(),
            });
        }
    }
    let mut aggregate_string_bytes = 0usize;
    for (ordinal, item) in containers[0]
        .children()
        .filter(|node| node.has_tag_name((RDF_NS, "li")))
        .enumerate()
    {
        if ordinal >= IDENTITY_BUNDLE_MAX_ENTITIES {
            return Err("MWG XMP region count exceeds the identity entity limit".to_string());
        }
        let descriptions = element_children(item, RDF_NS, "Description");
        if descriptions.len() > 1 {
            return Err(format!("MWG XMP region-{ordinal} repeats rdf:Description"));
        }
        let resource = descriptions.first().copied().unwrap_or(item);
        let region_id = format!("region-{ordinal}");
        if resource != item && xmp_has_unprojected_properties(item, &[(RDF_NS, "Description")], &[])
        {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_region_wrapper_property_not_projected".to_string(),
                stable_id: region_id.clone(),
            });
        }
        let region_type = unique_xmp_scalar_or_attribute(resource, MWG_RS_NS, "Type")?;
        if region_type.as_deref() != Some("Face") {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_region_type_not_face".to_string(),
                stable_id: region_id,
            });
            continue;
        }
        let Some(name) = unique_xmp_scalar_or_attribute(resource, MWG_RS_NS, "Name")? else {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_face_region_missing_name".to_string(),
                stable_id: region_id,
            });
            continue;
        };
        validate_text("MWG region name", &name)?;
        add_string_bytes(&name, &mut aggregate_string_bytes)?;
        let areas = element_children(resource, MWG_RS_NS, "Area");
        if areas.len() != 1 {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_face_region_missing_rectangular_area".to_string(),
                stable_id: region_id,
            });
            continue;
        }
        let area = areas[0];
        if xmp_has_unprojected_properties(
            area,
            &[],
            &[
                (ST_AREA_NS, "x"),
                (ST_AREA_NS, "y"),
                (ST_AREA_NS, "w"),
                (ST_AREA_NS, "h"),
                (ST_AREA_NS, "unit"),
            ],
        ) {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_area_property_not_projected".to_string(),
                stable_id: region_id.clone(),
            });
        }
        let geometry = ["x", "y", "w", "h"].map(|field| area.attribute((ST_AREA_NS, field)));
        if geometry.iter().any(|value| value.is_none()) {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_face_region_missing_rectangular_area".to_string(),
                stable_id: region_id,
            });
            continue;
        }
        if area.attribute((ST_AREA_NS, "unit")) != Some("normalized") {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_face_region_unit_not_normalized".to_string(),
                stable_id: region_id,
            });
            continue;
        }
        let parse_geometry = |index: usize, label: &str| {
            geometry[index]
                .expect("geometry presence checked above")
                .parse::<f32>()
                .map_err(|error| format!("MWG XMP {label} is not a float: {error}"))
        };
        let center_x = parse_geometry(0, "stArea:x")?;
        let center_y = parse_geometry(1, "stArea:y")?;
        let width = parse_geometry(2, "stArea:w")?;
        let height = parse_geometry(3, "stArea:h")?;
        let left = center_x - width / 2.0;
        let top = center_y - height / 2.0;
        let face_id = unique_xmp_scalar(resource, FACIAL_XMP_NS, "FaceId")?.unwrap_or_else(|| {
            xmp_staging_id(
                "xmp-face",
                &(
                    "facial-xmp-face-hint-v1",
                    &sidecar_sha256,
                    ordinal,
                    &name,
                    canonical_f32(center_x),
                    canonical_f32(center_y),
                    canonical_f32(width),
                    canonical_f32(height),
                ),
            )
        });
        let person_id =
            unique_xmp_scalar(resource, FACIAL_XMP_NS, "PersonId")?.unwrap_or_else(|| {
                xmp_staging_id(
                    "xmp-person",
                    &("facial-xmp-person-hint-v1", &sidecar_sha256, &name),
                )
            });
        validate_text("MWG region FaceId", &face_id)?;
        validate_text("MWG region PersonId", &person_id)?;
        add_string_bytes(&face_id, &mut aggregate_string_bytes)?;
        add_string_bytes(&person_id, &mut aggregate_string_bytes)?;
        let has_unprojected_child =
            resource
                .children()
                .filter(|node| node.is_element())
                .any(|node| {
                    let name = node.tag_name();
                    !matches!(
                        (name.namespace(), name.name()),
                        (Some(MWG_RS_NS), "Type")
                            | (Some(MWG_RS_NS), "Name")
                            | (Some(MWG_RS_NS), "Area")
                            | (Some(FACIAL_XMP_NS), "FaceId")
                            | (Some(FACIAL_XMP_NS), "PersonId")
                    )
                });
        let has_unprojected_attribute = resource.attributes().any(|attribute| {
            !matches!(
                (attribute.namespace(), attribute.name()),
                (Some(MWG_RS_NS), "Type")
                    | (Some(MWG_RS_NS), "Name")
                    | (Some(RDF_NS), "about")
                    | (Some(RDF_NS), "parseType")
            )
        });
        if has_unprojected_child || has_unprojected_attribute {
            unsupported.push(XmpUnsupportedSemantic {
                code: "xmp_region_property_not_projected".to_string(),
                stable_id: region_id.clone(),
            });
        }
        let region = MwgXmpRegion {
            face_id,
            person_id,
            name,
            bounds_normalized: [left, top, width, height],
        };
        // Reuse writer validation for finite and bounded coordinates.
        render_mwg_xmp(std::slice::from_ref(&region), None)?;
        regions.push(region);
    }
    regions.sort_by(|left, right| left.face_id.cmp(&right.face_id));
    unique_ids(
        "MWG XMP FaceId",
        regions.iter().map(|row| row.face_id.as_str()),
    )?;
    Ok(ParsedMwgXmp {
        regions,
        unsupported,
    })
}

fn xmp_has_unprojected_properties(
    node: roxmltree::Node<'_, '_>,
    children: &[(&str, &str)],
    attributes: &[(&str, &str)],
) -> bool {
    node.children().any(|child| {
        if child.is_element() {
            let tag = child.tag_name();
            !children
                .iter()
                .any(|(namespace, name)| tag.namespace() == Some(*namespace) && tag.name() == *name)
        } else {
            child.is_text() && child.text().is_some_and(|text| !text.trim().is_empty())
        }
    }) || node.attributes().any(|attribute| {
        !matches!(
            (attribute.namespace(), attribute.name()),
            (Some(RDF_NS), "about") | (Some(RDF_NS), "parseType")
        ) && !attributes.iter().any(|(namespace, name)| {
            attribute.namespace() == Some(*namespace) && attribute.name() == *name
        })
    })
}

fn element_children<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    namespace: &str,
    local_name: &str,
) -> Vec<roxmltree::Node<'a, 'input>> {
    node.children()
        .filter(|child| child.has_tag_name((namespace, local_name)))
        .collect()
}

fn unique_xmp_scalar(
    node: roxmltree::Node<'_, '_>,
    namespace: &str,
    local_name: &str,
) -> Result<Option<String>, String> {
    let values = element_children(node, namespace, local_name);
    if values.len() > 1 {
        return Err(format!("MWG XMP repeats {local_name}"));
    }
    values
        .first()
        .map(|value| {
            if value.attributes().len() != 0 || value.children().any(|child| !child.is_text()) {
                return Err(format!(
                    "MWG XMP {local_name} uses a complex non-scalar representation"
                ));
            }
            let scalar = value
                .children()
                .filter_map(|child| child.text())
                .collect::<String>();
            if scalar.is_empty() {
                Err(format!("MWG XMP {local_name} has no scalar value"))
            } else {
                Ok(scalar)
            }
        })
        .transpose()
}

fn unique_xmp_scalar_or_attribute(
    node: roxmltree::Node<'_, '_>,
    namespace: &str,
    local_name: &str,
) -> Result<Option<String>, String> {
    let child = unique_xmp_scalar(node, namespace, local_name)?;
    let attribute = node.attribute((namespace, local_name)).map(str::to_string);
    if child.is_some() && attribute.is_some() {
        return Err(format!(
            "MWG XMP {local_name} uses ambiguous child and attribute representations"
        ));
    }
    Ok(child.or(attribute))
}

fn xmp_staging_id<T: Serialize>(prefix: &str, material: &T) -> String {
    let bytes = serde_json::to_vec(material).expect("XMP staging ID material is serializable");
    format!("{prefix}-{}", sha256_bytes(&bytes))
}

struct GuardedFileBytes {
    bytes: Vec<u8>,
    canonical_path: PathBuf,
    guard: File,
}

fn read_bounded_regular_file(path: &Path, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    Ok(read_bounded_regular_file_snapshot(path, limit, label)?.bytes)
}

fn read_bounded_regular_file_snapshot(
    path: &Path,
    limit: usize,
    label: &str,
) -> Result<GuardedFileBytes, String> {
    let absolute = absolute_lexical_path(path, label)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("{label} has no parent directory"))?;
    let leaf = absolute
        .file_name()
        .ok_or_else(|| format!("{label} has no filename"))?;
    validate_portable_file_leaf(label, leaf)?;
    let guard = guard_existing_directory_chain(parent, label)?;
    let canonical_path = fs::canonicalize(&absolute)
        .map_err(|error| format!("canonicalize {label} {}: {error}", absolute.display()))?;
    let path_metadata = fs::symlink_metadata(&absolute)
        .map_err(|error| format!("inspect {label} {}: {error}", absolute.display()))?;
    if path_metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&path_metadata)
        || !path_metadata.is_file()
    {
        return Err(format!("{label} must be a regular non-symlink file"));
    }
    let length =
        usize::try_from(path_metadata.len()).map_err(|_| format!("{label} is too large"))?;
    if length > limit {
        return Err(format!("{label} is {length} bytes; limit is {limit}"));
    }
    let mut file = open_guarded_regular_leaf(&guard, leaf, label)?;
    let handle_before = file
        .metadata()
        .map_err(|error| format!("inspect open {label} handle: {error}"))?;
    let path_probe_before = open_guarded_regular_leaf(&guard, leaf, label)?;
    if !handle_before.is_file()
        || handle_before.len() != path_metadata.len()
        || !same_open_file_identity(&file, &path_probe_before)?
    {
        return Err(format!("{label} path and open handle identity differ"));
    }
    guard.revalidate(label)?;
    let mut bytes = Vec::with_capacity(length.min(limit));
    (&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {label} {}: {error}", absolute.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{label} changed while reading and exceeded {limit} bytes"
        ));
    }
    let handle_after = file
        .metadata()
        .map_err(|error| format!("reinspect open {label} handle: {error}"))?;
    let path_after = fs::symlink_metadata(&absolute)
        .map_err(|error| format!("reinspect {label} {}: {error}", absolute.display()))?;
    let path_probe_after = open_guarded_regular_leaf(&guard, leaf, label)?;
    let stable_modified = handle_before.modified().ok() == handle_after.modified().ok()
        && handle_before.modified().ok() == path_after.modified().ok();
    if path_after.file_type().is_symlink()
        || metadata_is_reparse_point(&path_after)
        || !path_after.is_file()
        || handle_before.len() != handle_after.len()
        || handle_before.len() != path_after.len()
        || !same_open_file_identity(&file, &path_probe_after)?
        || !stable_modified
    {
        return Err(format!("{label} changed identity while reading"));
    }
    guard.revalidate(label)?;
    Ok(GuardedFileBytes {
        bytes,
        canonical_path,
        guard: file,
    })
}

#[derive(Debug)]
struct PublicationOutcome {
    warning: Option<String>,
}

/// Recovery publication is a hard predecessor for destructive Match recovery.
/// A path can be byte-correct yet still lack durable directory/rename proof, so
/// ordinary export warnings are fatal on every recovery-copy surface.
fn publish_recovery_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    publish_durable_recovery_file(path, bytes, IDENTITY_BUNDLE_MAX_BYTES)
}

/// Publish an immutable recovery artifact through the same descriptor/handle-
/// confined create-new path used by identity recovery bundles. Recovery state
/// machines may use a different explicit byte ceiling, but durability warnings
/// remain fatal.
pub(super) fn publish_durable_recovery_file(
    path: &Path,
    bytes: &[u8],
    limit: usize,
) -> Result<(), String> {
    let publication = write_new_regular_file(path, bytes, limit)?;
    #[cfg(test)]
    if FAIL_NEXT_RECOVERY_PUBLICATION_AFTER_PUBLISH.swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(
            "injected recovery publication durability failure after final path commit".to_string(),
        );
    }
    require_durable_recovery_publication(publication)
}

/// Read a recovery state artifact while retaining all ancestor and leaf
/// anti-link/identity checks inside this module.
pub(super) fn read_guarded_recovery_file(
    path: &Path,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    Ok(read_bounded_regular_file_snapshot(path, limit, label)?.bytes)
}

fn require_durable_recovery_publication(publication: PublicationOutcome) -> Result<(), String> {
    match publication.warning {
        Some(warning) => Err(format!(
            "Match recovery publication did not prove durable commit: {warning}"
        )),
        None => Ok(()),
    }
}

fn write_new_regular_file(
    path: &Path,
    bytes: &[u8],
    limit: usize,
) -> Result<PublicationOutcome, String> {
    if bytes.len() > limit {
        return Err(format!("output exceeds {limit} bytes"));
    }
    let absolute = absolute_lexical_path(path, "output path")?;
    if fs::symlink_metadata(&absolute).is_ok() {
        return Err(format!(
            "refusing to overwrite existing output {}",
            absolute.display()
        ));
    }
    let parent = absolute
        .parent()
        .ok_or_else(|| "output path has no parent".to_string())?;
    let guard = guard_existing_directory_chain(parent, "output directory")?;
    fs::canonicalize(parent).map_err(|error| {
        format!(
            "canonicalize output directory {}: {error}",
            parent.display()
        )
    })?;
    let leaf = absolute
        .file_name()
        .ok_or_else(|| "output path has no filename".to_string())?;
    let leaf_text = leaf
        .to_str()
        .ok_or_else(|| "output filename is not UTF-8".to_string())?;
    validate_text("output filename", leaf_text)?;
    validate_portable_file_leaf("output", leaf)?;
    guard.revalidate("output directory")?;
    publish_new_file_guarded(&guard, leaf, bytes)
}

fn verify_open_file_content(file: &mut File, expected: &[u8], label: &str) -> Result<(), String> {
    let observed_len = usize::try_from(
        file.metadata()
            .map_err(|error| format!("inspect {label}: {error}"))?
            .len(),
    )
    .map_err(|_| format!("{label} length exceeds usize"))?;
    if observed_len != expected.len() {
        return Err(format!("{label} length differs from the staged payload"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {label}: {error}"))?;
    let mut observed = Vec::with_capacity(expected.len());
    file.take((expected.len() + 1) as u64)
        .read_to_end(&mut observed)
        .map_err(|error| format!("read {label}: {error}"))?;
    if observed != expected || sha256_bytes(&observed) != sha256_bytes(expected) {
        return Err(format!("{label} content differs from the staged payload"));
    }
    Ok(())
}

#[cfg(test)]
static MUTATE_NEXT_STAGED_PUBLICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_RECOVERY_DIRECTORY_PARENT_SYNC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, windows))]
static FAIL_NEXT_RECOVERY_REMOVAL_AFTER_DELETE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, windows))]
pub(super) fn fail_next_recovery_removal_after_delete() {
    FAIL_NEXT_RECOVERY_REMOVAL_AFTER_DELETE.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
static FAIL_NEXT_RECOVERY_PUBLICATION_AFTER_PUBLISH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn fail_next_recovery_publication_after_publish() {
    FAIL_NEXT_RECOVERY_PUBLICATION_AFTER_PUBLISH.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
static REMOVE_NEXT_RESOLVED_MEDIA_BEFORE_IMPORT_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static REMOVE_NEXT_RESOLVED_MEDIA_AFTER_IMPORT_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_IDENTITY_RECONCILIATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn fail_next_identity_reconciliation() {
    FAIL_NEXT_IDENTITY_RECONCILIATION.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
static LEAVE_IMPORT_JOURNAL_PLAN_TOKENS: std::sync::OnceLock<std::sync::Mutex<BTreeSet<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
static FAIL_IMPORT_ROLLBACK_AFTER_PORTABLE_RESTORE_PLAN_TOKENS: std::sync::OnceLock<
    std::sync::Mutex<BTreeSet<String>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn inject_leave_import_journal_for_plan(plan_token: &str) {
    LEAVE_IMPORT_JOURNAL_PLAN_TOKENS
        .get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(plan_token.to_string());
}

#[cfg(test)]
fn leave_import_journal_for_plan(plan_token: &str) -> bool {
    LEAVE_IMPORT_JOURNAL_PLAN_TOKENS
        .get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(plan_token)
}

#[cfg(test)]
fn inject_import_rollback_failure_after_portable_restore(plan_token: &str) {
    FAIL_IMPORT_ROLLBACK_AFTER_PORTABLE_RESTORE_PLAN_TOKENS
        .get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(plan_token.to_string());
}

#[cfg(test)]
fn fail_import_rollback_after_portable_restore_for_plan(plan_token: &str) -> bool {
    FAIL_IMPORT_ROLLBACK_AFTER_PORTABLE_RESTORE_PLAN_TOKENS
        .get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(plan_token)
}

#[cfg(all(test, windows))]
static FAIL_NEXT_WINDOWS_POST_RENAME_FLUSH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn inject_staged_publication_mutation(file: &mut File, expected: &[u8]) -> Result<(), String> {
    if MUTATE_NEXT_STAGED_PUBLICATION.swap(false, std::sync::atomic::Ordering::SeqCst)
        && !expected.is_empty()
    {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("seek staged mutation injection: {error}"))?;
        file.write_all(&[expected[0] ^ 0xff])
            .map_err(|error| format!("write staged mutation injection: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync staged mutation injection: {error}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn flush_windows_publication_after_rename(file: &File, output: &Path) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

    #[cfg(test)]
    if FAIL_NEXT_WINDOWS_POST_RENAME_FLUSH.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err("injected Windows post-rename durability failure".to_string());
    }

    let flushed = unsafe { FlushFileBuffers(file.as_raw_handle() as _) };
    if flushed == 0 {
        return Err(format!(
            "flush published output rename metadata {}: {}",
            output.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn publish_new_file_guarded(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<PublicationOutcome, String> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileRenameInfo, SetFileInformationByHandle, DELETE,
        FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FILE_SHARE_READ,
    };
    let temp_leaf = format!(".facial-exchange-{}.tmp", uuid::Uuid::new_v4().simple());
    let temp = guard.absolute_path.join(&temp_leaf);
    let output = guard.absolute_path.join(leaf);
    let mut options = OpenOptions::new();
    options
        .create_new(true)
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut staged = options
        .open(&temp)
        .map_err(|error| format!("create staged output {}: {error}", temp.display()))?;
    let output_wide = output.as_os_str().encode_wide().collect::<Vec<_>>();
    let output_name_bytes = output_wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| "published output filename length overflow".to_string())?;
    let output_name_bytes = u32::try_from(output_name_bytes)
        .map_err(|_| "published output filename exceeds Windows rename limits".to_string())?;
    let rename_buffer_bytes = std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(
            output_wide
                .len()
                .saturating_sub(1)
                .checked_mul(std::mem::size_of::<u16>())
                .ok_or_else(|| "published output rename buffer length overflow".to_string())?,
        )
        .ok_or_else(|| "published output rename buffer length overflow".to_string())?;
    let rename_buffer_len = u32::try_from(rename_buffer_bytes)
        .map_err(|_| "published output rename buffer exceeds Windows limits".to_string())?;
    let mut moved = false;
    let mut committed = false;
    let result = (|| {
        staged
            .write_all(bytes)
            .map_err(|error| format!("write staged output {}: {error}", temp.display()))?;
        staged
            .sync_all()
            .map_err(|error| format!("sync staged output {}: {error}", temp.display()))?;
        #[cfg(test)]
        inject_staged_publication_mutation(&mut staged, bytes)?;
        verify_open_file_content(&mut staged, bytes, "staged output before publication")?;
        guard.revalidate("output directory before publication")?;
        let staged_probe =
            open_guarded_publication_leaf(guard, temp_leaf.as_ref(), "staged output")?;
        if !same_open_file_identity(&staged, &staged_probe)? {
            return Err("staged output changed identity before publication".to_string());
        }
        drop(staged_probe);
        let rename_slots = rename_buffer_bytes.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>());
        let mut rename_buffer =
            vec![std::mem::MaybeUninit::<FILE_RENAME_INFO>::zeroed(); rename_slots];
        let rename_info = rename_buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*rename_info).Anonymous.ReplaceIfExists = false;
            (*rename_info).RootDirectory = std::ptr::null_mut();
            (*rename_info).FileNameLength = output_name_bytes;
            std::ptr::copy_nonoverlapping(
                output_wide.as_ptr(),
                (*rename_info).FileName.as_mut_ptr(),
                output_wide.len(),
            );
        }
        let renamed = unsafe {
            SetFileInformationByHandle(
                staged.as_raw_handle() as _,
                FileRenameInfo,
                rename_info.cast(),
                rename_buffer_len,
            )
        };
        if renamed == 0 {
            return Err(format!(
                "publish output create-new without clobber {}: {}",
                output.display(),
                std::io::Error::last_os_error()
            ));
        }
        moved = true;
        // The staged contents were flushed before rename. Flush the retained
        // file handle again after the descriptor-bound, no-clobber rename so
        // Windows commits the renamed file's buffered data and metadata before
        // this publication can be reported as durable.
        flush_windows_publication_after_rename(&staged, &output)?;
        let mut published = open_guarded_publication_leaf(guard, leaf, "published output")?;
        if !same_open_file_identity(&staged, &published)? {
            return Err("published output identity differs from staged output".to_string());
        }
        verify_open_file_content(&mut published, bytes, "published output")?;
        guard.revalidate("output directory after publication")?;
        sync_guarded_directory(guard, "output directory after publication")?;
        committed = true;
        Ok(())
    })();
    if committed {
        drop(staged);
        debug_assert!(result.is_ok());
        return Ok(PublicationOutcome { warning: None });
    }
    let mut cleanup_warning = None;
    if !moved {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        let deleted = unsafe {
            SetFileInformationByHandle(
                staged.as_raw_handle() as _,
                FileDispositionInfo,
                (&raw const disposition).cast(),
                u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>())
                    .expect("FILE_DISPOSITION_INFO size fits u32"),
            )
        };
        if deleted == 0 {
            cleanup_warning = Some(format!(
                "failed to disposition-delete staged output {}: {}",
                temp.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    drop(staged);
    let retained_path_warning = if moved {
        format!(
            "unverified output may remain at {}; it was not deleted because final-name identity was not proven",
            output.display()
        )
    } else if let Some(cleanup_warning) = cleanup_warning {
        format!("; {cleanup_warning}")
    } else {
        String::new()
    };
    let error = result.unwrap_err();
    Err(format!("{error}{retained_path_warning}"))
}

#[cfg(unix)]
fn publish_new_file_guarded(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<PublicationOutcome, String> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let parent_fd = guard.parent_handle()?.as_raw_fd();
    let output_name = CString::new(leaf.as_bytes())
        .map_err(|_| "output filename contains an embedded NUL".to_string())?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let fd = unsafe {
        libc::openat(
            parent_fd,
            c".".as_ptr(),
            libc::O_RDWR | libc::O_TMPFILE | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let fd = {
        return Err(
            "descriptor-bound unnamed create-new publication is unsupported on this Unix target"
                .to_string(),
        );
    };
    if fd < 0 {
        return Err(format!(
            "create unnamed staged output: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let mut published = false;
    let mut committed = false;
    let result = (|| {
        staged
            .write_all(bytes)
            .map_err(|error| format!("write staged output: {error}"))?;
        staged
            .sync_all()
            .map_err(|error| format!("sync staged output: {error}"))?;
        #[cfg(test)]
        inject_staged_publication_mutation(&mut staged, bytes)?;
        verify_open_file_content(&mut staged, bytes, "staged output before publication")?;
        guard.revalidate("output directory before publication")?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let linked = {
            let empty = b"\0";
            let direct = unsafe {
                libc::linkat(
                    staged.as_raw_fd(),
                    empty.as_ptr().cast(),
                    parent_fd,
                    output_name.as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            if direct == 0 {
                0
            } else {
                // Unprivileged Linux may reject AT_EMPTY_PATH. /proc/self/fd
                // plus AT_SYMLINK_FOLLOW still binds the link source to the
                // retained unnamed descriptor, never an attacker-replaceable
                // staging pathname.
                let descriptor_path = CString::new(format!("/proc/self/fd/{}", staged.as_raw_fd()))
                    .expect("descriptor path contains no NUL");
                unsafe {
                    libc::linkat(
                        libc::AT_FDCWD,
                        descriptor_path.as_ptr(),
                        parent_fd,
                        output_name.as_ptr(),
                        libc::AT_SYMLINK_FOLLOW,
                    )
                }
            }
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let linked = {
            return Err(
                "descriptor-bound create-new publication is unsupported on this Unix target"
                    .to_string(),
            );
        };
        if linked != 0 {
            return Err(format!(
                "publish output create-new without clobber: {}",
                std::io::Error::last_os_error()
            ));
        }
        published = true;
        let mut published = open_guarded_regular_leaf(guard, leaf, "published output")?;
        if !same_open_file_identity(&staged, &published)? {
            return Err("published output identity differs from staged output".to_string());
        }
        verify_open_file_content(&mut published, bytes, "published output")?;
        guard.revalidate("output directory after publication")?;
        committed = true;
        Ok(())
    })();
    if committed {
        let mut warnings = Vec::new();
        if let Err(error) = result {
            warnings.push(format!("post-publication verification: {error}"));
        }
        match guard.parent_handle() {
            Ok(parent) => {
                if let Err(error) = parent.sync_all() {
                    warnings.push(format!("output-directory sync: {error}"));
                }
            }
            Err(error) => warnings.push(format!("output-directory sync handle: {error}")),
        }
        return Ok(PublicationOutcome {
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        });
    }
    let mut cleanup_failures = Vec::new();
    if published {
        cleanup_failures.push(
            "unverified output may remain in the retained output directory; it was not deleted because final-name identity was not proven"
                .to_string(),
        );
    }
    let error = result.unwrap_err();
    if cleanup_failures.is_empty() {
        Err(error)
    } else {
        Err(format!(
            "{error}; cleanup failures: {}",
            cleanup_failures.join("; ")
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn publish_new_file_guarded(
    guard: &ExistingDirectoryChainGuard,
    leaf: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<PublicationOutcome, String> {
    let path = guard.absolute_path.join(leaf);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("create output {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write output {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync output {}: {error}", path.display()))?;
    Ok(PublicationOutcome { warning: None })
}

fn validate_existing_directory_chain_no_links(path: &Path) -> Result<(), String> {
    let mut probe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => probe.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("output directory must not contain parent traversal".to_string())
            }
            Component::Normal(value) => {
                probe.push(value);
                let metadata = fs::symlink_metadata(&probe).map_err(|error| {
                    format!(
                        "inspect output directory ancestor {}: {error}",
                        probe.display()
                    )
                })?;
                if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
                    return Err(
                        "output directory must not traverse a symlink or reparse point".to_string(),
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn same_open_file_identity(left: &File, right: &File) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let left = left
        .metadata()
        .map_err(|error| format!("read first Unix file identity: {error}"))?;
    let right = right
        .metadata()
        .map_err(|error| format!("read second Unix file identity: {error}"))?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(windows)]
fn same_open_file_identity(left: &File, right: &File) -> Result<bool, String> {
    fn identity(file: &File) -> Result<(u32, u64), String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) };
        if succeeded == 0 {
            return Err(format!(
                "read Windows file identity: {}",
                std::io::Error::last_os_error()
            ));
        }
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        Ok((information.dwVolumeSerialNumber, file_index))
    }
    Ok(identity(left)? == identity(right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_open_file_identity(_left: &File, _right: &File) -> Result<bool, String> {
    Err("stable file identity is unsupported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wp086_portable_video_namespaces_reject_same_stream_forgery_and_disjoint_copy_ownership() {
        use crate::match_video::{
            VideoDetection, VideoFrame, VideoPolicy, VideoTime, VideoTracker,
        };
        let fingerprint = "a".repeat(64);
        let mut graph = IdentityBundleGraph::default();
        let add = |graph: &mut IdentityBundleGraph,
                   media: &str,
                   stream,
                   namespace: Option<String>,
                   pts: i64,
                   source_index| {
            let policy = VideoPolicy::default();
            let mut tracker = match namespace {
                Some(namespace) => {
                    VideoTracker::new_for_source(fingerprint.clone(), stream, policy, namespace)
                        .unwrap()
                }
                None => VideoTracker::new(fingerprint.clone(), stream, policy).unwrap(),
            };
            let frame = VideoFrame {
                stream_index: stream,
                playback_origin: VideoTime::default(),
                time: VideoTime {
                    pts,
                    ..VideoTime::default()
                },
                frame_sha256: "b".repeat(64),
                scene_score: 0.0,
                detections: vec![VideoDetection {
                    source_index: 0,
                    bounds: [0.1, 0.1, 0.3, 0.3],
                    quality: 0.9,
                    pose_bucket: "frontal".into(),
                    detector_generation: "fixture".into(),
                }],
            };
            if pts > 0 {
                let mut first = frame.clone();
                first.time = VideoTime::default();
                first.detections.clear();
                tracker.ingest(first).unwrap();
            }
            let observation = tracker.ingest(frame).unwrap().observations.remove(0);
            let timestamp = "2026-08-25T00:00:00Z".to_string();
            let face = FaceObservation {
                face_id: format!("video-face-{}", observation.observation_id),
                media_key: media.into(),
                media_fingerprint: fingerprint.clone(),
                source_index,
                source_width: None,
                source_height: None,
                exif_orientation: None,
                bounds_normalized: observation.detection.bounds.to_vec(),
                landmarks_normalized: Vec::new(),
                alignment_valid: false,
                quality: observation.detection.quality,
                pose_bucket: observation.detection.pose_bucket.clone(),
                operator_owned: false,
                schema_generation: MATCH_SCHEMA_GENERATION.into(),
                face_revision: 1,
                created_at: timestamp.clone(),
                updated_at: timestamp,
            };
            graph.video_observations.push(StoredVideoObservation {
                observation_id: observation.observation_id.clone(),
                face_id: face.face_id.clone(),
                track_id: observation.track_id.clone(),
                media_key: face.media_key.clone(),
                media_fingerprint: fingerprint.clone(),
                revision: 1,
                closed: true,
                exemplar: true,
                payload: serde_json::to_string(&observation).unwrap(),
            });
            graph.faces.push(face);
        };
        add(&mut graph, "video.mkv", 0, None, 0, 0);
        add(&mut graph, "video.mkv", 1, Some("c".repeat(64)), 0, 1);
        // A different physical stream may retain a different historical seed.
        validate_identity_graph(&graph).unwrap();
        let mut forged = graph.clone();
        add(&mut forged, "video.mkv", 0, Some("c".repeat(64)), 0, 2);
        assert!(validate_identity_graph(&forged)
            .unwrap_err()
            .contains("inconsistent identity namespaces"));
        let mut shared_copy = graph.clone();
        add(
            &mut shared_copy,
            "copy.mkv",
            1,
            Some("c".repeat(64)),
            500,
            0,
        );
        assert_eq!(
            shared_copy
                .faces
                .iter()
                .map(|face| &face.face_id)
                .collect::<BTreeSet<_>>()
                .len(),
            shared_copy.faces.len()
        );
        for row in &shared_copy.video_observations {
            row.observation().unwrap();
        }
        let mut resampled_copy = IdentityBundleGraph::default();
        add(
            &mut resampled_copy,
            "copy.mkv",
            1,
            Some("c".repeat(64)),
            0,
            0,
        );
        assert_eq!(
            resampled_copy.faces[0].face_id, graph.faces[1].face_id,
            "checkpointless overlapping resampling collides despite disjoint stored FaceIds"
        );
        assert!(validate_identity_graph(&shared_copy)
            .unwrap_err()
            .contains("belongs to multiple media sources"));
        let mut legacy_copy = graph.clone();
        add(&mut legacy_copy, "legacy-copy.mkv", 0, None, 500, 0);
        validate_identity_graph(&legacy_copy).unwrap();
        let root = TestRoot::new("video-namespace-import");
        for (graph, expected, file) in [
            (
                forged,
                "inconsistent identity namespaces",
                "canonical-forgery.json",
            ),
            (
                shared_copy,
                "belongs to multiple media sources",
                "shared-copy.json",
            ),
        ] {
            let bundle = IdentityBundleV1 {
                manifest: IdentityBundleManifest {
                    format: IDENTITY_BUNDLE_FORMAT.into(),
                    version: IDENTITY_BUNDLE_VERSION,
                    schema_version: MATCH_SCHEMA_VERSION,
                    schema_generation: MATCH_SCHEMA_GENERATION.into(),
                    content_sha256: bundle_content_sha256(&graph).unwrap(),
                },
                graph,
            };
            let input = root.0.join(file);
            fs::write(&input, canonical_bundle_bytes(&bundle).unwrap()).unwrap();
            assert!(MatchStore::read_identity_bundle(&input)
                .unwrap_err()
                .contains(expected));
        }
    }

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "facial-exchange-{label}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exact_owner_edge_accepts_equal_millisecond_reverse_uuid_order() {
        let instant = chrono::DateTime::parse_from_rfc3339("2026-08-25T00:00:00.123Z").unwrap();
        let producer_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
        let consumer_id = "00000000-0000-4000-8000-000000000000";
        assert!(producer_id > consumer_id);
        assert!(exact_owner_edge_precedes(&instant, &instant));
        assert!(!operation_precedes(
            &instant,
            producer_id,
            &instant,
            consumer_id
        ));
    }

    fn poison_identity_import_caches(store: &MatchStore) {
        let mut caches = store.caches.write().unwrap();
        caches.identity_revision = u64::MAX;
        caches.catalog_revision = u64::MAX;
        caches.projections.insert(
            "stale/import-cache-only.jpg".to_string(),
            PeopleProjection {
                media_key: "stale/import-cache-only.jpg".to_string(),
                media_fingerprint: "stale-import-cache-only".to_string(),
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                model_generation: "stale-import-cache-only".to_string(),
                identity_revision: u64::MAX,
                catalog_revision: u64::MAX,
                person_ids: vec!["stale-import-cache-person".to_string()],
                published_at: now(),
            },
        );
        caches.autocomplete = AutocompleteIndex {
            valid: true,
            catalog_revision: u64::MAX,
            entries: vec![AutocompleteEntry {
                person_id: "stale-import-cache-person".to_string(),
                display_name: "Stale Import Cache".to_string(),
                search: "stale import cache".to_string(),
            }],
        };
        caches.last_query_plan = Some(QueryPlanEvidence {
            observed: true,
            index: "stale-import-cache-only".to_string(),
            uses_hnsw: true,
            exact_rerank: true,
            model_generation: "stale-import-cache-only".to_string(),
            observed_at: now(),
        });
    }

    fn assert_identity_import_caches_fail_closed(store: &MatchStore) {
        let caches = store.caches.read().unwrap();
        assert_eq!(caches.identity_revision, 0);
        assert_eq!(caches.catalog_revision, 0);
        assert!(caches.projections.is_empty());
        assert!(!caches.autocomplete.valid);
        assert!(caches.autocomplete.entries.is_empty());
        assert!(caches.last_query_plan.is_none());
    }

    #[cfg(windows)]
    fn try_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(unix)]
    fn try_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[test]
    fn identity_import_receipt_samples_are_bounded_and_report_truncation() {
        let values = (0..(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 17)).collect::<Vec<_>>();
        let (sample, truncated) = bounded_receipt_sample(&values);
        assert_eq!(sample.len(), IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT);
        assert_eq!(sample.first(), Some(&0));
        assert_eq!(
            sample.last(),
            Some(&(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT - 1))
        );
        assert!(truncated);

        let (complete, truncated) = bounded_receipt_sample(&values[..3]);
        assert_eq!(complete, vec![0, 1, 2]);
        assert!(!truncated);

        let graph = IdentityBundleGraph::default();
        let content_sha256 = bundle_content_sha256(&graph).unwrap();
        let bundle = IdentityBundleV1 {
            manifest: IdentityBundleManifest {
                format: IDENTITY_BUNDLE_FORMAT.to_string(),
                version: IDENTITY_BUNDLE_VERSION,
                schema_version: MATCH_SCHEMA_VERSION,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                content_sha256: content_sha256.clone(),
            },
            graph: graph.clone(),
        };
        let conflicts = (0..(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 17))
            .map(|index| IdentityImportConflict {
                table: PERSON_TABLE.to_string(),
                stable_id: format!("person-{index}"),
            })
            .collect::<Vec<_>>();
        let unresolved_root_ids = (0..(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 23))
            .map(|index| format!("root-{index}"))
            .collect::<Vec<_>>();
        let unresolved_media_keys = (0..(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 29))
            .map(|index| format!("media/{index}.jpg"))
            .collect::<Vec<_>>();
        let mut plan = IdentityImportPlan {
            plan_token: "a".repeat(64),
            content_sha256,
            mode: IdentityImportMode::Replace,
            creates: 0,
            updates: 0,
            identical: 0,
            deletes: 0,
            conflicts,
            unresolved_root_ids,
            unresolved_media_keys,
            already_applied: false,
            entity_count: 0,
            reference_count: 0,
            bundle,
            relocated_graph: graph,
            relocations: BTreeMap::new(),
            relocated_media_sha256: BTreeMap::new(),
            current_state_sha256: "b".repeat(64),
        };
        let summary = plan.summary();
        assert_eq!(summary.sample_limit, IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT);
        assert_eq!(
            summary.conflict_count,
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 17
        );
        assert_eq!(
            summary.unresolved_root_count,
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 23
        );
        assert_eq!(
            summary.unresolved_media_count,
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 29
        );
        assert_eq!(
            summary.conflicts.len(),
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT
        );
        assert_eq!(
            summary.unresolved_root_ids.len(),
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT
        );
        assert_eq!(
            summary.unresolved_media_keys.len(),
            IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT
        );
        assert!(summary.conflicts_truncated);
        assert!(summary.unresolved_roots_truncated);
        assert!(summary.unresolved_media_truncated);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(!serialized.contains("relocated_graph"));
        assert!(!serialized.contains("current_state_sha256"));

        plan.plan_token = identity_import_plan_token(&PlanTokenMaterial {
            content_sha256: &plan.content_sha256,
            current_state_sha256: &plan.current_state_sha256,
            mode: plan.mode.as_str(),
            relocations: &plan.relocations,
            relocated_media_sha256: &plan.relocated_media_sha256,
            creates: plan.creates,
            updates: plan.updates,
            identical: plan.identical,
            deletes: plan.deletes,
            conflicts: &plan.conflicts,
            unresolved_root_ids: &plan.unresolved_root_ids,
            unresolved_media_keys: &plan.unresolved_media_keys,
            already_applied: plan.already_applied,
            entity_count: plan.entity_count,
            reference_count: plan.reference_count,
        })
        .unwrap();

        let root = TestRoot::new("bounded-unresolved-error");
        let store = MatchStore::open(&root.0).unwrap();
        let error = store.apply_identity_bundle_import(plan).unwrap_err();
        assert!(
            error.contains(&format!(
                "{} unresolved roots",
                IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT + 23
            )),
            "{error}"
        );
        assert!(error.len() < 4096);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn exchange_paths_reject_linked_ancestors_and_noncanonical_components() {
        let root = TestRoot::new("linked-ancestor");
        let real = root.0.join("real");
        let nested = real.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let input = nested.join("identity.json");
        fs::write(&input, b"{}").unwrap();
        let linked = root.0.join("linked");
        if try_directory_link(&real, &linked).is_ok() {
            let linked_input = linked.join("nested").join("identity.json");
            assert!(read_bounded_regular_file(&linked_input, 1024, "linked input").is_err());
            let linked_output = linked.join("nested").join("output.json");
            assert!(write_new_regular_file(&linked_output, b"safe", 1024).is_err());
            assert!(!nested.join("output.json").exists());
            assert!(validate_relocation_root(&linked.join("nested").to_string_lossy()).is_err());
            let linked_xmp = linked.join("nested").join("sidecar.xmp");
            assert!(validate_xmp_sidecar_path(&linked_xmp, false).is_err());
        }

        let dotted_input = nested.join(".").join("identity.json");
        assert!(read_bounded_regular_file(&dotted_input, 1024, "dotted input").is_err());
        let parent_input = nested.join("child").join("..").join("identity.json");
        assert!(read_bounded_regular_file(&parent_input, 1024, "parent input").is_err());
    }

    #[test]
    fn exchange_leaf_contract_rejects_ads_devices_and_non_normal_win32_names() {
        for rejected in [
            "victim.jpg:facial.xmp",
            "CON.xmp",
            "nul",
            "LPT9.json",
            "COM¹.xmp",
            "com²",
            "LPT³.json",
            "trailing.xmp.",
            "trailing.xmp ",
            "bad?.xmp",
        ] {
            assert!(
                validate_portable_file_leaf("test output", std::ffi::OsStr::new(rejected)).is_err(),
                "unexpectedly accepted {rejected}"
            );
        }
        assert!(validate_portable_file_leaf(
            "test output",
            std::ffi::OsStr::new("identity-bundle.json")
        )
        .is_ok());
        for rejected in ["aux/photo.jpg", "album./photo.jpg"] {
            assert!(
                validate_exchange_media_key(rejected).is_err(),
                "unexpectedly accepted media key {rejected}"
            );
        }
        for rejected in [
            "nested/AUX/cache",
            "nested/COM1.txt/cache",
            "nested/trailing./cache",
            "nested/trailing /cache",
        ] {
            assert!(
                validate_root_exclusion(rejected).is_err(),
                "unexpectedly accepted root exclusion {rejected}"
            );
        }
        assert!(validate_root_exclusion("nested/cache/thumbnails").is_ok());
    }

    #[test]
    fn relocation_fingerprint_byte_budget_rejects_file_and_aggregate_exhaustion() {
        let mut per_file = RelocationVerificationBudget::default();
        assert!(per_file
            .authorize_file("media/oversize.bin", IDENTITY_RELOCATION_MAX_FILE_BYTES + 1,)
            .unwrap_err()
            .contains("file limit"));

        let mut aggregate = RelocationVerificationBudget {
            hashed_bytes: IDENTITY_RELOCATION_MAX_TOTAL_BYTES,
        };
        assert!(aggregate
            .authorize_file("media/aggregate.bin", 1)
            .unwrap_err()
            .contains("aggregate byte limit"));
        let limits = IdentityBundleLimits::default();
        assert_eq!(
            limits.relocation_file_bytes,
            IDENTITY_RELOCATION_MAX_FILE_BYTES
        );
        assert_eq!(
            limits.relocation_total_bytes,
            IDENTITY_RELOCATION_MAX_TOTAL_BYTES
        );
        assert_eq!(
            limits.relocation_evidence_handles,
            IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES
        );
        assert_eq!(
            limits.import_derived_recovery_bytes,
            IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES
        );
    }

    #[test]
    fn relocation_evidence_handle_ceiling_fails_before_filesystem_resolution() {
        let graph = IdentityBundleGraph {
            correction_media_operations: (0..=IDENTITY_RELOCATION_MAX_EVIDENCE_HANDLES)
                .map(|index| PortableCorrectionMediaOperation {
                    mapping_id: format!("mapping-{index}"),
                    media_key: format!("media/{index:05}.jpg"),
                    media_fingerprint: "0".repeat(64),
                    operation_id: format!("operation-{index}"),
                    kind: "operator_correction".to_string(),
                    created_at: "2026-08-25T00:00:00Z".to_string(),
                })
                .collect(),
            roots: vec![PortableMatchRoot {
                root_id: "must-not-open".to_string(),
                portable_path: "relative-root-that-must-not-be-inspected".to_string(),
                exclusions: Vec::new(),
                enabled: true,
                created_at: "2026-08-25T00:00:00Z".to_string(),
                updated_at: "2026-08-25T00:00:00Z".to_string(),
            }],
            ..IdentityBundleGraph::default()
        };
        let error = resolve_relocated_media(&graph).unwrap_err();
        assert!(error.contains("retained evidence handles"), "{error}");
    }

    #[test]
    fn portable_correction_projection_excludes_vectors_recursively_and_import_rejects_them() {
        let embedding = FaceEmbedding {
            embedding_id: embedding_id("face-vector", "model-vector"),
            face_id: "face-vector".to_string(),
            vector: vec![1.0; EMBEDDING_DIM],
            model_generation: "model-vector".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: "fingerprint-vector".to_string(),
            face_revision: 1,
            job_id: "job-vector".to_string(),
            active: true,
            created_at: "2026-08-25T00:00:00Z".to_string(),
        };
        let trusted_search = TrustedSearchEmbedding {
            membership_id: "membership-vector".to_string(),
            person_id: "person-vector".to_string(),
            look_id: "look-vector".to_string(),
            face_id: embedding.face_id.clone(),
            embedding_id: embedding.embedding_id.clone(),
            vector: embedding.vector.clone(),
            model_generation: embedding.model_generation.clone(),
            created_at: embedding.created_at.clone(),
        };
        let cases = [
            ("manual_face", true, false),
            ("delete_face_analysis", false, false),
            ("not_a_face", false, true),
        ];
        let mut raw_operations = Vec::new();
        let mut projected_operations = Vec::new();
        for (kind, creates, include_trusted_search) in cases {
            let mut rows = vec![CorrectionRowDelta {
                table: CorrectionTable::Embedding,
                stable_id: embedding.embedding_id.clone(),
                before: (!creates).then(|| serde_json::to_value(&embedding).unwrap()),
                after: creates.then(|| serde_json::to_value(&embedding).unwrap()),
            }];
            if include_trusted_search {
                rows.push(CorrectionRowDelta {
                    table: CorrectionTable::TrustedSearch,
                    stable_id: trusted_search.membership_id.clone(),
                    before: Some(serde_json::to_value(&trusted_search).unwrap()),
                    after: None,
                });
            }
            let before = rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>();
            let envelope = ExchangeCorrectionDeltaEnvelope {
                version: 1,
                kind: kind.to_string(),
                rows,
                face_ids: vec![embedding.face_id.clone()],
                media_keys: vec!["media/vector.jpg".to_string()],
                identity_changed: true,
                catalog_changed: false,
            };
            let operation = MatchOperation {
                operation_id: format!("operation-vector-{kind}"),
                kind: format!("correction_{kind}"),
                face_id: Some(embedding.face_id.clone()),
                person_id: None,
                before_json: serde_json::to_string(&before).unwrap(),
                after_json: serde_json::to_string(&envelope).unwrap(),
                reversible: true,
                created_at: embedding.created_at.clone(),
            };
            projected_operations.push(project_match_operation_for_portable(&operation).unwrap());
            raw_operations.push(operation);
        }

        let raw_graph = IdentityBundleGraph {
            operations: raw_operations,
            ..IdentityBundleGraph::default()
        };
        assert!(validate_portable_graph_has_no_vectors(&raw_graph)
            .unwrap_err()
            .contains("prohibited embedding vector"));
        let projected_graph = IdentityBundleGraph {
            operations: projected_operations.clone(),
            ..IdentityBundleGraph::default()
        };
        validate_portable_graph_has_no_vectors(&projected_graph).unwrap();
        let projected_json = serde_json::to_string(&projected_graph).unwrap();
        assert!(!projected_json.contains("\\\"vector\\\""));
        assert!(projected_json.contains("\\\"vector_omitted\\\":true"));

        let mut forged_graph = projected_graph;
        let forged_operation = forged_graph.operations.first_mut().unwrap();
        let mut forged_envelope: Value =
            serde_json::from_str(&forged_operation.after_json).unwrap();
        forged_envelope["rows"][0]["after"]["vector"] = json!([1.0]);
        forged_operation.after_json = serde_json::to_string(&forged_envelope).unwrap();
        let mut forged_bundle = IdentityBundleV1 {
            manifest: IdentityBundleManifest {
                format: IDENTITY_BUNDLE_FORMAT.to_string(),
                version: IDENTITY_BUNDLE_VERSION,
                schema_version: MATCH_SCHEMA_VERSION,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                content_sha256: String::new(),
            },
            graph: forged_graph,
        };
        forged_bundle.manifest.content_sha256 =
            bundle_content_sha256(&forged_bundle.graph).unwrap();
        assert!(validate_bundle(&forged_bundle)
            .unwrap_err()
            .contains("prohibited embedding vector"));
    }

    #[test]
    fn projected_operation_self_import_is_idempotent_and_preserves_raw_undo_history() {
        let source_root = TestRoot::new("projected-operation-self-import");
        let media_root = source_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &media_root);

        let strict_face: FaceObservation = source
            .require(FACE_TABLE, "face-strict", "strict Face")
            .unwrap();
        let strict_embedding = FaceEmbedding {
            embedding_id: embedding_id(&strict_face.face_id, "model-v1"),
            face_id: strict_face.face_id.clone(),
            vector: {
                let mut vector = vec![0.0; EMBEDDING_DIM];
                vector[0] = 1.0;
                vector
            },
            model_generation: "model-v1".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: strict_face.media_fingerprint.clone(),
            face_revision: strict_face.face_revision,
            job_id: "job-self-import-vector-history".to_string(),
            active: true,
            created_at: strict_face.created_at.clone(),
        };
        source
            .upsert_json(
                EMBEDDING_TABLE,
                &strict_embedding.embedding_id,
                &strict_embedding,
            )
            .unwrap();
        let correction = source
            .mark_not_a_face(
                &strict_face.face_id,
                &source.correction_fence(&strict_face.face_id).unwrap(),
            )
            .unwrap();
        let raw_operation: MatchOperation = source
            .require(
                OPERATION_TABLE,
                &correction.operation_id,
                "raw vector-bearing correction",
            )
            .unwrap();
        assert!(raw_operation.after_json.contains("\"vector\""));

        let bundle_path = source_root.0.join("projected-self-import.json");
        source.export_identity_bundle(&bundle_path).unwrap();
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let merge = source
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Merge)
            .unwrap();
        assert!(merge.already_applied);
        assert_eq!(merge.updates, 0);
        assert!(merge.conflicts.is_empty());
        let replace = source
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(replace.already_applied);
        assert_eq!(replace.updates, 0);
        assert_eq!(replace.deletes, 0);
        assert!(replace.conflicts.is_empty());

        let receipt = source.apply_identity_bundle_import(merge).unwrap();
        assert!(receipt.already_applied);
        assert_eq!(receipt.created, 0);
        assert_eq!(receipt.updated, 0);
        assert_eq!(receipt.deleted, 0);
        assert_eq!(
            source
                .require::<MatchOperation>(
                    OPERATION_TABLE,
                    &correction.operation_id,
                    "raw vector-bearing correction after no-op import",
                )
                .unwrap(),
            raw_operation
        );

        let independent = source
            .create_person("Independent overlapping lineage", Vec::new())
            .unwrap();
        let overlapping = source
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Merge)
            .unwrap();
        assert!(overlapping.already_applied);
        assert_eq!(overlapping.creates, 0);
        assert_eq!(overlapping.updates, 0);
        assert_eq!(overlapping.deletes, 0);
        assert!(overlapping.conflicts.is_empty());
        source.apply_identity_bundle_import(overlapping).unwrap();
        assert!(source
            .get_one::<Person>(PERSON_TABLE, &independent.person_id)
            .unwrap()
            .is_some());
        assert_eq!(
            source
                .require::<MatchOperation>(
                    OPERATION_TABLE,
                    &correction.operation_id,
                    "raw vector-bearing correction after overlapping merge",
                )
                .unwrap(),
            raw_operation
        );
    }

    #[test]
    fn relocated_media_resolution_requires_one_regular_descriptor_safe_target() {
        let root = TestRoot::new("relocated-media-resolution");
        let first = root.0.join("first");
        let second = root.0.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let mut graph = IdentityBundleGraph {
            faces: vec![face("face-resolution", "media/item.jpg")],
            roots: vec![PortableMatchRoot {
                root_id: "root-first".to_string(),
                portable_path: fs::canonicalize(&first)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                exclusions: Vec::new(),
                enabled: true,
                created_at: "2026-08-25T00:00:00Z".to_string(),
                updated_at: "2026-08-25T00:00:00Z".to_string(),
            }],
            ..IdentityBundleGraph::default()
        };
        assert_eq!(
            unresolved_relocated_media_keys(&graph).unwrap(),
            vec!["media/item.jpg".to_string()]
        );

        let first_media = first.join("media/item.jpg");
        fs::create_dir_all(first_media.parent().unwrap()).unwrap();
        fs::write(&first_media, b"fixture-media/item.jpg").unwrap();
        assert!(unresolved_relocated_media_keys(&graph).unwrap().is_empty());
        graph.faces[0].media_fingerprint = format!("sha256:{}", graph.faces[0].media_fingerprint);
        assert!(unresolved_relocated_media_keys(&graph).unwrap().is_empty());
        graph.faces[0].media_fingerprint = format!("sha256:{}", "0".repeat(64));
        assert_eq!(
            unresolved_relocated_media_keys(&graph).unwrap(),
            vec!["media/item.jpg".to_string()]
        );
        graph.faces[0].media_fingerprint = "fingerprint-face-resolution".to_string();
        fs::write(&first_media, b"wrong-bytes-hidden-by-opaque-fingerprint").unwrap();
        assert!(unresolved_relocated_media_keys(&graph)
            .unwrap_err()
            .contains("lowercase SHA-256 digest"));
        graph.faces[0] = face("face-resolution", "media/item.jpg");
        fs::write(&first_media, b"fixture-media/item.jpg").unwrap();

        let second_media = second.join("media/item.jpg");
        fs::create_dir_all(second_media.parent().unwrap()).unwrap();
        fs::write(&second_media, b"fixture-media/item.jpg").unwrap();
        graph.roots.push(PortableMatchRoot {
            root_id: "root-second".to_string(),
            portable_path: fs::canonicalize(&second)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            exclusions: Vec::new(),
            enabled: true,
            created_at: "2026-08-25T00:00:00Z".to_string(),
            updated_at: "2026-08-25T00:00:00Z".to_string(),
        });
        assert_eq!(
            unresolved_relocated_media_keys(&graph).unwrap(),
            vec!["media/item.jpg".to_string()]
        );

        let mut conflicting_face = face("face-conflict", "media/item.jpg");
        conflicting_face.media_fingerprint = sha256_bytes(b"conflicting-media-bytes");
        graph.faces.push(conflicting_face);
        let error = unresolved_relocated_media_keys(&graph).unwrap_err();
        assert!(
            error.contains("conflicting current/historical fingerprints"),
            "{error}"
        );
    }

    #[test]
    fn identity_import_preview_reports_missing_media_and_apply_refuses_it() {
        let source_root = TestRoot::new("missing-media-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);
        let bundle_path = source_root.0.join("identity.json");
        source.export_identity_bundle(&bundle_path).unwrap();

        let target_root = TestRoot::new("missing-media-target");
        let empty_media = target_root.0.join("relocated");
        fs::create_dir_all(&empty_media).unwrap();
        let target = MatchStore::open(&target_root.0).unwrap();
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&empty_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert_eq!(plan.unresolved_root_ids, Vec::<String>::new());
        assert_eq!(
            plan.unresolved_media_keys,
            vec![
                "media/a.jpg".to_string(),
                "media/b.jpg".to_string(),
                "media/c.jpg".to_string(),
            ]
        );
        assert!(target
            .apply_identity_bundle_import(plan)
            .unwrap_err()
            .contains("3 unresolved media keys"));

        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let path = empty_media.join(media_key);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("fixture-{media_key}")).unwrap();
        }
        let stale_plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        fs::remove_file(empty_media.join("media/b.jpg")).unwrap();
        let error = target.apply_identity_bundle_import(stale_plan).unwrap_err();
        assert!(
            error.contains("media resolution changed after preview"),
            "{error}"
        );

        fs::write(empty_media.join("media/b.jpg"), b"fixture-media/b.jpg").unwrap();
        let opaque_digest_plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        let opaque_path = empty_media.join("media/c.jpg");
        let mut opaque_bytes = fs::read(&opaque_path).unwrap();
        opaque_bytes[0] ^= 0xff;
        fs::write(&opaque_path, &opaque_bytes).unwrap();
        let error = target
            .apply_identity_bundle_import(opaque_digest_plan)
            .unwrap_err();
        assert!(
            error.contains("media content changed after preview"),
            "{error}"
        );
        assert_eq!(target.count(PERSON_TABLE).unwrap(), 0);

        fs::write(empty_media.join("media/c.jpg"), b"fixture-media/c.jpg").unwrap();
        let pre_commit_race = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        REMOVE_NEXT_RESOLVED_MEDIA_BEFORE_IMPORT_COMMIT
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = target
            .apply_identity_bundle_import(pre_commit_race)
            .unwrap_err();
        assert!(error.contains("pre-mutation boundary"), "{error}");
        assert_eq!(target.count(PERSON_TABLE).unwrap(), 0);

        fs::write(empty_media.join("media/a.jpg"), b"fixture-media/a.jpg").unwrap();
        let during_commit_race = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        REMOVE_NEXT_RESOLVED_MEDIA_AFTER_IMPORT_COMMIT
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = target
            .apply_identity_bundle_import(during_commit_race)
            .unwrap_err();
        assert!(error.contains("media changed during commit"), "{error}");
        assert_eq!(target.count(PERSON_TABLE).unwrap(), 0);
        assert!(target
            .build_identity_bundle()
            .unwrap()
            .graph
            .people
            .is_empty());
    }

    #[test]
    fn interrupted_import_journal_rolls_back_before_open_reconciliation() {
        let source_root = TestRoot::new("journal-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);
        let bundle_path = source_root.0.join("identity.json");
        source.export_identity_bundle(&bundle_path).unwrap();

        let target_root = TestRoot::new("journal-target");
        let target_media = target_root.0.join("relocated");
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let target_path = target_media.join(media_key);
            fs::create_dir_all(target_path.parent().unwrap()).unwrap();
            fs::copy(source_media.join(media_key), target_path).unwrap();
        }
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&target_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let target = MatchStore::open(&target_root.0).unwrap();
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        poison_identity_import_caches(&target);
        inject_leave_import_journal_for_plan(&plan.plan_token);
        let error = target.apply_identity_bundle_import(plan).unwrap_err();
        assert!(error.contains("injected identity import interruption"));
        assert_identity_import_caches_fail_closed(&target);
        assert!(target.count(PERSON_TABLE).unwrap() > 0);
        assert!(target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_some());

        drop(target);
        crate::surreal_store::wait_until_closed(&MediaDb::db_path(&target_root.0)).unwrap();
        let reopened = MatchStore::open(&target_root.0).unwrap();
        assert_eq!(reopened.count(PERSON_TABLE).unwrap(), 0);
        assert!(reopened
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_none());
        assert!(!fs::read_dir(MediaDb::media_state_dir(&target_root.0))
            .unwrap()
            .any(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().into_string().ok())
                    .is_some_and(|name| name.starts_with(IDENTITY_IMPORT_RECOVERY_PREFIX))
            }));
    }

    #[test]
    fn unreadable_import_journal_invalidates_caches_before_decode_failure() {
        let target_root = TestRoot::new("journal-read-failure-cache-invalidation");
        let target = MatchStore::open(&target_root.0).unwrap();
        let db = target.store.db();
        surreal_store::run(async move {
            db.query(
                "DEFINE FIELD OVERWRITE corruption_probe ON TABLE \
                 match_identity_import_journal TYPE string;",
            )
            .await
            .map_err(|error| format!("define malformed journal probe field: {error}"))?
            .check()
            .map_err(|error| format!("define malformed journal probe field: {error}"))?;
            Ok(())
        })
        .unwrap();
        let timestamp = now();
        let mut malformed = serde_json::to_value(IdentityImportJournal {
            journal_id: IDENTITY_IMPORT_JOURNAL_ID.to_string(),
            version: IDENTITY_IMPORT_JOURNAL_VERSION,
            plan_token: "0".repeat(64),
            pre_state_sha256: "0".repeat(64),
            recovery_path: "malformed-journal-probe".to_string(),
            recovery_file_sha256: "0".repeat(64),
            recovery_content_sha256: "0".repeat(64),
            relocations_json: "{}".to_string(),
            phase: "prepared".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
        .unwrap();
        malformed
            .as_object_mut()
            .unwrap()
            .insert("corruption_probe".to_string(), json!("unexpected"));
        target
            .upsert_json(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
                &malformed,
            )
            .unwrap();
        poison_identity_import_caches(&target);

        let error = {
            let _guard = target.store.transaction_lock().write().unwrap();
            target
                .recover_present_identity_import_journal_unlocked()
                .unwrap_err()
        };
        assert!(
            error.contains("unknown field") && error.contains("corruption_probe"),
            "{error}"
        );
        assert_identity_import_caches_fail_closed(&target);
        assert!(target
            .cached_projection("stale/import-cache-only.jpg")
            .unwrap()
            .is_none());
    }

    #[test]
    fn failed_import_rollback_invalidates_caches_after_portable_restore() {
        let target_root = TestRoot::new("rollback-cache-invalidation-target");
        let target = MatchStore::open(&target_root.0).unwrap();
        let media_root = target_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&target, &media_root);
        seed_trusted_projection_and_calibration(&target);

        let empty_root = TestRoot::new("rollback-cache-invalidation-empty");
        let empty = MatchStore::open(&empty_root.0).unwrap();
        let empty_bundle_path = empty_root.0.join("empty-identity.json");
        empty.export_identity_bundle(&empty_bundle_path).unwrap();

        let plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        inject_import_rollback_failure_after_portable_restore(&plan.plan_token);
        target
            .trusted_search_reconcile_failures
            .store(1, Ordering::SeqCst);
        poison_identity_import_caches(&target);
        let error = target.apply_identity_bundle_import(plan).unwrap_err();
        assert!(
            error.contains("derived rollback failure after portable restore"),
            "{error}"
        );
        assert!(target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_some());
        assert_identity_import_caches_fail_closed(&target);
        assert!(target
            .cached_projection("stale/import-cache-only.jpg")
            .unwrap()
            .is_none());
        assert!(target
            .autocomplete("stale import cache", u64::MAX, 10)
            .unwrap_err()
            .contains("stale autocomplete catalog revision"));
    }

    #[test]
    fn ordinary_mutation_recovers_pending_import_before_writing() {
        let source_root = TestRoot::new("same-process-import-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);
        let bundle_path = source_root.0.join("identity.json");
        source.export_identity_bundle(&bundle_path).unwrap();

        let target_root = TestRoot::new("same-process-import-target");
        let target_media = target_root.0.join("relocated");
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let target_path = target_media.join(media_key);
            fs::create_dir_all(target_path.parent().unwrap()).unwrap();
            fs::copy(source_media.join(media_key), target_path).unwrap();
        }
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&target_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let target = MatchStore::open(&target_root.0).unwrap();
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        inject_leave_import_journal_for_plan(&plan.plan_token);
        assert!(target
            .apply_identity_bundle_import(plan)
            .unwrap_err()
            .contains("injected identity import interruption"));
        assert!(target.count(PERSON_TABLE).unwrap() > 0);

        let journal = target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .unwrap();
        let live_bundle = target.identity_import_recovery_path(&journal).unwrap();
        let live_derived = target
            .identity_import_derived_recovery_path(
                &parse_identity_import_recovery_evidence(&journal.relocations_json)
                    .unwrap()
                    .derived,
            )
            .unwrap();
        assert!(live_bundle.is_file());
        assert!(live_derived.is_file());
        {
            let _guard = target.store.transaction_lock().write().unwrap();
            assert_eq!(
                target
                    .reconcile_identity_import_recovery_artifacts_unlocked()
                    .unwrap(),
                0
            );
        }
        assert!(live_bundle.is_file());
        assert!(live_derived.is_file());

        let created = target
            .create_person("After pending import", Vec::new())
            .unwrap();
        assert_eq!(target.count(PERSON_TABLE).unwrap(), 1);
        assert!(target
            .get_one::<Person>(PERSON_TABLE, &created.person_id)
            .unwrap()
            .is_some());
        assert!(target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn orphan_import_artifacts_are_confined_and_reconciled_before_mutation() {
        let root = TestRoot::new("orphan-import-recovery");
        let store = MatchStore::open(&root.0).unwrap();
        let state_root = MediaDb::media_state_dir(&root.0);
        let orphan_bundle = state_root.join(format!(
            "{IDENTITY_IMPORT_RECOVERY_PREFIX}orphan.facial-identity.json"
        ));
        let orphan_derived = state_root.join(format!(
            "{IDENTITY_IMPORT_RECOVERY_PREFIX}orphan{IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX}"
        ));
        fs::write(&orphan_bundle, b"orphan-bundle").unwrap();
        fs::write(&orphan_derived, b"orphan-derived").unwrap();
        let unrelated = state_root.join("operator-owned-unrelated.json");
        fs::write(&unrelated, b"keep").unwrap();

        store.create_person("Orphan sweep", Vec::new()).unwrap();
        assert!(!orphan_bundle.exists());
        assert!(!orphan_derived.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn pending_import_recovery_restores_exact_derived_snapshot_before_maintenance_and_open() {
        let target_root = TestRoot::new("journal-exact-derived-target");
        let target = MatchStore::open(&target_root.0).unwrap();
        let media_root = target_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&target, &media_root);
        seed_trusted_projection_and_calibration(&target);
        let before = target.capture_exchange_derived_snapshot_unlocked().unwrap();

        let empty_root = TestRoot::new("journal-exact-derived-empty");
        let empty = MatchStore::open(&empty_root.0).unwrap();
        let empty_bundle_path = empty_root.0.join("empty-identity.json");
        empty.export_identity_bundle(&empty_bundle_path).unwrap();

        let plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        inject_leave_import_journal_for_plan(&plan.plan_token);
        assert!(target
            .apply_identity_bundle_import(plan)
            .unwrap_err()
            .contains("injected identity import interruption"));
        let journal = target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .unwrap();
        let evidence = parse_identity_import_recovery_evidence(&journal.relocations_json).unwrap();
        assert!(Path::new(&evidence.derived.canonical_path).is_file());
        assert_eq!(
            evidence.derived.snapshot_sha256,
            exchange_derived_snapshot_digest(&before).unwrap()
        );

        target.preview_rebuild_match_analysis().unwrap();
        assert_eq!(
            target.capture_exchange_derived_snapshot_unlocked().unwrap(),
            before
        );
        assert!(target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_none());
        assert!(!Path::new(&evidence.derived.canonical_path).exists());

        let restart_before = target.capture_exchange_derived_snapshot_unlocked().unwrap();
        let plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        inject_leave_import_journal_for_plan(&plan.plan_token);
        assert!(target
            .apply_identity_bundle_import(plan)
            .unwrap_err()
            .contains("injected identity import interruption"));
        drop(target);
        crate::surreal_store::wait_until_closed(&MediaDb::db_path(&target_root.0)).unwrap();

        let reopened = MatchStore::open(&target_root.0).unwrap();
        assert_eq!(
            reopened
                .capture_exchange_derived_snapshot_unlocked()
                .unwrap(),
            restart_before
        );
        assert!(reopened
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_none());
        assert!(!fs::read_dir(MediaDb::media_state_dir(&target_root.0))
            .unwrap()
            .any(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().into_string().ok())
                    .is_some_and(|name| name.starts_with(IDENTITY_IMPORT_RECOVERY_PREFIX))
            }));
    }

    #[test]
    fn maintenance_fails_closed_when_pending_import_derived_evidence_is_corrupt() {
        let target_root = TestRoot::new("journal-corrupt-derived-target");
        let target = MatchStore::open(&target_root.0).unwrap();
        let media_root = target_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&target, &media_root);
        seed_trusted_projection_and_calibration(&target);

        let empty_root = TestRoot::new("journal-corrupt-derived-empty");
        let empty = MatchStore::open(&empty_root.0).unwrap();
        let empty_bundle_path = empty_root.0.join("empty-identity.json");
        empty.export_identity_bundle(&empty_bundle_path).unwrap();
        let plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        inject_leave_import_journal_for_plan(&plan.plan_token);
        assert!(target
            .apply_identity_bundle_import(plan)
            .unwrap_err()
            .contains("injected identity import interruption"));
        let journal = target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .unwrap();
        let evidence = parse_identity_import_recovery_evidence(&journal.relocations_json).unwrap();
        let mut escaped = evidence.derived.clone();
        escaped.canonical_path = std::env::temp_dir()
            .join(format!(
                "{IDENTITY_IMPORT_RECOVERY_PREFIX}escaped{IDENTITY_IMPORT_DERIVED_RECOVERY_SUFFIX}"
            ))
            .to_string_lossy()
            .to_string();
        assert!(target
            .identity_import_derived_recovery_path(&escaped)
            .unwrap_err()
            .contains("escaped app-owned state"));
        fs::write(&evidence.derived.canonical_path, b"corrupt").unwrap();

        poison_identity_import_caches(&target);
        let rebuild_error = target.preview_rebuild_match_analysis().unwrap_err();
        assert!(
            rebuild_error.contains("pending identity import"),
            "{rebuild_error}"
        );
        assert_identity_import_caches_fail_closed(&target);
        assert!(target
            .cached_projection("stale/import-cache-only.jpg")
            .unwrap()
            .is_none());
        let clear_path = target_root.0.join("must-not-publish-clear-recovery.json");
        let clear_error = target
            .preview_clear_all_match_data(&clear_path)
            .unwrap_err();
        assert!(
            clear_error.contains("pending identity import"),
            "{clear_error}"
        );
        assert!(!clear_path.exists());
        assert!(target
            .get_one::<IdentityImportJournal>(
                IDENTITY_IMPORT_JOURNAL_TABLE,
                IDENTITY_IMPORT_JOURNAL_ID,
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn xmp_writer_stops_at_incremental_sidecar_budget() {
        let region = MwgXmpRegion {
            face_id: "face-budget".to_string(),
            person_id: "person-budget".to_string(),
            name: "Budget".to_string(),
            bounds_normalized: [0.1, 0.1, 0.2, 0.2],
        };
        let regions = vec![region; 25_000];
        assert!(render_mwg_xmp(&regions, Some([100, 100]))
            .unwrap_err()
            .contains("MWG XMP exceeds"));
    }

    #[test]
    fn derived_recovery_uses_one_checked_aggregate_byte_budget() {
        assert_eq!(
            charge_derived_recovery_bytes(IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES - 1, 1)
                .unwrap(),
            IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES
        );
        assert!(
            charge_derived_recovery_bytes(IDENTITY_IMPORT_DERIVED_RECOVERY_MAX_BYTES, 1)
                .unwrap_err()
                .contains("aggregate byte ceiling before materialization")
        );
    }

    #[test]
    fn legacy_provenance_current_anchor_index_has_linear_checked_work() {
        let mut stored = StoredIdentityGraph::default();
        stored.faces = (0..512)
            .map(|index| face(&format!("face-index-{index:04}"), "media/index.jpg"))
            .collect();
        let history = LegacySuggestionHistory::default();
        let mut work = 0usize;
        let lookup = build_legacy_suggestion_lookup(&stored, &history, &mut work).unwrap();
        assert_eq!(lookup.current_faces.len(), 512);
        assert_eq!(work, 512);
        assert!(lookup.current_faces.contains_key("face-index-0511"));

        stored.faces.push(stored.faces[0].clone());
        let duplicate = build_legacy_suggestion_lookup(&stored, &history, &mut 0).unwrap_err();
        assert!(duplicate.contains("duplicate current Face"));
    }

    fn face(id: &str, media_key: &str) -> FaceObservation {
        FaceObservation {
            face_id: id.to_string(),
            media_key: media_key.to_string(),
            media_fingerprint: sha256_bytes(format!("fixture-{media_key}").as_bytes()),
            source_index: 0,
            source_width: Some(100),
            source_height: Some(100),
            exif_orientation: Some(1),
            bounds_normalized: vec![0.1, 0.2, 0.3, 0.4],
            landmarks_normalized: vec![vec![0.2, 0.3], vec![0.3, 0.3]],
            alignment_valid: true,
            quality: 0.9,
            pose_bucket: "frontal".to_string(),
            operator_owned: true,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: "2026-08-25T00:00:00Z".to_string(),
            updated_at: "2026-08-25T00:00:00Z".to_string(),
        }
    }

    fn seed_portable_graph(store: &MatchStore, root_path: &Path) {
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let path = root_path.join(media_key);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("fixture-{media_key}")).unwrap();
        }
        let now = "2026-08-25T00:00:00Z".to_string();
        let generation = ModelGeneration {
            generation: "model-v1".to_string(),
            state: "active".to_string(),
            validated: true,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let person = Person {
            person_id: "person-a".to_string(),
            name: "Alice & Bob".to_string(),
            aliases: vec!["A".to_string()],
            cover_media_key: Some("media/a.jpg".to_string()),
            hidden: false,
            favorite: true,
            revision: 1,
            catalog_revision: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let look = Look {
            look_id: "look-a".to_string(),
            person_id: person.person_id.clone(),
            name: "default".to_string(),
            revision: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let set = TrustedTemplateSet {
            set_id: "set-a".to_string(),
            look_id: look.look_id.clone(),
            name: "trusted".to_string(),
            revision: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let assigned_face = face("face-a", "media/a.jpg");
        let suggested_face = face("face-suggestion", "media/b.jpg");
        let strict_face = face("face-strict", "media/c.jpg");
        let operator_operation_id = "operation-a".to_string();
        let historical = MatchOperation {
            operation_id: "operation-history".to_string(),
            kind: "not_sure".to_string(),
            face_id: Some("removed-face".to_string()),
            person_id: Some("removed-person".to_string()),
            before_json: "[null,null]".to_string(),
            after_json: "null".to_string(),
            reversible: false,
            created_at: now.clone(),
        };
        let assignment = Assignment {
            assignment_id: assigned_face.face_id.clone(),
            face_id: assigned_face.face_id.clone(),
            person_id: person.person_id.clone(),
            media_key: assigned_face.media_key.clone(),
            look_id: Some(look.look_id.clone()),
            placement: "look".to_string(),
            state: "operator_confirmed".to_string(),
            provenance: "operator".to_string(),
            locked: true,
            model_generation: None,
            calibration_generation: None,
            envelope_hash: None,
            face_revision: 1,
            person_revision: 1,
            operation_id: operator_operation_id.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let operation = MatchOperation {
            operation_id: operator_operation_id,
            kind: "assign_operator_confirmed".to_string(),
            face_id: Some(assigned_face.face_id.clone()),
            person_id: Some(person.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&assignment).unwrap(),
            reversible: true,
            created_at: now.clone(),
        };
        let operation_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &operation.operation_id,
                &assigned_face.media_key,
            ),
            media_key: assigned_face.media_key.clone(),
            media_fingerprint: assigned_face.media_fingerprint.clone(),
            operation_id: operation.operation_id.clone(),
            kind: operation.kind.clone(),
            created_at: operation.created_at.clone(),
        };
        let historical_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &historical.operation_id,
                &suggested_face.media_key,
            ),
            media_key: suggested_face.media_key.clone(),
            media_fingerprint: suggested_face.media_fingerprint.clone(),
            operation_id: historical.operation_id.clone(),
            kind: historical.kind.clone(),
            created_at: historical.created_at.clone(),
        };
        let strict_operation_id = "operation-strict".to_string();
        let strict_assignment = Assignment {
            assignment_id: strict_face.face_id.clone(),
            face_id: strict_face.face_id.clone(),
            person_id: person.person_id.clone(),
            media_key: strict_face.media_key.clone(),
            look_id: None,
            placement: "unsorted".to_string(),
            state: "committed_strict_automatic".to_string(),
            provenance: "strict_recognition_v1".to_string(),
            locked: false,
            model_generation: Some(generation.generation.clone()),
            calibration_generation: Some("calibration-v1".to_string()),
            envelope_hash: Some("a".repeat(64)),
            face_revision: 1,
            person_revision: 1,
            operation_id: strict_operation_id.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let strict_operation = MatchOperation {
            operation_id: strict_operation_id,
            kind: "assign_committed_strict_automatic".to_string(),
            face_id: Some(strict_face.face_id.clone()),
            person_id: Some(person.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&strict_assignment).unwrap(),
            reversible: true,
            created_at: now.clone(),
        };
        let strict_operation_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &strict_operation.operation_id,
                &strict_face.media_key,
            ),
            media_key: strict_face.media_key.clone(),
            media_fingerprint: strict_face.media_fingerprint.clone(),
            operation_id: strict_operation.operation_id.clone(),
            kind: strict_operation.kind.clone(),
            created_at: strict_operation.created_at.clone(),
        };
        let membership_operation_id = "operation-membership".to_string();
        let membership = TrustedTemplateMembership {
            membership_id: trusted_member_id(&set.set_id, &assigned_face.face_id),
            set_id: set.set_id.clone(),
            look_id: look.look_id.clone(),
            face_id: assigned_face.face_id.clone(),
            authorized: true,
            alignment_valid: true,
            quality_passed: true,
            pose_passed: true,
            diversity_passed: true,
            provenance: "operator".to_string(),
            model_generation: generation.generation.clone(),
            embedding_id: embedding_id(&assigned_face.face_id, &generation.generation),
            media_fingerprint: assigned_face.media_fingerprint.clone(),
            face_revision: 1,
            quality_score: 0.9,
            quality_threshold: 0.7,
            pose_bucket: "frontal".to_string(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
            operation_id: membership_operation_id.clone(),
            created_at: now.clone(),
        };
        let membership_operation = MatchOperation {
            operation_id: membership_operation_id,
            kind: "authorize_trusted_reference".to_string(),
            face_id: Some(assigned_face.face_id.clone()),
            person_id: Some(person.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&membership).unwrap(),
            reversible: true,
            created_at: now.clone(),
        };
        let suggestion = Suggestion {
            suggestion_id: suggestion_id(&suggested_face.face_id, &person.person_id),
            face_id: suggested_face.face_id.clone(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.8,
            model_generation: generation.generation.clone(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: suggested_face.media_fingerprint.clone(),
            face_revision: 1,
            person_revision: 1,
            job_id: "job-history".to_string(),
            created_at: now.clone(),
        };
        let suggestion_embedding = FaceEmbedding {
            embedding_id: embedding_id(&suggested_face.face_id, &generation.generation),
            face_id: suggested_face.face_id.clone(),
            vector: {
                let mut vector = vec![0.0; EMBEDDING_DIM];
                vector[0] = 1.0;
                vector
            },
            model_generation: generation.generation.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: suggested_face.media_fingerprint.clone(),
            face_revision: suggested_face.face_revision,
            job_id: suggestion.job_id.clone(),
            active: true,
            created_at: now.clone(),
        };
        let root = MatchIndexRoot {
            root_id: "root-a".to_string(),
            path: fs::canonicalize(root_path)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            exclusions: vec![],
            enabled: true,
            created_at: now.clone(),
            updated_at: now,
        };
        let owned = [
            (
                GENERATION_TABLE.to_string(),
                generation.generation.clone(),
                serde_json::to_value(&generation).unwrap(),
            ),
            (
                PERSON_TABLE.to_string(),
                person.person_id.clone(),
                serde_json::to_value(&person).unwrap(),
            ),
            (
                LOOK_TABLE.to_string(),
                look.look_id.clone(),
                serde_json::to_value(&look).unwrap(),
            ),
            (
                TEMPLATE_SET_TABLE.to_string(),
                set.set_id.clone(),
                serde_json::to_value(&set).unwrap(),
            ),
            (
                FACE_TABLE.to_string(),
                assigned_face.face_id.clone(),
                serde_json::to_value(&assigned_face).unwrap(),
            ),
            (
                FACE_TABLE.to_string(),
                suggested_face.face_id.clone(),
                serde_json::to_value(&suggested_face).unwrap(),
            ),
            (
                FACE_TABLE.to_string(),
                strict_face.face_id.clone(),
                serde_json::to_value(&strict_face).unwrap(),
            ),
            (
                OPERATION_TABLE.to_string(),
                operation.operation_id.clone(),
                serde_json::to_value(&operation).unwrap(),
            ),
            (
                OPERATION_TABLE.to_string(),
                historical.operation_id.clone(),
                serde_json::to_value(&historical).unwrap(),
            ),
            (
                OPERATION_TABLE.to_string(),
                strict_operation.operation_id.clone(),
                serde_json::to_value(&strict_operation).unwrap(),
            ),
            (
                OPERATION_TABLE.to_string(),
                membership_operation.operation_id.clone(),
                serde_json::to_value(&membership_operation).unwrap(),
            ),
            (
                CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                operation_mapping.mapping_id.clone(),
                serde_json::to_value(&operation_mapping).unwrap(),
            ),
            (
                CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                historical_mapping.mapping_id.clone(),
                serde_json::to_value(&historical_mapping).unwrap(),
            ),
            (
                CORRECTION_MEDIA_OPERATION_TABLE.to_string(),
                strict_operation_mapping.mapping_id.clone(),
                serde_json::to_value(&strict_operation_mapping).unwrap(),
            ),
            (
                ASSIGNMENT_TABLE.to_string(),
                assignment.face_id.clone(),
                serde_json::to_value(&assignment).unwrap(),
            ),
            (
                TRUSTED_MEMBER_TABLE.to_string(),
                membership.membership_id.clone(),
                serde_json::to_value(&membership).unwrap(),
            ),
            (
                ASSIGNMENT_TABLE.to_string(),
                strict_assignment.face_id.clone(),
                serde_json::to_value(&strict_assignment).unwrap(),
            ),
            (
                SUGGESTION_TABLE.to_string(),
                suggestion.suggestion_id.clone(),
                serde_json::to_value(&suggestion).unwrap(),
            ),
            (
                EMBEDDING_TABLE.to_string(),
                suggestion_embedding.embedding_id.clone(),
                serde_json::to_value(&suggestion_embedding).unwrap(),
            ),
            (
                ROOT_CONFIG_TABLE.to_string(),
                root.root_id.clone(),
                serde_json::to_value(&root).unwrap(),
            ),
        ];
        let borrowed = owned
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&borrowed, &[]).unwrap();
    }

    fn seed_trusted_projection_and_calibration(store: &MatchStore) -> CalibrationActivation {
        let face: FaceObservation = store.require(FACE_TABLE, "face-a", "face").unwrap();
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, "model-v1"),
            face_id: face.face_id.clone(),
            vector: {
                let mut vector = vec![0.0; EMBEDDING_DIM];
                vector[0] = 1.0;
                vector
            },
            model_generation: "model-v1".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint,
            face_revision: face.face_revision,
            job_id: "exchange-derived-fixture".to_string(),
            active: true,
            created_at: "2026-08-25T00:00:00Z".to_string(),
        };
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        assert_eq!(store.reconcile_trusted_search().unwrap(), 1);
        let build: TrustedIndexBuildReceipt = store
            .require(TRUSTED_INDEX_BUILD_TABLE, "global", "trusted build receipt")
            .unwrap();
        let timestamp = "2026-08-25T00:00:00Z".to_string();
        let mut activation = CalibrationActivation {
            calibration_generation: "calibration-exchange".to_string(),
            model_generation: "model-v1".to_string(),
            envelope_hash: "a".repeat(64),
            runtime_configuration_digest: strict_runtime_configuration_digest(100, 50),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: build.build_digest,
            gallery_members_digest: store.trusted_gallery_members_digest().unwrap(),
            verifier_artifact_id: "VAL-WP-085-EXCHANGE".to_string(),
            contract_sha256: "b".repeat(64),
            raw_records_sha256: "c".repeat(64),
            evidence_digest: "d".repeat(64),
            review_digest: "e".repeat(64),
            automatic_threshold: 0.9,
            suggestion_threshold: 0.8,
            runner_up_margin: 0.1,
            minimum_quality: 0.7,
            candidate_k: 100,
            rerank_k: 50,
            people_max: 10,
            looks_per_person_max: 8,
            templates_per_look_max: 16,
            total_templates_max: 100,
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
            .upsert_json(
                CALIBRATION_TABLE,
                &activation.calibration_generation,
                &activation,
            )
            .unwrap();
        activation
    }

    #[test]
    fn identity_import_failure_and_both_rollback_paths_restore_derived_truth() {
        let target_root = TestRoot::new("derived-rollback-target");
        let target = MatchStore::open(&target_root.0).unwrap();
        let media_root = target_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&target, &media_root);
        let active_calibration = seed_trusted_projection_and_calibration(&target);
        let before = target.capture_exchange_derived_snapshot_unlocked().unwrap();
        let recovery_path = target_root.0.join("pre-import-recovery.json");
        target
            .export_identity_recovery_bundle(&recovery_path)
            .unwrap();

        let empty_root = TestRoot::new("derived-rollback-empty");
        let empty = MatchStore::open(&empty_root.0).unwrap();
        let empty_bundle_path = empty_root.0.join("empty-identity.json");
        empty.export_identity_bundle(&empty_bundle_path).unwrap();

        let failed_plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        target
            .trusted_search_reconcile_failures
            .store(1, Ordering::SeqCst);
        assert!(target
            .apply_identity_bundle_import(failed_plan)
            .unwrap_err()
            .contains("pre-import recovery: Ok"));
        assert_eq!(
            target
                .capture_exchange_derived_snapshot_unlocked()
                .unwrap()
                .trusted_search_rows,
            before.trusted_search_rows
        );
        assert_eq!(
            target
                .capture_exchange_derived_snapshot_unlocked()
                .unwrap()
                .trusted_index_build,
            before.trusted_index_build
        );
        assert_eq!(
            target
                .capture_exchange_derived_snapshot_unlocked()
                .unwrap()
                .calibrations,
            before.calibrations
        );

        let apply_plan = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        let applied = target.apply_identity_bundle_import(apply_plan).unwrap();
        let invalidated: CalibrationActivation = target
            .require(
                CALIBRATION_TABLE,
                &active_calibration.calibration_generation,
                "invalidated calibration",
            )
            .unwrap();
        assert!(!invalidated.active);
        assert_eq!(
            invalidated.invalidation_reason.as_deref(),
            Some("trusted_search_reconciled:was_active")
        );
        let mut derived_drift = invalidated.clone();
        derived_drift.updated_at = format!("{}-drift", derived_drift.updated_at);
        target
            .upsert_json(
                CALIBRATION_TABLE,
                &derived_drift.calibration_generation,
                &derived_drift,
            )
            .unwrap();
        assert!(target
            .rollback_identity_bundle_import(applied.rollback.clone())
            .unwrap_err()
            .contains("derived Match state changed"));
        target
            .upsert_json(
                CALIBRATION_TABLE,
                &invalidated.calibration_generation,
                &invalidated,
            )
            .unwrap();
        target
            .rollback_identity_bundle_import(applied.rollback)
            .unwrap();
        let local = target.capture_exchange_derived_snapshot_unlocked().unwrap();
        assert_eq!(local.trusted_search_rows, before.trusted_search_rows);
        assert_eq!(local.trusted_index_build, before.trusted_index_build);
        assert_eq!(local.calibrations, before.calibrations);

        let second_apply = target
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        target.apply_identity_bundle_import(second_apply).unwrap();
        drop(target);

        let reopened = MatchStore::open(&target_root.0).unwrap();
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&media_root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let restart_plan = reopened
            .preview_identity_bundle_import(
                &recovery_path,
                &relocations,
                IdentityImportMode::Replace,
            )
            .unwrap();
        reopened
            .rollback_identity_bundle_import_file(
                &recovery_path,
                &relocations,
                &restart_plan.plan_token,
            )
            .unwrap();
        let reactivated: CalibrationActivation = reopened
            .require(
                CALIBRATION_TABLE,
                &active_calibration.calibration_generation,
                "reactivated calibration",
            )
            .unwrap();
        assert!(reactivated.active);
        assert!(reactivated.invalidation_reason.is_none());
        assert_eq!(
            reopened.trusted_gallery_members_digest().unwrap(),
            reactivated.gallery_members_digest
        );
    }

    #[test]
    fn historical_prefixed_fingerprints_export_clear_and_restore_without_rewriting_truth() {
        fn prefix_fixture_fingerprints(value: &mut Value) {
            match value {
                Value::Array(values) => values.iter_mut().for_each(prefix_fixture_fingerprints),
                Value::Object(fields) => {
                    for (key, value) in fields {
                        if key == "media_fingerprint" {
                            if let Some(digest) = value.as_str().and_then(canonical_media_sha256) {
                                *value = Value::String(format!("sha256:{digest}"));
                            }
                        } else if matches!(key.as_str(), "before_json" | "after_json") {
                            let mut decoded: Value =
                                serde_json::from_str(value.as_str().unwrap()).unwrap();
                            prefix_fixture_fingerprints(&mut decoded);
                            *value = Value::String(serde_json::to_string(&decoded).unwrap());
                        } else {
                            prefix_fixture_fingerprints(value);
                        }
                    }
                }
                _ => {}
            }
        }
        let root = TestRoot::new("historical-prefixed-exchange");
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        let store = MatchStore::open(&root.0).unwrap();
        seed_portable_graph(&store, &media_root);
        let mut rows =
            stored_graph_rows(&store.collect_stored_identity_graph_unlocked().unwrap()).unwrap();
        for row in &mut rows {
            // Direct-operation mappings have always stored canonical raw
            // digests; preserve those while recreating old worker Face data.
            if row.table != "match_correction_media_operation" {
                prefix_fixture_fingerprints(&mut row.value);
            }
        }
        for embedding in store.list::<FaceEmbedding>(EMBEDDING_TABLE).unwrap() {
            let mut value = serde_json::to_value(&embedding).unwrap();
            prefix_fixture_fingerprints(&mut value);
            rows.push(OwnedPortableRow {
                table: EMBEDDING_TABLE.to_string(),
                stable_id: embedding.embedding_id,
                value,
            });
        }
        store
            .commit_owned_exchange_rows_unlocked(&rows, &[])
            .unwrap();
        let db = store.store.db();
        surreal_store::run(async move {db.query("REMOVE TABLE match_media_context; UPDATE match_schema_state:global SET schema_version = 20;").await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;Ok(())}).unwrap();
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root.0)).unwrap();
        let store = MatchStore::open(&root.0).unwrap();
        // Upgrade creates the canonical source table; stale metadata and empty
        // tombstones remain portable without claiming current physical bytes.
        let stale_context = CanonicalMediaContext {
            media_key: "media/a.jpg".into(),
            media_fingerprint: "f".repeat(64),
            revision: 4,
            capture_unix_millis: Some(1234),
            time_window_millis: Some(50),
            album_ids: vec!["album-1".into()],
        };
        let tombstone = CanonicalMediaContext {
            media_key: "media/context-tombstone.jpg".into(),
            media_fingerprint: "e".repeat(64),
            revision: 8,
            capture_unix_millis: None,
            time_window_millis: None,
            album_ids: Vec::new(),
        };
        for row in [&stale_context, &tombstone] {
            store
                .upsert_json(
                    super::super::context::MEDIA_CONTEXT_TABLE,
                    &row.media_key,
                    row,
                )
                .unwrap();
        }
        let context_face = store.list::<FaceObservation>(FACE_TABLE).unwrap().remove(0);
        // Exercise all three WP086 portable table counters through real clear/restore.
        let mut tracker = crate::match_video::VideoTracker::new(
            canonical_media_sha256(&context_face.media_fingerprint)
                .unwrap()
                .into(),
            0,
            crate::match_video::VideoPolicy::default(),
        )
        .unwrap();
        let observation = tracker
            .ingest(crate::match_video::VideoFrame {
                stream_index: 0,
                playback_origin: crate::match_video::VideoTime::default(),
                time: crate::match_video::VideoTime {
                    pts: 100,
                    numerator: 1,
                    denominator: 1000,
                },
                frame_sha256: "a".repeat(64),
                scene_score: 0.0,
                detections: vec![crate::match_video::VideoDetection {
                    source_index: 0,
                    bounds: context_face.bounds_normalized.clone().try_into().unwrap(),
                    quality: 0.8,
                    pose_bucket: "front".into(),
                    detector_generation: "portable-video-fixture".into(),
                }],
            })
            .unwrap()
            .observations
            .remove(0);
        let mut video_face = context_face.clone();
        video_face.face_id = format!("video-face-{}", observation.observation_id);
        video_face.source_index = context_face.source_index.checked_add(1).unwrap();
        store
            .upsert_json(FACE_TABLE, &video_face.face_id, &video_face)
            .unwrap();
        let video = super::super::video::StoredVideoObservation {
            observation_id: observation.observation_id.clone(),
            face_id: video_face.face_id,
            track_id: observation.track_id.clone(),
            media_key: video_face.media_key,
            media_fingerprint: video_face.media_fingerprint,
            revision: 1,
            closed: true,
            exemplar: true,
            payload: serde_json::to_string(&observation).unwrap(),
        };
        video.observation().unwrap();
        store
            .upsert_json(
                super::super::video::VIDEO_OBSERVATION_TABLE,
                &video.observation_id,
                &video,
            )
            .unwrap();
        let context_person = store.list::<Person>(PERSON_TABLE).unwrap().remove(0);
        let review = StoredReviewContext {
            context_id: format!(
                "{:x}",
                Sha256::digest(format!(
                    "{}\0{}",
                    context_face.face_id, context_person.person_id
                ))
            ),
            face_id: context_face.face_id,
            person_id: context_person.person_id,
            payload: "[]".into(),
        };
        store
            .upsert_json(
                super::super::context::CONTEXT_TABLE,
                &review.context_id,
                &review,
            )
            .unwrap();
        let before = store.collect_stored_identity_graph_unlocked().unwrap();
        assert!(before
            .faces
            .iter()
            .all(|face| face.media_fingerprint.starts_with("sha256:")));
        let before_rows = stored_graph_rows(&before).unwrap();
        let raw = fs::read(media_root.join("media/a.jpg")).unwrap();
        let path = root.0.join("historical-recovery.json");
        let preview = store.preview_clear_all_match_data(&path).unwrap();
        let exported = MatchStore::read_identity_bundle(&path).unwrap();
        assert_eq!(
            preview.recovery_bundle.table_counts[super::super::context::MEDIA_CONTEXT_TABLE],
            2
        );
        assert_eq!(
            preview.recovery_bundle.table_counts[super::super::context::CONTEXT_TABLE],
            1
        );
        assert_eq!(exported.graph.faces, before.faces);
        assert_eq!(
            preview.recovery_bundle.table_counts[super::super::video::VIDEO_OBSERVATION_TABLE],
            1
        );
        assert_eq!(exported.graph.video_observations, before.video_observations);
        assert_eq!(exported.graph.operations, before.operations);
        assert_eq!(exported.graph.media_context, before.media_context);
        let clear = store.clear_all_match_data(&preview).unwrap();
        assert_eq!(store.count(FACE_TABLE).unwrap(), 0);
        assert_eq!(
            store
                .count(super::super::context::MEDIA_CONTEXT_TABLE)
                .unwrap(),
            0
        );
        let relocations = before
            .roots
            .iter()
            .map(|root| (root.root_id.clone(), root.path.clone()))
            .collect();
        store
            .restore_clear_recovery_bundle(
                &path,
                &relocations,
                &clear.recovery_bundle.restore_token,
            )
            .unwrap();
        let restored = store.collect_stored_identity_graph_unlocked().unwrap();
        assert_eq!(stored_graph_rows(&restored).unwrap(), before_rows);
        assert_eq!(
            store.media_context(&stale_context.media_key).unwrap(),
            Some(stale_context)
        );
        assert_eq!(
            store.media_context(&tombstone.media_key).unwrap(),
            Some(tombstone)
        );
        assert_eq!(fs::read(media_root.join("media/a.jpg")).unwrap(), raw);
        for invalid in [
            "sha256:BAD".to_string(),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:sha256:{}", "a".repeat(64)),
        ] {
            let mut bad = exported.clone();
            bad.graph.faces[0].media_fingerprint = invalid;
            assert!(validate_identity_graph(&bad.graph).is_err());
        }
    }

    #[test]
    fn identity_bundle_roundtrip_relocation_no_promotion_and_xmp_sidecar() {
        let source_root = TestRoot::new("source");
        let source = MatchStore::open(&source_root.0).unwrap();
        let media_root = source_root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&source, &media_root);
        let bundle_path = source_root.0.join("identity.json");
        let exported = source.export_identity_bundle(&bundle_path).unwrap();

        let target_root = TestRoot::new("target");
        let relocated = target_root.0.join("relocated-media");
        fs::create_dir_all(&relocated).unwrap();
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let path = relocated.join(media_key);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(media_root.join(media_key), path).unwrap();
        }
        let target = MatchStore::open(&target_root.0).unwrap();
        let canonical_relocated = fs::canonicalize(&relocated).unwrap();
        let supplied_relocation = relocated.to_string_lossy().to_string();
        #[cfg(windows)]
        let supplied_relocation = if let Some(unc) = supplied_relocation.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            supplied_relocation
                .strip_prefix(r"\\?\")
                .unwrap_or(&supplied_relocation)
                .to_string()
        };
        #[cfg(windows)]
        assert!(!supplied_relocation.starts_with(r"\\?\"));
        let relocations = BTreeMap::from([("root-a".to_string(), supplied_relocation)]);
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(plan.conflicts.is_empty());
        let initial_receipt = target.apply_identity_bundle_import(plan).unwrap();
        let restored_root = target.index_root("root-a").unwrap();
        assert_eq!(restored_root.root_id, "root-a");
        assert_eq!(
            Path::new(&restored_root.path),
            canonical_relocated.as_path()
        );
        assert_eq!(
            fs::canonicalize(&restored_root.path).unwrap(),
            PathBuf::from(&restored_root.path)
        );
        let local_rollback = target
            .rollback_identity_bundle_import(initial_receipt.rollback.clone())
            .unwrap();
        assert!(local_rollback.identity_revision > initial_receipt.identity_revision);
        assert!(local_rollback.catalog_revision > initial_receipt.catalog_revision);
        assert!(target
            .get_one::<Person>(PERSON_TABLE, "person-a")
            .unwrap()
            .is_none());
        let reimport = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        let receipt = target.apply_identity_bundle_import(reimport).unwrap();
        assert!(receipt.identity_revision > local_rollback.identity_revision);
        assert!(receipt.catalog_revision > local_rollback.catalog_revision);
        assert_eq!(receipt.content_sha256, exported.content_sha256);
        assert_eq!(
            receipt.summary().identity_revision,
            receipt.identity_revision
        );
        assert_eq!(receipt.summary().catalog_revision, receipt.catalog_revision);
        let assignment: Assignment = target
            .require(ASSIGNMENT_TABLE, "face-a", "assignment")
            .unwrap();
        assert_eq!(assignment.state, "operator_confirmed");
        assert!(target
            .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-suggestion")
            .unwrap()
            .is_none());
        assert!(target
            .get_one::<Suggestion>(
                SUGGESTION_TABLE,
                &suggestion_id("face-suggestion", "person-a"),
            )
            .unwrap()
            .is_some());
        let strict: Assignment = target
            .require(ASSIGNMENT_TABLE, "face-strict", "strict assignment")
            .unwrap();
        assert_eq!(strict.state, "committed_strict_automatic");
        assert!(!strict.locked);
        assert_eq!(strict.model_generation.as_deref(), Some("model-v1"));
        assert_eq!(
            strict.calibration_generation.as_deref(),
            Some("calibration-v1")
        );
        assert_eq!(
            target
                .list::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)
                .unwrap()
                .len(),
            0
        );
        assert!(target
            .get_one::<MatchOperation>(OPERATION_TABLE, "operation-history")
            .unwrap()
            .is_some());
        let pre_import_fence = target
            .operator_mutation_fence("face-a", "person-a")
            .unwrap();
        let second = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(second.already_applied);
        let no_op = target.apply_identity_bundle_import(second).unwrap();
        assert!(no_op.already_applied);
        assert_eq!(no_op.created, 0);
        assert_eq!(no_op.updated, 0);
        assert_eq!(no_op.deleted, 0);
        assert_eq!(no_op.identity_revision, receipt.identity_revision);
        assert_eq!(no_op.catalog_revision, receipt.catalog_revision);

        let recovery_path = target_root.0.join("pre-change-recovery.json");
        target
            .export_identity_recovery_bundle(&recovery_path)
            .unwrap();
        let mut changed: Person = target.require(PERSON_TABLE, "person-a", "Person").unwrap();
        changed.name = "Conflicting name".to_string();
        target
            .upsert_json(PERSON_TABLE, &changed.person_id, &changed)
            .unwrap();
        let merge = target
            .preview_identity_bundle_import(&recovery_path, &relocations, IdentityImportMode::Merge)
            .unwrap();
        assert_eq!(merge.conflicts.len(), 1);
        let mut mode_mutated = merge.clone();
        mode_mutated.mode = IdentityImportMode::Replace;
        let error = target
            .apply_identity_bundle_import(mode_mutated)
            .unwrap_err();
        assert!(
            error.contains("mutated identity import plan token"),
            "{error}"
        );
        let still_conflicting: Person = target.require(PERSON_TABLE, "person-a", "Person").unwrap();
        assert_eq!(still_conflicting.name, "Conflicting name");
        assert!(target.apply_identity_bundle_import(merge).is_err());
        let replace = target
            .preview_identity_bundle_import(
                &recovery_path,
                &relocations,
                IdentityImportMode::Replace,
            )
            .unwrap();
        let rollback_token = replace.plan_token.clone();
        drop(target);
        let target = MatchStore::open(&target_root.0).unwrap();
        let rollback_receipt = target
            .rollback_identity_bundle_import_file(&recovery_path, &relocations, &rollback_token)
            .unwrap();
        assert!(rollback_receipt.identity_revision > no_op.identity_revision);
        assert!(rollback_receipt.catalog_revision > no_op.catalog_revision);
        let restored: Person = target.require(PERSON_TABLE, "person-a", "Person").unwrap();
        assert_eq!(restored.name, "Alice & Bob");
        assert!(target
            .move_to_look("face-a", "look-a", &pre_import_fence)
            .unwrap_err()
            .contains("stale operator identity mutation fence"));

        let original_path = target_root.0.join("media-a.jpg");
        fs::write(&original_path, b"original-media-bytes").unwrap();
        let original_hash = sha256_bytes(&fs::read(&original_path).unwrap());
        let xmp_path = target_root.0.join("media-a.xmp");
        let preview = target
            .preview_mwg_xmp_sidecar("media/a.jpg", &xmp_path)
            .unwrap();
        assert_eq!(preview.regions.len(), 1);
        target
            .export_mwg_xmp_sidecar("media/a.jpg", &xmp_path, &preview.preview_token)
            .unwrap();
        let import = MatchStore::preview_mwg_xmp_import(&xmp_path).unwrap();
        let staged = MatchStore::import_mwg_xmp_sidecar(&xmp_path, &import.preview_token).unwrap();
        assert_eq!(staged.staged_regions, preview.regions);
        assert_eq!(staged.applied_match_rows, 0);
        assert_eq!(staged.original_media_rows_mutated, 0);
        assert_eq!(
            sha256_bytes(&fs::read(&original_path).unwrap()),
            original_hash
        );
    }

    #[test]
    fn identity_bundle_rejects_hash_duplicate_dangling_path_and_limits_before_mutation() {
        let root = TestRoot::new("negative");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let before = store.count(PERSON_TABLE).unwrap();
        let stale_output = root.0.join("stale-identity.json");
        assert!(store
            .export_identity_bundle_expected(&stale_output, &"0".repeat(64))
            .unwrap_err()
            .contains("stale identity export preview"));
        assert!(!stale_output.exists());
        let mut bundle = store.build_identity_bundle().unwrap();

        bundle.manifest.content_sha256 = "0".repeat(64);
        let bytes = canonical_bundle_bytes(&bundle).unwrap();
        assert!(store
            .preview_identity_bundle_import_bytes(
                &bytes,
                &BTreeMap::new(),
                IdentityImportMode::Replace
            )
            .unwrap_err()
            .contains("content hash mismatch"));

        bundle = store.build_identity_bundle().unwrap();
        bundle.graph.people.push(bundle.graph.people[0].clone());
        bundle.manifest.content_sha256 = bundle_content_sha256(&bundle.graph).unwrap();
        let bytes = canonical_bundle_bytes(&bundle).unwrap();
        assert!(store
            .preview_identity_bundle_import_bytes(
                &bytes,
                &BTreeMap::new(),
                IdentityImportMode::Replace
            )
            .unwrap_err()
            .contains("duplicate Person"));

        bundle = store.build_identity_bundle().unwrap();
        bundle.graph.looks[0].person_id = "missing-person".to_string();
        bundle.manifest.content_sha256 = bundle_content_sha256(&bundle.graph).unwrap();
        let bytes = canonical_bundle_bytes(&bundle).unwrap();
        assert!(store
            .preview_identity_bundle_import_bytes(
                &bytes,
                &BTreeMap::new(),
                IdentityImportMode::Replace
            )
            .unwrap_err()
            .contains("dangling Look Person"));

        bundle = store.build_identity_bundle().unwrap();
        bundle.graph.roots[0].portable_path = "../escape".to_string();
        bundle.manifest.content_sha256 = bundle_content_sha256(&bundle.graph).unwrap();
        let bytes = canonical_bundle_bytes(&bundle).unwrap();
        assert!(store
            .preview_identity_bundle_import_bytes(
                &bytes,
                &BTreeMap::new(),
                IdentityImportMode::Replace
            )
            .unwrap_err()
            .contains("canonical relative"));

        bundle = store.build_identity_bundle().unwrap();
        let strict = bundle
            .graph
            .assignments
            .iter_mut()
            .find(|row| row.face_id == "face-strict")
            .unwrap();
        strict.state = "operator_confirmed".to_string();
        bundle.manifest.content_sha256 = bundle_content_sha256(&bundle.graph).unwrap();
        let bytes = canonical_bundle_bytes(&bundle).unwrap();
        assert!(store
            .preview_identity_bundle_import_bytes(
                &bytes,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap_err()
            .contains("operator-confirmed evidence semantics"));
        assert_eq!(store.count(PERSON_TABLE).unwrap(), before);

        let exact = Value::String("x".repeat(IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES));
        let mut total = 0;
        let mut depth = 0;
        inspect_json_limits(&exact, 1, &mut depth, &mut total).unwrap();
        let over = Value::String("x".repeat(IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES + 1));
        let mut total = 0;
        let mut depth = 0;
        assert!(inspect_json_limits(&over, 1, &mut depth, &mut total).is_err());
        assert!(validate_relocation_root(&root.0.join("missing").to_string_lossy()).is_err());
    }

    #[test]
    fn portable_graph_rejects_stale_or_semantically_impossible_match_evidence() {
        fn bind_trusted_membership_owner(graph: &mut IdentityBundleGraph) {
            let membership = graph.trusted_memberships[0].clone();
            let owner = graph
                .operations
                .iter_mut()
                .find(|operation| operation.operation_id == membership.operation_id)
                .unwrap();
            owner.face_id = Some(membership.face_id.clone());
            owner.after_json = serde_json::to_string(&membership).unwrap();
        }

        let root = TestRoot::new("graph-integrity");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let graph = store.build_identity_bundle().unwrap().graph;
        validate_identity_graph(&graph).unwrap();
        let mut missing_direct_assignment = graph.clone();
        missing_direct_assignment.people[0].cover_media_key = None;
        missing_direct_assignment
            .assignments
            .retain(|assignment| assignment.operation_id != "operation-a");
        assert!(validate_identity_graph(&missing_direct_assignment).is_err());
        let mut cross_person_cover = graph.clone();
        cross_person_cover.people[0].cover_media_key = Some("media/b.jpg".to_string());
        assert!(validate_identity_graph(&cross_person_cover)
            .unwrap_err()
            .contains("cover must reference operator-confirmed media"));
        let mut missing_cover = graph.clone();
        missing_cover.people[0].cover_media_key = Some("media/missing.jpg".to_string());
        assert!(validate_identity_graph(&missing_cover)
            .unwrap_err()
            .contains("cover must reference operator-confirmed media"));

        let mut invalid = graph.clone();
        invalid.assignments[0].operation_id = "operation-membership".to_string();
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("is not authorized by operation kind authorize_trusted_reference"));

        let mut invalid = graph.clone();
        let assigned_face = invalid
            .faces
            .iter()
            .find(|face| face.face_id == "face-a")
            .unwrap()
            .clone();
        let suggestion = invalid.suggestions.first_mut().unwrap();
        suggestion.face_id = assigned_face.face_id.clone();
        suggestion.suggestion_id =
            suggestion_id(&suggestion.face_id, &suggestion.candidate_person_id);
        suggestion.media_fingerprint = assigned_face.media_fingerprint;
        suggestion.face_revision = assigned_face.face_revision;
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("conflicts with a committed assignment"));

        let mut invalid = graph.clone();
        invalid.suggestions[0].suggestion_id = "forged-suggestion-id".to_string();
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("canonical Face/Person logical ID"));
        let mut invalid = graph.clone();
        invalid.suggestions[0].face_revision += 1;
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("stale revision, fingerprint, score, or model provenance"));
        let mut invalid = graph.clone();
        invalid.suggestions[0].media_fingerprint = "f".repeat(64);
        let error = validate_identity_graph(&invalid).unwrap_err();
        assert!(
            error.contains("stale revision, fingerprint, score, or model provenance"),
            "{error}"
        );
        let mut invalid = graph.clone();
        invalid.suggestions[0].person_revision += 1;
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("stale revision, fingerprint, score, or model provenance"));

        let mut invalid = graph.clone();
        invalid.assignments[0].face_revision += 1;
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("stale Face/Person revision or media provenance"));
        let mut invalid = graph.clone();
        invalid.assignments[0].person_revision += 1;
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("stale Face/Person revision or media provenance"));
        let mut invalid = graph.clone();
        let strict = invalid
            .assignments
            .iter_mut()
            .find(|assignment| assignment.face_id == "face-strict")
            .unwrap();
        strict.provenance = "forged-strict-provenance".to_string();
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("invalid strict-recognition provenance or generation"));
        let mut invalid = graph.clone();
        let strict = invalid
            .assignments
            .iter_mut()
            .find(|assignment| assignment.face_id == "face-strict")
            .unwrap();
        strict.envelope_hash = Some("not-a-digest".to_string());
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("strict assignment envelope hash"));

        let mut invalid = graph.clone();
        invalid.trusted_memberships[0].membership_id = "forged-membership".to_string();
        bind_trusted_membership_owner(&mut invalid);
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));
        let mut invalid = graph.clone();
        invalid.trusted_memberships[0].authorized = false;
        bind_trusted_membership_owner(&mut invalid);
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));
        let mut invalid = graph.clone();
        invalid.trusted_memberships[0].quality_threshold = TRUSTED_MIN_QUALITY + 0.01;
        bind_trusted_membership_owner(&mut invalid);
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));
        let mut invalid = graph.clone();
        invalid.trusted_memberships[0].pose_bucket = "profile".to_string();
        bind_trusted_membership_owner(&mut invalid);
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));
        let mut invalid = graph.clone();
        invalid.model_generations[0].state = "usable".to_string();
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));

        let mut invalid = graph.clone();
        let peer_face = face("face-trusted-peer", "media/peer.jpg");
        let mut peer_assignment = invalid
            .assignments
            .iter()
            .find(|assignment| assignment.face_id == "face-a")
            .unwrap()
            .clone();
        peer_assignment.assignment_id = peer_face.face_id.clone();
        peer_assignment.face_id = peer_face.face_id.clone();
        peer_assignment.media_key = peer_face.media_key.clone();
        peer_assignment.face_revision = peer_face.face_revision;
        peer_assignment.operation_id = "operation-peer-assignment".to_string();
        let peer_assignment_operation = MatchOperation {
            operation_id: peer_assignment.operation_id.clone(),
            kind: "assign_operator_confirmed".to_string(),
            face_id: Some(peer_face.face_id.clone()),
            person_id: Some(peer_assignment.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&peer_assignment).unwrap(),
            reversible: true,
            created_at: peer_assignment.updated_at.clone(),
        };
        let mut peer_membership = invalid.trusted_memberships[0].clone();
        peer_membership.face_id = peer_face.face_id.clone();
        peer_membership.membership_id =
            trusted_member_id(&peer_membership.set_id, &peer_membership.face_id);
        peer_membership.media_fingerprint = peer_face.media_fingerprint.clone();
        peer_membership.face_revision = peer_face.face_revision;
        peer_membership.quality_score = peer_face.quality;
        peer_membership.pose_bucket = peer_face.pose_bucket.clone();
        peer_membership.embedding_id =
            embedding_id(&peer_membership.face_id, &peer_membership.model_generation);
        peer_membership.operation_id = "operation-peer-membership".to_string();
        let peer_membership_operation = MatchOperation {
            operation_id: peer_membership.operation_id.clone(),
            kind: "authorize_trusted_reference".to_string(),
            face_id: Some(peer_face.face_id.clone()),
            person_id: Some(peer_assignment.person_id.clone()),
            before_json: "null".to_string(),
            after_json: serde_json::to_string(&peer_membership).unwrap(),
            reversible: true,
            created_at: peer_membership.created_at.clone(),
        };
        invalid.faces.push(peer_face);
        invalid.assignments.push(peer_assignment);
        invalid.operations.push(peer_assignment_operation);
        invalid.trusted_memberships.push(peer_membership);
        invalid.operations.push(peer_membership_operation);
        assert!(validate_identity_graph(&invalid)
            .unwrap_err()
            .contains("trusted-reference authorization evidence contract"));
    }

    #[test]
    fn every_closed_correction_kind_rejects_cross_kind_typed_effects() {
        let root = TestRoot::new("closed-correction-kind-contracts");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let graph = store.build_identity_bundle().unwrap().graph;
        let before = graph.assignments[0].clone();
        let mut after = before.clone();
        after.operation_id = "operation-cross-kind".to_string();
        after.updated_at = "2026-08-25T23:59:59Z".to_string();
        let row = CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: before.assignment_id.clone(),
            before: Some(serde_json::to_value(&before).unwrap()),
            after: Some(serde_json::to_value(&after).unwrap()),
        };
        for kind in [
            "remove_person",
            "manual_face",
            "ignored",
            "delete_face_analysis",
        ] {
            let envelope = ExchangeCorrectionDeltaEnvelope {
                version: 1,
                kind: kind.to_string(),
                rows: vec![row.clone()],
                face_ids: vec![before.face_id.clone()],
                media_keys: vec![before.media_key.clone()],
                identity_changed: true,
                catalog_changed: matches!(kind, "remove_person"),
            };
            let before_rows = vec![(
                CorrectionTable::Assignment,
                before.assignment_id.clone(),
                Some(serde_json::to_value(&before).unwrap()),
            )];
            let operation = MatchOperation {
                operation_id: "operation-cross-kind".to_string(),
                kind: format!("correction_{kind}"),
                face_id: if matches!(kind, "manual_face" | "delete_face_analysis") {
                    Some(before.face_id.clone())
                } else {
                    None
                },
                person_id: Some(before.person_id.clone()),
                before_json: serde_json::to_string(&before_rows).unwrap(),
                after_json: serde_json::to_string(&envelope).unwrap(),
                reversible: true,
                created_at: before.updated_at.clone(),
            };
            assert!(
                validate_correction_operation(&operation, &envelope).is_err(),
                "cross-kind Assignment effect was accepted as {kind}"
            );
        }
    }

    #[test]
    fn change_person_kinds_bind_source_cannot_links_and_target_assignments() {
        let root = TestRoot::new("change-person-constraint-contract");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let graph = store.build_identity_bundle().unwrap().graph;
        let source = graph.assignments[0].clone();

        for (kind, operation_id, face_id) in [
            (
                "change_person",
                "operation-change-person-contract",
                Some(source.face_id.clone()),
            ),
            (
                "batch_change_person",
                "operation-batch-change-person-contract",
                None,
            ),
        ] {
            let target_person_id = "person-change-target".to_string();
            let mut target = source.clone();
            target.person_id = target_person_id.clone();
            target.look_id = None;
            target.placement = "unsorted".to_string();
            target.operation_id = operation_id.to_string();
            target.provenance = kind.to_string();
            target.updated_at = "2026-08-25T23:59:59Z".to_string();
            let constraint = CannotLinkConstraint {
                constraint_id: cannot_link_id(&source.face_id, &source.person_id),
                face_id: source.face_id.clone(),
                person_id: source.person_id.clone(),
                operation_id: operation_id.to_string(),
                operator_owned: true,
                created_at: target.updated_at.clone(),
            };
            let rows = vec![
                CorrectionRowDelta {
                    table: CorrectionTable::Assignment,
                    stable_id: source.assignment_id.clone(),
                    before: Some(serde_json::to_value(&source).unwrap()),
                    after: Some(serde_json::to_value(&target).unwrap()),
                },
                CorrectionRowDelta {
                    table: CorrectionTable::Constraint,
                    stable_id: constraint.constraint_id.clone(),
                    before: None,
                    after: Some(serde_json::to_value(&constraint).unwrap()),
                },
            ];
            let envelope = ExchangeCorrectionDeltaEnvelope {
                version: 1,
                kind: kind.to_string(),
                rows: rows.clone(),
                face_ids: vec![source.face_id.clone()],
                media_keys: vec![source.media_key.clone()],
                identity_changed: true,
                catalog_changed: false,
            };
            let before_rows = rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>();
            let operation = MatchOperation {
                operation_id: operation_id.to_string(),
                kind: format!("correction_{kind}"),
                face_id,
                person_id: Some(target_person_id.clone()),
                before_json: serde_json::to_string(&before_rows).unwrap(),
                after_json: serde_json::to_string(&envelope).unwrap(),
                reversible: true,
                created_at: target.updated_at.clone(),
            };
            validate_correction_operation(&operation, &envelope).unwrap();

            if kind == "batch_change_person" {
                let mut source_suggestion = graph.suggestions[0].clone();
                source_suggestion.suggestion_id = suggestion_id(&source.face_id, &source.person_id);
                source_suggestion.face_id = source.face_id.clone();
                source_suggestion.candidate_person_id = source.person_id.clone();
                source_suggestion.person_revision = source.person_revision;
                let mut forged_batch = envelope.clone();
                forged_batch.rows[0].before = None;
                forged_batch.rows.push(CorrectionRowDelta {
                    table: CorrectionTable::Suggestion,
                    stable_id: source_suggestion.suggestion_id.clone(),
                    before: Some(serde_json::to_value(source_suggestion).unwrap()),
                    after: None,
                });
                let mut forged_batch_operation = operation.clone();
                forged_batch_operation.before_json = serde_json::to_string(
                    &forged_batch
                        .rows
                        .iter()
                        .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                assert!(
                    validate_correction_operation(&forged_batch_operation, &forged_batch)
                        .unwrap_err()
                        .contains("requires a prior Assignment")
                );
            }

            let mut forged_look = envelope.clone();
            forged_look.rows[0].after.as_mut().unwrap()["look_id"] =
                Value::String("look-forged".to_string());
            forged_look.rows[0].after.as_mut().unwrap()["placement"] =
                Value::String("look".to_string());
            assert!(validate_correction_operation(&operation, &forged_look)
                .unwrap_err()
                .contains("Unsorted"));

            let mut forged_media = envelope.clone();
            forged_media.media_keys = vec!["media/forged.jpg".to_string()];
            forged_media.rows[0].after.as_mut().unwrap()["media_key"] =
                Value::String("media/forged.jpg".to_string());
            assert!(validate_correction_operation(&operation, &forged_media)
                .unwrap_err()
                .contains("source identity or media provenance"));

            let mut forged = envelope.clone();
            let forged_constraint_id = cannot_link_id(&source.face_id, &target_person_id);
            forged.rows[1].stable_id = forged_constraint_id.clone();
            forged.rows[1].after.as_mut().unwrap()["constraint_id"] =
                Value::String(forged_constraint_id);
            forged.rows[1].after.as_mut().unwrap()["person_id"] = Value::String(target_person_id);
            let mut forged_operation = operation.clone();
            forged_operation.before_json = serde_json::to_string(
                &forged
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert!(validate_correction_operation(&forged_operation, &forged)
                .unwrap_err()
                .contains("rejected source Person"));
        }
    }

    #[test]
    fn change_person_strict_assignment_sources_require_usable_generation_single_and_batch() {
        let root = TestRoot::new("change-person-strict-source-provenance");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let base = store.build_identity_bundle().unwrap().graph;
        let strict = base
            .assignments
            .iter()
            .find(|row| row.state == "committed_strict_automatic")
            .unwrap()
            .clone();

        for kind in ["change_person", "batch_change_person"] {
            for forgery in ["absent_generation", "retired_generation"] {
                let operation_id = format!("operation-{kind}-{forgery}");
                let mut source = strict.clone();
                if forgery == "absent_generation" {
                    source.model_generation = Some("model-absent-change-person".to_string());
                }
                let mut target = strict.clone();
                target.person_id = "person-change-person-target".to_string();
                target.state = "operator_confirmed".to_string();
                target.provenance = kind.to_string();
                target.locked = true;
                target.model_generation = None;
                target.calibration_generation = None;
                target.envelope_hash = None;
                target.operation_id = operation_id.clone();
                let constraint = CannotLinkConstraint {
                    constraint_id: cannot_link_id(&source.face_id, &source.person_id),
                    face_id: source.face_id.clone(),
                    person_id: source.person_id.clone(),
                    operation_id: operation_id.clone(),
                    operator_owned: true,
                    created_at: target.updated_at.clone(),
                };
                let rows = vec![
                    CorrectionRowDelta {
                        table: CorrectionTable::Assignment,
                        stable_id: source.assignment_id.clone(),
                        before: Some(serde_json::to_value(&source).unwrap()),
                        after: Some(serde_json::to_value(&target).unwrap()),
                    },
                    CorrectionRowDelta {
                        table: CorrectionTable::Constraint,
                        stable_id: constraint.constraint_id.clone(),
                        before: None,
                        after: Some(serde_json::to_value(&constraint).unwrap()),
                    },
                ];
                let envelope = ExchangeCorrectionDeltaEnvelope {
                    version: 1,
                    kind: kind.to_string(),
                    rows: rows.clone(),
                    face_ids: vec![source.face_id.clone()],
                    media_keys: vec![source.media_key.clone()],
                    identity_changed: true,
                    catalog_changed: false,
                };
                let operation = MatchOperation {
                    operation_id: operation_id.clone(),
                    kind: format!("correction_{kind}"),
                    face_id: (kind == "change_person").then(|| source.face_id.clone()),
                    person_id: Some(target.person_id.clone()),
                    before_json: serde_json::to_string(
                        &rows
                            .iter()
                            .map(|row| {
                                (row.table.clone(), row.stable_id.clone(), row.before.clone())
                            })
                            .collect::<Vec<_>>(),
                    )
                    .unwrap(),
                    after_json: serde_json::to_string(&envelope).unwrap(),
                    reversible: true,
                    created_at: target.updated_at.clone(),
                };
                let indexed = index_correction_envelope(&operation).unwrap().unwrap();
                let faces = base
                    .faces
                    .iter()
                    .map(|row| (row.face_id.as_str(), row))
                    .collect();
                let people = base
                    .people
                    .iter()
                    .map(|row| (row.person_id.as_str(), row))
                    .collect();
                let mut generations = base.model_generations.clone();
                if forgery == "retired_generation" {
                    generations[0].state = "retired".to_string();
                }
                let generation_by_id = generations
                    .iter()
                    .map(|row| (row.generation.as_str(), row))
                    .collect();
                let operation_by_id =
                    BTreeMap::from([(operation.operation_id.as_str(), &operation)]);
                let mut anchor_work = 0;
                let error = validate_portable_correction_source_provenance(
                    &operation,
                    &indexed,
                    &faces,
                    &people,
                    &generation_by_id,
                    &[],
                    &BTreeMap::new(),
                    &operation_by_id,
                    &mut anchor_work,
                )
                .unwrap_err();
                assert!(
                    error.contains("strict Assignment source"),
                    "unexpected {kind} {forgery} error: {error}"
                );
            }
        }
    }

    #[test]
    fn suggestion_same_provenance_survives_monotonic_person_updates_and_removal() {
        let root = TestRoot::new("suggestion-same-person-lifecycle");
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        let store = MatchStore::open(&root.0).unwrap();
        seed_portable_graph(&store, &media_root);
        let face: FaceObservation = store
            .require(FACE_TABLE, "face-suggestion", "suggested Face")
            .unwrap();
        let execution = store.execution_state().unwrap();
        let asset = JobAsset {
            asset_id: "asset-suggestion-same-lifecycle".to_string(),
            job_id: "job-history".to_string(),
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
            model_generation: "model-v1".to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: "2026-08-25T00:00:01Z".to_string(),
        };
        store
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        let fence = store
            .correction_fence_for(&face.face_id, vec!["person-a".to_string()])
            .unwrap();
        let same = store
            .same_correction(&face.face_id, "person-a", &fence)
            .unwrap();

        let person: Person = store.require(PERSON_TABLE, "person-a", "Person").unwrap();
        let revised = store
            .update_person(
                &person.person_id,
                person.revision,
                "Alice Same history revised",
                vec!["Same history".to_string()],
            )
            .unwrap();
        let revised = store
            .update_person_preferences(
                &revised.person_id,
                revised.revision,
                revised.cover_media_key.clone(),
                true,
                true,
            )
            .unwrap();
        let revised_bundle = store.build_identity_bundle().unwrap();
        let provenance = revised_bundle
            .graph
            .suggestion_source_provenance
            .iter()
            .find(|row| row.operation_id == same.operation_id)
            .unwrap();
        assert_eq!(provenance.person_revision, 1);
        assert!(revised.revision > provenance.person_revision);

        let removal = store.preview_remove_person(&revised.person_id).unwrap();
        store.remove_person(&removal).unwrap();
        let removed_bundle = store.build_identity_bundle().unwrap();
        assert!(removed_bundle
            .graph
            .suggestion_source_provenance
            .iter()
            .any(|row| row.operation_id == same.operation_id && row.person_revision == 1));
    }

    #[test]
    fn merge_lineage_accepts_an_ordinary_person_update_between_merges() {
        let root = TestRoot::new("merge-ordinary-update-merge");
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        let store = MatchStore::open(&root.0).unwrap();
        seed_portable_graph(&store, &media_root);

        let first_source = store.create_person("First source", Vec::new()).unwrap();
        let first_merge = store
            .preview_merge_people(&first_source.person_id, "person-a")
            .unwrap();
        store.merge_people(&first_merge).unwrap();

        let target: Person = store.require(PERSON_TABLE, "person-a", "Person").unwrap();
        store
            .update_person(
                &target.person_id,
                target.revision,
                "Alice between merges",
                vec!["Between merges".to_string()],
            )
            .unwrap();

        let second_source = store.create_person("Second source", Vec::new()).unwrap();
        let second_merge = store
            .preview_merge_people(&second_source.person_id, "person-a")
            .unwrap();
        store.merge_people(&second_merge).unwrap();

        let bundle = store.build_identity_bundle().unwrap();
        assert_eq!(
            bundle
                .graph
                .operations
                .iter()
                .filter(|operation| operation.kind == "correction_merge_people")
                .count(),
            2
        );
    }

    #[test]
    fn suggestion_backed_change_person_exports_and_imports_as_unsorted_with_source_constraint() {
        let source_root = TestRoot::new("suggestion-change-person-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);

        let suggested_face: FaceObservation = source
            .require(FACE_TABLE, "face-suggestion", "suggested Face")
            .unwrap();
        let execution = source.execution_state().unwrap();
        let asset = JobAsset {
            asset_id: "asset-suggestion-change-person".to_string(),
            job_id: "job-history".to_string(),
            media_key: suggested_face.media_key.clone(),
            source_path: None,
            media_fingerprint: suggested_face.media_fingerprint.clone(),
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
            model_generation: "model-v1".to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: "2026-08-25T00:00:01Z".to_string(),
        };
        source
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();
        let target_person = source.create_person("Target", Vec::new()).unwrap();
        let fence = source
            .correction_fence_for(
                &suggested_face.face_id,
                vec!["person-a".to_string(), target_person.person_id.clone()],
            )
            .unwrap();
        source
            .change_person_correction(
                &suggested_face.face_id,
                "person-a",
                &target_person.person_id,
                &fence,
            )
            .unwrap();

        let bundle = source.build_identity_bundle().unwrap();
        let assignment = bundle
            .graph
            .assignments
            .iter()
            .find(|assignment| assignment.face_id == suggested_face.face_id)
            .unwrap();
        assert_eq!(assignment.person_id, target_person.person_id);
        assert_eq!(assignment.look_id, None);
        assert_eq!(assignment.placement, "unsorted");
        assert!(bundle.graph.constraints.iter().any(|constraint| {
            constraint.face_id == suggested_face.face_id && constraint.person_id == "person-a"
        }));

        for forgery in [
            "face_revision",
            "empty_fingerprint",
            "zero_person_revision",
            "missing_generation",
            "retired_generation",
            "empty_job",
            "incomplete_calibration",
        ] {
            let mut forged_graph = bundle.graph.clone();
            let mut retired_generation = None;
            if forgery == "retired_generation" {
                let mut generation = forged_graph.model_generations.first().unwrap().clone();
                generation.generation = "model-retired-history".to_string();
                generation.state = "retired".to_string();
                generation.validated = true;
                retired_generation = Some(generation.generation.clone());
                forged_graph.model_generations.push(generation);
            }
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|operation| operation.kind == "correction_change_person")
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let row = envelope
                .rows
                .iter_mut()
                .find(|row| {
                    row.table == CorrectionTable::Suggestion
                        && row.before.is_some()
                        && row.after.is_none()
                })
                .unwrap();
            let mut historical: Suggestion =
                serde_json::from_value(row.before.clone().unwrap()).unwrap();
            match forgery {
                "face_revision" => historical.face_revision += 1,
                "empty_fingerprint" => historical.media_fingerprint.clear(),
                "zero_person_revision" => historical.person_revision = 0,
                "missing_generation" => {
                    historical.model_generation = "model-missing-history".to_string()
                }
                "retired_generation" => historical.model_generation = retired_generation.unwrap(),
                "empty_job" => historical.job_id.clear(),
                "incomplete_calibration" => {
                    historical.calibration_generation = Some("calibration-forged".to_string());
                    historical.envelope_hash = None;
                }
                _ => unreachable!(),
            }
            row.before = Some(serde_json::to_value(historical).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            let error = validate_identity_graph(&forged_graph).unwrap_err();
            assert!(
                error.contains("historical Suggestion")
                    || error.contains("historical correction Suggestion")
                    || error.contains("historical portable Suggestion"),
                "unexpected {forgery} validation error: {error}"
            );
        }

        let bundle_path = source_root.0.join("suggestion-change-person.json");
        source.export_identity_bundle(&bundle_path).unwrap();
        let target_root = TestRoot::new("suggestion-change-person-target");
        let relocated_media = target_root.0.join("relocated-media");
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let destination = relocated_media.join(media_key);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source_media.join(media_key), destination).unwrap();
        }
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&relocated_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let target = MatchStore::open(&target_root.0).unwrap();
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        target.apply_identity_bundle_import(plan).unwrap();
        let restored = target.build_identity_bundle().unwrap();
        assert!(restored.graph.assignments.iter().any(|assignment| {
            assignment.face_id == suggested_face.face_id
                && assignment.person_id == target_person.person_id
                && assignment.look_id.is_none()
                && assignment.placement == "unsorted"
        }));
        assert!(restored.graph.constraints.iter().any(|constraint| {
            constraint.face_id == suggested_face.face_id && constraint.person_id == "person-a"
        }));

        let source_person: Person = source
            .require(PERSON_TABLE, "person-a", "source Person")
            .unwrap();
        let revised_source_person = source
            .update_person(
                &source_person.person_id,
                source_person.revision,
                "Alice historical-source revised",
                vec!["Historical source".to_string()],
            )
            .unwrap();
        let revised_source_person = source
            .update_person_preferences(
                &revised_source_person.person_id,
                revised_source_person.revision,
                revised_source_person.cover_media_key.clone(),
                true,
                true,
            )
            .unwrap();
        let revised_bundle = source.build_identity_bundle().unwrap();
        let revised_provenance = revised_bundle
            .graph
            .suggestion_source_provenance
            .iter()
            .find(|row| row.operation_kind == "change_person")
            .unwrap();
        assert_eq!(revised_provenance.person_revision, 1);
        assert!(revised_source_person.revision > revised_provenance.person_revision);

        let remove_source = source
            .preview_remove_person(&revised_source_person.person_id)
            .unwrap();
        source.remove_person(&remove_source).unwrap();
        let removed_person_bundle = source.build_identity_bundle().unwrap();
        assert!(removed_person_bundle
            .graph
            .suggestion_source_provenance
            .iter()
            .any(|row| {
                row.operation_kind == "change_person"
                    && row.candidate_person_id == revised_source_person.person_id
                    && row.person_revision == 1
            }));

        let delete_face_fence = source
            .correction_fence_for(
                &suggested_face.face_id,
                vec![target_person.person_id.clone()],
            )
            .unwrap();
        let deleted = source
            .delete_face_analysis(&suggested_face.face_id, &delete_face_fence)
            .unwrap();
        let historical_bundle = source.build_identity_bundle().unwrap();
        assert!(historical_bundle
            .graph
            .people
            .iter()
            .all(|person| person.person_id != revised_source_person.person_id));
        assert!(historical_bundle
            .graph
            .faces
            .iter()
            .all(|face| face.face_id != suggested_face.face_id));
        assert!(historical_bundle.graph.operations.iter().any(|operation| {
            operation.kind == "correction_change_person"
                && operation.face_id.as_deref() == Some(suggested_face.face_id.as_str())
        }));

        let historical_path = source_root
            .0
            .join("suggestion-change-person-historical-only.json");
        source.export_identity_bundle(&historical_path).unwrap();
        let historical_bytes = fs::read(&historical_path).unwrap();
        let historical_text = std::str::from_utf8(&historical_bytes).unwrap();
        assert!(!historical_text.contains("\\\"vector\\\""));
        assert!(historical_text.contains("\\\"vector_omitted\\\":true"));
        let historical_target_root = TestRoot::new("suggestion-change-person-historical-target");
        let historical_target = MatchStore::open(&historical_target_root.0).unwrap();
        let historical_plan = historical_target
            .preview_identity_bundle_import(
                &historical_path,
                &relocations,
                IdentityImportMode::Replace,
            )
            .unwrap();
        historical_target
            .apply_identity_bundle_import(historical_plan)
            .unwrap();
        let historical_restored = historical_target.build_identity_bundle().unwrap();
        assert!(historical_restored
            .graph
            .operations
            .iter()
            .any(|operation| {
                operation.kind == "correction_change_person"
                    && operation.face_id.as_deref() == Some(suggested_face.face_id.as_str())
            }));
        historical_target
            .undo_correction(&deleted.operation_id)
            .unwrap();
        assert!(historical_target
            .get_one::<FaceObservation>(FACE_TABLE, &suggested_face.face_id)
            .unwrap()
            .is_some());
        assert!(historical_target
            .get_one::<FaceEmbedding>(
                EMBEDDING_TABLE,
                &embedding_id(&suggested_face.face_id, "model-v1"),
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn suggestion_different_history_survives_face_delete_persistent_undo_and_relocated_import() {
        let source_root = TestRoot::new("suggestion-different-undo-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);

        let suggested_face: FaceObservation = source
            .require(FACE_TABLE, "face-suggestion", "suggested Face")
            .unwrap();
        let execution = source.execution_state().unwrap();
        let asset = JobAsset {
            asset_id: "asset-suggestion-different-undo".to_string(),
            job_id: "job-history".to_string(),
            media_key: suggested_face.media_key.clone(),
            source_path: None,
            media_fingerprint: suggested_face.media_fingerprint.clone(),
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
            model_generation: "model-v1".to_string(),
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            updated_at: "2026-08-25T00:00:01Z".to_string(),
        };
        source
            .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
            .unwrap();

        let different_fence = source
            .correction_fence_for(&suggested_face.face_id, vec!["person-a".to_string()])
            .unwrap();
        let different = source
            .different_correction(&suggested_face.face_id, "person-a", &different_fence)
            .unwrap();
        let different_operation: MatchOperation = source
            .require(
                OPERATION_TABLE,
                &different.operation_id,
                "Different operation",
            )
            .unwrap();
        let durable_provenance: Vec<SuggestionSourceProvenance> =
            source.list(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap();
        assert_eq!(durable_provenance.len(), 1);
        assert_eq!(durable_provenance[0].operation_id, different.operation_id);
        source
            .transactional_upserts_deletes(
                &[],
                &[(
                    SUGGESTION_SOURCE_PROVENANCE_TABLE,
                    durable_provenance[0].provenance_id.as_str(),
                )],
            )
            .unwrap();
        assert_eq!(source.count(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap(), 0);
        assert_eq!(
            source
                .build_identity_bundle()
                .unwrap()
                .graph
                .suggestion_source_provenance,
            durable_provenance
        );
        let legacy_recovery_path = source_root.0.join("legacy-provenance-recovery.json");
        let legacy_clear_preview = source
            .preview_clear_all_match_data(&legacy_recovery_path)
            .unwrap();
        assert_eq!(source.count(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap(), 0);
        assert_eq!(
            legacy_clear_preview
                .recovery_bundle
                .table_counts
                .get(SUGGESTION_SOURCE_PROVENANCE_TABLE),
            Some(&1)
        );

        let strict_face: FaceObservation = source
            .require(FACE_TABLE, "face-strict", "strict Face")
            .unwrap();
        let strict_embedding = FaceEmbedding {
            embedding_id: embedding_id(&strict_face.face_id, "model-v1"),
            face_id: strict_face.face_id.clone(),
            vector: {
                let mut vector = vec![0.0; EMBEDDING_DIM];
                vector[0] = 1.0;
                vector
            },
            model_generation: "model-v1".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: strict_face.media_fingerprint.clone(),
            face_revision: strict_face.face_revision,
            job_id: "job-strict-vector-history".to_string(),
            active: true,
            created_at: strict_face.created_at.clone(),
        };
        source
            .upsert_json(
                EMBEDDING_TABLE,
                &strict_embedding.embedding_id,
                &strict_embedding,
            )
            .unwrap();
        let strict_fence = source.correction_fence(&strict_face.face_id).unwrap();
        let not_a_face = source
            .mark_not_a_face(&strict_face.face_id, &strict_fence)
            .unwrap();
        let raw_not_a_face: MatchOperation = source
            .require(
                OPERATION_TABLE,
                &not_a_face.operation_id,
                "raw not-a-face vector history",
            )
            .unwrap();
        assert!(raw_not_a_face.after_json.contains("\"vector\""));

        let empty_root = TestRoot::new("legacy-provenance-empty-import");
        let empty_store = MatchStore::open(&empty_root.0).unwrap();
        let empty_bundle_path = empty_root.0.join("empty.json");
        empty_store
            .export_identity_bundle(&empty_bundle_path)
            .unwrap();
        let interrupted_plan = source
            .preview_identity_bundle_import(
                &empty_bundle_path,
                &BTreeMap::new(),
                IdentityImportMode::Replace,
            )
            .unwrap();
        inject_leave_import_journal_for_plan(&interrupted_plan.plan_token);
        assert!(source
            .apply_identity_bundle_import(interrupted_plan)
            .unwrap_err()
            .contains("injected identity import interruption"));
        drop(source);
        crate::surreal_store::wait_until_closed(&MediaDb::db_path(&source_root.0)).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        assert_eq!(source.count(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap(), 0);
        let restored_not_a_face: MatchOperation = source
            .require(
                OPERATION_TABLE,
                &not_a_face.operation_id,
                "restart-restored not-a-face vector history",
            )
            .unwrap();
        assert_eq!(restored_not_a_face, raw_not_a_face);
        assert!(restored_not_a_face.after_json.contains("\"vector\""));
        assert_eq!(
            source
                .build_identity_bundle()
                .unwrap()
                .graph
                .suggestion_source_provenance,
            durable_provenance
        );
        let different_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&different_operation.after_json).unwrap();
        let historical_suggestion: Suggestion = serde_json::from_value(
            different_envelope
                .rows
                .iter()
                .find(|row| {
                    row.table == CorrectionTable::Suggestion
                        && row.before.is_some()
                        && row.after.is_none()
                })
                .unwrap()
                .before
                .clone()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            historical_suggestion.face_revision,
            suggested_face.face_revision
        );
        assert_eq!(
            historical_suggestion.media_fingerprint,
            suggested_face.media_fingerprint
        );

        let source_person: Person = source.require(PERSON_TABLE, "person-a", "Person").unwrap();
        let revised_person = source
            .update_person(
                &source_person.person_id,
                source_person.revision,
                "Alice Different history revised",
                vec!["Different history".to_string()],
            )
            .unwrap();
        let revised_person = source
            .update_person_preferences(
                &revised_person.person_id,
                revised_person.revision,
                revised_person.cover_media_key.clone(),
                true,
                true,
            )
            .unwrap();
        let revised_bundle = source.build_identity_bundle().unwrap();
        let revised_provenance = revised_bundle
            .graph
            .suggestion_source_provenance
            .iter()
            .find(|row| row.operation_id == different.operation_id)
            .unwrap();
        assert_eq!(revised_provenance.person_revision, 1);
        assert!(revised_person.revision > revised_provenance.person_revision);

        let delete_fence = source.correction_fence(&suggested_face.face_id).unwrap();
        let deleted = source
            .delete_face_analysis(&suggested_face.face_id, &delete_fence)
            .unwrap();
        let mut equal_time_delete: MatchOperation = source
            .require(
                OPERATION_TABLE,
                &deleted.operation_id,
                "equal-time delete operation",
            )
            .unwrap();
        equal_time_delete.created_at = different_operation.created_at.clone();
        source
            .upsert_json(
                OPERATION_TABLE,
                &equal_time_delete.operation_id,
                &equal_time_delete,
            )
            .unwrap();
        let mut delete_mapping: PortableCorrectionMediaOperation = source
            .list::<PortableCorrectionMediaOperation>(CORRECTION_MEDIA_OPERATION_TABLE)
            .unwrap()
            .into_iter()
            .find(|row| row.operation_id == deleted.operation_id)
            .unwrap();
        delete_mapping.created_at = equal_time_delete.created_at.clone();
        source
            .upsert_json(
                CORRECTION_MEDIA_OPERATION_TABLE,
                &delete_mapping.mapping_id,
                &delete_mapping,
            )
            .unwrap();
        drop(source);
        crate::surreal_store::wait_until_closed(&MediaDb::db_path(&source_root.0)).unwrap();

        let source = MatchStore::open(&source_root.0).unwrap();
        source.undo_correction(&deleted.operation_id).unwrap();
        let restored_face: FaceObservation = source
            .require(FACE_TABLE, &suggested_face.face_id, "restored Face")
            .unwrap();
        assert_eq!(restored_face.face_id, suggested_face.face_id);
        assert_eq!(
            restored_face.face_revision,
            suggested_face.face_revision + 1
        );
        assert_eq!(
            restored_face.media_fingerprint,
            suggested_face.media_fingerprint
        );

        let bundle = source.build_identity_bundle().unwrap();
        assert_eq!(
            bundle.graph.suggestion_source_provenance,
            durable_provenance
        );
        let bundled_different = bundle
            .graph
            .operations
            .iter()
            .find(|operation| operation.operation_id == different.operation_id)
            .unwrap();
        let bundled_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&bundled_different.after_json).unwrap();
        let bundled_historical: Suggestion = serde_json::from_value(
            bundled_envelope
                .rows
                .iter()
                .find(|row| {
                    row.table == CorrectionTable::Suggestion
                        && row.before.is_some()
                        && row.after.is_none()
                })
                .unwrap()
                .before
                .clone()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            bundled_historical.face_revision,
            historical_suggestion.face_revision
        );
        assert_ne!(
            bundled_historical.face_revision,
            restored_face.face_revision
        );
        assert_eq!(
            bundled_historical.media_fingerprint,
            historical_suggestion.media_fingerprint
        );

        let bundle_path = source_root.0.join("suggestion-different-undo.json");
        source.export_identity_bundle(&bundle_path).unwrap();
        let target_root = TestRoot::new("suggestion-different-undo-target");
        let relocated_media = target_root.0.join("relocated-media");
        for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
            let destination = relocated_media.join(media_key);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source_media.join(media_key), destination).unwrap();
        }
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&relocated_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let target = MatchStore::open(&target_root.0).unwrap();
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        target.apply_identity_bundle_import(plan).unwrap();
        let imported = target.build_identity_bundle().unwrap();
        assert_eq!(
            imported.graph.suggestion_source_provenance,
            bundle.graph.suggestion_source_provenance
        );
        let imported_different = imported
            .graph
            .operations
            .iter()
            .find(|operation| operation.operation_id == different.operation_id)
            .unwrap();
        let imported_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&imported_different.after_json).unwrap();
        let imported_historical: Suggestion = serde_json::from_value(
            imported_envelope
                .rows
                .iter()
                .find(|row| {
                    row.table == CorrectionTable::Suggestion
                        && row.before.is_some()
                        && row.after.is_none()
                })
                .unwrap()
                .before
                .clone()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(imported_historical, historical_suggestion);
        let recovery_path = target_root
            .0
            .join("suggestion-provenance-clear-recovery.json");
        let clear_preview = target.preview_clear_all_match_data(&recovery_path).unwrap();
        assert_eq!(
            clear_preview
                .recovery_bundle
                .table_counts
                .get(SUGGESTION_SOURCE_PROVENANCE_TABLE),
            Some(&1)
        );
    }

    #[test]
    fn person_revision_updates_reconcile_derived_evidence_and_round_trip_relocated_import() {
        for update_kind in ["profile", "preferences"] {
            let source_root = TestRoot::new(&format!("person-revision-{update_kind}-source"));
            let source_media = source_root.0.join("media-root");
            fs::create_dir_all(&source_media).unwrap();
            let source = MatchStore::open(&source_root.0).unwrap();
            seed_portable_graph(&source, &source_media);
            let mut before: Person = source.require(PERSON_TABLE, "person-a", "Person").unwrap();
            if update_kind == "profile" {
                before.cover_media_key = Some("media/c.jpg".to_string());
                source
                    .upsert_json(PERSON_TABLE, &before.person_id, &before)
                    .unwrap();
            } else {
                assert!(source
                    .update_person_preferences(
                        &before.person_id,
                        before.revision,
                        Some("media/c.jpg".to_string()),
                        true,
                        false,
                    )
                    .unwrap_err()
                    .contains("Person gallery"));
            }
            let execution_before = source.execution_state().unwrap();
            let updated = if update_kind == "profile" {
                source
                    .update_person(
                        &before.person_id,
                        before.revision,
                        "Alice revised",
                        vec!["Reconciled alias".to_string()],
                    )
                    .unwrap()
            } else {
                source
                    .update_person_preferences(
                        &before.person_id,
                        before.revision,
                        before.cover_media_key.clone(),
                        true,
                        false,
                    )
                    .unwrap()
            };
            let execution_after = source.execution_state().unwrap();
            assert_eq!(
                execution_after.catalog_revision,
                execution_before.catalog_revision + 1
            );
            assert_eq!(
                execution_after.identity_revision,
                execution_before.identity_revision
            );
            assert_eq!(updated.catalog_revision, execution_after.catalog_revision);
            if update_kind == "profile" {
                assert_eq!(updated.cover_media_key, None);
            } else {
                assert_eq!(updated.cover_media_key.as_deref(), Some("media/a.jpg"));
            }
            let operator_assignment: Assignment = source
                .require(ASSIGNMENT_TABLE, "face-a", "operator Assignment")
                .unwrap();
            assert_eq!(operator_assignment.person_revision, updated.revision);
            assert_eq!(operator_assignment.state, "operator_confirmed");
            assert!(source
                .get_one::<Assignment>(ASSIGNMENT_TABLE, "face-strict")
                .unwrap()
                .is_none());
            assert!(source
                .list::<Suggestion>(SUGGESTION_TABLE)
                .unwrap()
                .iter()
                .all(|suggestion| suggestion.candidate_person_id != updated.person_id));

            let bundle_path = source_root
                .0
                .join(format!("person-revision-{update_kind}.json"));
            source.export_identity_bundle(&bundle_path).unwrap();

            let target_root = TestRoot::new(&format!("person-revision-{update_kind}-target"));
            let relocated_media = target_root.0.join("relocated-media");
            for media_key in ["media/a.jpg", "media/b.jpg", "media/c.jpg"] {
                let destination = relocated_media.join(media_key);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::copy(source_media.join(media_key), destination).unwrap();
            }
            let relocations = BTreeMap::from([(
                "root-a".to_string(),
                fs::canonicalize(&relocated_media)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            )]);
            let target = MatchStore::open(&target_root.0).unwrap();
            let plan = target
                .preview_identity_bundle_import(
                    &bundle_path,
                    &relocations,
                    IdentityImportMode::Replace,
                )
                .unwrap();
            target.apply_identity_bundle_import(plan).unwrap();
            let restored = target.build_identity_bundle().unwrap();
            let restored_person = restored
                .graph
                .people
                .iter()
                .find(|person| person.person_id == updated.person_id)
                .unwrap();
            assert_eq!(restored_person.revision, updated.revision);
            let restored_assignment = restored
                .graph
                .assignments
                .iter()
                .find(|assignment| assignment.face_id == "face-a")
                .unwrap();
            assert_eq!(restored_assignment.person_revision, updated.revision);
            assert!(restored
                .graph
                .assignments
                .iter()
                .all(|assignment| assignment.face_id != "face-strict"));
            assert!(restored
                .graph
                .suggestions
                .iter()
                .all(|suggestion| suggestion.candidate_person_id != updated.person_id));
        }
    }

    #[test]
    fn portable_graph_rejects_forged_typed_operation_semantics() {
        let root = TestRoot::new("operation-integrity");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let graph = store.build_identity_bundle().unwrap().graph;
        validate_identity_graph(&graph).unwrap();
        let media_fingerprint = |media_key: &str| {
            graph
                .faces
                .iter()
                .find(|face| face.media_key == media_key)
                .map(|face| face.media_fingerprint.clone())
                .expect("portable test media has Face evidence")
        };

        let mut unknown = graph.operations[0].clone();
        unknown.kind = "future_untyped_operation".to_string();
        assert!(validate_match_operation(&unknown, &graph)
            .unwrap_err()
            .contains("unknown Match operation kind"));

        let mut malformed_time = graph.clone();
        malformed_time.operations[0].created_at = "not-rfc3339".to_string();
        assert!(validate_identity_graph(&malformed_time)
            .unwrap_err()
            .contains("invalid RFC3339 creation time"));

        let mut future_operation = graph.clone();
        future_operation.operations[0].created_at = "2100-01-01T00:00:00+14:00".to_string();
        assert!(validate_identity_graph(&future_operation)
            .unwrap_err()
            .contains("clock-skew allowance"));

        let mut future_suggestion = graph.clone();
        future_suggestion.suggestions[0].created_at = "2100-01-01T00:00:00-12:00".to_string();
        assert!(validate_identity_graph(&future_suggestion)
            .unwrap_err()
            .contains("clock-skew allowance"));

        let mut future_strict = graph
            .operations
            .iter()
            .find(|operation| operation.operation_id == "operation-strict")
            .unwrap()
            .clone();
        let mut future_assignment: Assignment =
            serde_json::from_str(&future_strict.after_json).unwrap();
        future_assignment.created_at = "2100-01-01T00:00:00Z".to_string();
        future_assignment.updated_at = future_assignment.created_at.clone();
        future_strict.created_at = future_assignment.created_at.clone();
        future_strict.after_json = serde_json::to_string(&future_assignment).unwrap();
        assert!(validate_match_operation(&future_strict, &graph)
            .unwrap_err()
            .contains("clock-skew allowance"));

        let mut missing_direct_mapping = graph.clone();
        missing_direct_mapping
            .correction_media_operations
            .retain(|mapping| mapping.operation_id != "operation-history");
        assert!(validate_identity_graph(&missing_direct_mapping)
            .unwrap_err()
            .contains(
                "direct media mapping must contain its one canonical media key and fingerprint"
            ));

        let mut forged_direct_fingerprint = graph.clone();
        forged_direct_fingerprint
            .correction_media_operations
            .iter_mut()
            .find(|mapping| mapping.operation_id == "operation-a")
            .unwrap()
            .media_fingerprint = "f".repeat(64);
        assert!(validate_identity_graph(&forged_direct_fingerprint)
            .unwrap_err()
            .contains("contradicts canonical Face fingerprint evidence"));

        let mut not_sure = graph
            .operations
            .iter()
            .find(|operation| operation.operation_id == "operation-history")
            .unwrap()
            .clone();
        not_sure.reversible = true;
        assert!(validate_match_operation(&not_sure, &graph)
            .unwrap_err()
            .contains("typed no-change receipt"));

        let assigned = graph
            .assignments
            .iter()
            .find(|assignment| assignment.face_id == "face-a")
            .unwrap()
            .clone();
        let mut wrong_person = graph
            .operations
            .iter()
            .find(|operation| operation.operation_id == "operation-a")
            .unwrap()
            .clone();
        wrong_person.person_id = Some("another-person".to_string());
        assert!(validate_match_operation(&wrong_person, &graph)
            .unwrap_err()
            .contains("assignment payload contradicts its typed kind"));

        let mut moved = assigned.clone();
        moved.look_id = Some("look-history-target".to_string());
        moved.placement = "look".to_string();
        moved.operation_id = "operation-history-move".to_string();
        let historical_move = MatchOperation {
            operation_id: moved.operation_id.clone(),
            kind: "move_to_look".to_string(),
            face_id: Some(moved.face_id.clone()),
            person_id: Some(moved.person_id.clone()),
            before_json: serde_json::to_string(&assigned).unwrap(),
            after_json: serde_json::to_string(&moved).unwrap(),
            reversible: true,
            created_at: moved.updated_at.clone(),
        };
        validate_match_operation(&historical_move, &graph).unwrap();
        let mut forged_move = historical_move;
        let mut forged_moved = moved;
        forged_moved.provenance = "forged-history-provenance".to_string();
        forged_move.after_json = serde_json::to_string(&forged_moved).unwrap();
        assert!(validate_match_operation(&forged_move, &graph)
            .unwrap_err()
            .contains("move-to-Look payload is inconsistent"));

        let mut strict_after = graph
            .assignments
            .iter()
            .find(|assignment| assignment.face_id == "face-strict")
            .unwrap()
            .clone();
        strict_after.operation_id = "operation-forged-same".to_string();
        let same_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: strict_after.assignment_id.clone(),
            before: Some(
                serde_json::to_value(
                    graph
                        .assignments
                        .iter()
                        .find(|assignment| assignment.face_id == "face-strict")
                        .unwrap(),
                )
                .unwrap(),
            ),
            after: Some(serde_json::to_value(&strict_after).unwrap()),
        }];
        let same_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "same".to_string(),
            rows: same_rows.clone(),
            face_ids: vec![strict_after.face_id.clone()],
            media_keys: vec![strict_after.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let same_before = same_rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let forged_same = MatchOperation {
            operation_id: "operation-forged-same".to_string(),
            kind: "correction_same".to_string(),
            face_id: Some(strict_after.face_id.clone()),
            person_id: Some(strict_after.person_id.clone()),
            before_json: serde_json::to_string(&same_before).unwrap(),
            after_json: serde_json::to_string(&same_envelope).unwrap(),
            reversible: true,
            created_at: strict_after.updated_at.clone(),
        };
        assert!(validate_match_operation(&forged_same, &graph)
            .unwrap_err()
            .contains("does not create confirmed evidence"));

        let mut bound_same_assignment = strict_after.clone();
        bound_same_assignment.state = "operator_confirmed".to_string();
        bound_same_assignment.locked = true;
        bound_same_assignment.provenance = "same".to_string();
        bound_same_assignment.model_generation = None;
        bound_same_assignment.calibration_generation = None;
        bound_same_assignment.envelope_hash = None;
        let bound_same_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: bound_same_assignment.assignment_id.clone(),
            before: Some(
                serde_json::to_value(
                    graph
                        .assignments
                        .iter()
                        .find(|assignment| assignment.face_id == "face-strict")
                        .unwrap(),
                )
                .unwrap(),
            ),
            after: Some(serde_json::to_value(&bound_same_assignment).unwrap()),
        }];
        let bound_same_before = bound_same_rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let bound_same_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "same".to_string(),
            rows: bound_same_rows,
            face_ids: vec![bound_same_assignment.face_id.clone()],
            media_keys: vec![bound_same_assignment.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let valid_bound_same = MatchOperation {
            operation_id: bound_same_assignment.operation_id.clone(),
            kind: "correction_same".to_string(),
            face_id: Some(bound_same_assignment.face_id.clone()),
            person_id: Some(bound_same_assignment.person_id.clone()),
            before_json: serde_json::to_string(&bound_same_before).unwrap(),
            after_json: serde_json::to_string(&bound_same_envelope).unwrap(),
            reversible: true,
            created_at: bound_same_assignment.updated_at.clone(),
        };
        validate_match_operation(&valid_bound_same, &graph).unwrap();
        let mut forged_same_look = valid_bound_same.clone();
        let mut forged_same_look_envelope = bound_same_envelope.clone();
        forged_same_look_envelope.rows[0].after.as_mut().unwrap()["look_id"] =
            Value::String("look-forged-same".to_string());
        forged_same_look_envelope.rows[0].after.as_mut().unwrap()["placement"] =
            Value::String("look".to_string());
        forged_same_look.after_json = serde_json::to_string(&forged_same_look_envelope).unwrap();
        assert!(validate_match_operation(&forged_same_look, &graph)
            .unwrap_err()
            .contains("does not create confirmed evidence"));

        let mut candidate_less_same_envelope = bound_same_envelope.clone();
        candidate_less_same_envelope.rows[0].before = None;
        let mut candidate_less_same = valid_bound_same.clone();
        candidate_less_same.before_json = serde_json::to_string(
            &candidate_less_same_envelope
                .rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        candidate_less_same.after_json =
            serde_json::to_string(&candidate_less_same_envelope).unwrap();
        assert!(validate_match_operation(&candidate_less_same, &graph)
            .unwrap_err()
            .contains("suggestion source evidence"));

        let same_suggestion_source = graph.suggestions.first().unwrap().clone();
        let same_suggestion_face = graph
            .faces
            .iter()
            .find(|face| face.face_id == same_suggestion_source.face_id)
            .unwrap();
        let mut suggestion_same_assignment = bound_same_assignment.clone();
        suggestion_same_assignment.assignment_id = same_suggestion_source.face_id.clone();
        suggestion_same_assignment.face_id = same_suggestion_source.face_id.clone();
        suggestion_same_assignment.person_id = same_suggestion_source.candidate_person_id.clone();
        suggestion_same_assignment.media_key = same_suggestion_face.media_key.clone();
        suggestion_same_assignment.face_revision = same_suggestion_source.face_revision;
        suggestion_same_assignment.person_revision = same_suggestion_source.person_revision;
        suggestion_same_assignment.operation_id = "operation-suggestion-same".to_string();
        suggestion_same_assignment.created_at = same_suggestion_source.created_at.clone();
        suggestion_same_assignment.updated_at = same_suggestion_source.created_at.clone();
        let suggestion_same_rows = vec![
            CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: suggestion_same_assignment.assignment_id.clone(),
                before: None,
                after: Some(serde_json::to_value(&suggestion_same_assignment).unwrap()),
            },
            CorrectionRowDelta {
                table: CorrectionTable::Suggestion,
                stable_id: same_suggestion_source.suggestion_id.clone(),
                before: Some(serde_json::to_value(&same_suggestion_source).unwrap()),
                after: None,
            },
        ];
        let suggestion_same_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "same".to_string(),
            rows: suggestion_same_rows.clone(),
            face_ids: vec![suggestion_same_assignment.face_id.clone()],
            media_keys: vec![suggestion_same_assignment.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let suggestion_same_operation = MatchOperation {
            operation_id: suggestion_same_assignment.operation_id.clone(),
            kind: "correction_same".to_string(),
            face_id: Some(suggestion_same_assignment.face_id.clone()),
            person_id: Some(suggestion_same_assignment.person_id.clone()),
            before_json: serde_json::to_string(
                &suggestion_same_rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            after_json: serde_json::to_string(&suggestion_same_envelope).unwrap(),
            reversible: true,
            created_at: suggestion_same_assignment.updated_at.clone(),
        };
        validate_match_operation(&suggestion_same_operation, &graph).unwrap();

        let suggestion_same_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &suggestion_same_operation.operation_id,
                &suggestion_same_assignment.media_key,
            ),
            media_key: suggestion_same_assignment.media_key.clone(),
            media_fingerprint: media_fingerprint(&suggestion_same_assignment.media_key),
            operation_id: suggestion_same_operation.operation_id.clone(),
            kind: "same".to_string(),
            created_at: suggestion_same_operation.created_at.clone(),
        };
        let mut suggestion_same_graph = graph.clone();
        suggestion_same_graph
            .operations
            .push(suggestion_same_operation.clone());
        suggestion_same_graph
            .correction_media_operations
            .push(suggestion_same_mapping);
        suggestion_same_graph
            .suggestions
            .retain(|suggestion| suggestion.suggestion_id != same_suggestion_source.suggestion_id);
        suggestion_same_graph.assignments.retain(|assignment| {
            assignment.assignment_id != suggestion_same_assignment.assignment_id
        });
        suggestion_same_graph
            .assignments
            .push(suggestion_same_assignment.clone());
        let mut same_provenance = SuggestionSourceProvenance {
            provenance_id: String::new(),
            operation_id: suggestion_same_operation.operation_id.clone(),
            operation_kind: "same".to_string(),
            suggestion_id: same_suggestion_source.suggestion_id.clone(),
            face_id: same_suggestion_source.face_id.clone(),
            candidate_person_id: same_suggestion_source.candidate_person_id.clone(),
            media_key: same_suggestion_face.media_key.clone(),
            media_fingerprint: same_suggestion_source.media_fingerprint.clone(),
            face_revision: same_suggestion_source.face_revision,
            person_revision: same_suggestion_source.person_revision,
            model_generation: same_suggestion_source.model_generation.clone(),
            job_id: same_suggestion_source.job_id.clone(),
            suggestion_created_at: same_suggestion_source.created_at.clone(),
            similarity_bits: same_suggestion_source.similarity.to_bits(),
            calibration_generation: same_suggestion_source.calibration_generation.clone(),
            envelope_hash: same_suggestion_source.envelope_hash.clone(),
            embedding_id: embedding_id(
                &same_suggestion_source.face_id,
                &same_suggestion_source.model_generation,
            ),
            embedding_created_at: same_suggestion_source.created_at.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            operation_created_at: suggestion_same_operation.created_at.clone(),
        };
        same_provenance.provenance_id = suggestion_source_provenance_id(&same_provenance);
        suggestion_same_graph
            .suggestion_source_provenance
            .push(same_provenance);
        validate_identity_graph(&suggestion_same_graph).unwrap();

        let mut future_provenance = suggestion_same_graph.clone();
        future_provenance.suggestion_source_provenance[0].embedding_created_at =
            "2100-01-01T00:00:00+00:00".to_string();
        assert!(validate_identity_graph(&future_provenance)
            .unwrap_err()
            .contains("clock-skew allowance"));

        for source_forgery in [
            "fingerprint",
            "missing_generation",
            "empty_job",
            "incomplete_calibration",
        ] {
            let mut forged_graph = suggestion_same_graph.clone();
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|operation| operation.operation_id == suggestion_same_operation.operation_id)
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let suggestion_row = envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Suggestion)
                .unwrap();
            let mut source: Suggestion =
                serde_json::from_value(suggestion_row.before.clone().unwrap()).unwrap();
            match source_forgery {
                "fingerprint" => {
                    source.media_fingerprint = "fingerprint-forged-same-history".to_string()
                }
                "missing_generation" => {
                    source.model_generation = "model-missing-same-history".to_string()
                }
                "empty_job" => source.job_id.clear(),
                "incomplete_calibration" => {
                    source.calibration_generation = Some("calibration-forged".to_string());
                    source.envelope_hash = None;
                }
                _ => unreachable!(),
            }
            suggestion_row.before = Some(serde_json::to_value(source).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            assert!(
                validate_identity_graph(&forged_graph).is_err(),
                "forged Same {source_forgery} source provenance was accepted"
            );
        }

        for source_forgery in ["wrong_person", "operator_confirmed"] {
            let mut forged_source_envelope = bound_same_envelope.clone();
            let mut forged_source: Assignment =
                serde_json::from_value(forged_source_envelope.rows[0].before.clone().unwrap())
                    .unwrap();
            if source_forgery == "wrong_person" {
                forged_source.person_id = "person-forged-same-source".to_string();
            } else {
                forged_source.state = "operator_confirmed".to_string();
                forged_source.locked = true;
                forged_source.provenance = "operator".to_string();
                forged_source.model_generation = None;
                forged_source.calibration_generation = None;
                forged_source.envelope_hash = None;
            }
            forged_source_envelope.rows[0].before =
                Some(serde_json::to_value(forged_source).unwrap());
            let mut forged_source_operation = valid_bound_same.clone();
            forged_source_operation.before_json = serde_json::to_string(
                &forged_source_envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            forged_source_operation.after_json =
                serde_json::to_string(&forged_source_envelope).unwrap();
            assert!(validate_match_operation(&forged_source_operation, &graph)
                .unwrap_err()
                .contains("strict source evidence"));
        }

        let bound_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &valid_bound_same.operation_id,
                &bound_same_assignment.media_key,
            ),
            media_key: bound_same_assignment.media_key.clone(),
            media_fingerprint: media_fingerprint(&bound_same_assignment.media_key),
            operation_id: valid_bound_same.operation_id.clone(),
            kind: "same".to_string(),
            created_at: valid_bound_same.created_at.clone(),
        };
        let mut unapplied_same = graph.clone();
        unapplied_same.operations.push(valid_bound_same.clone());
        unapplied_same
            .correction_media_operations
            .push(bound_mapping.clone());
        let error = validate_identity_graph(&unapplied_same).unwrap_err();
        assert!(
            error.contains("durable effect"),
            "unexpected unapplied Same lineage error: {error}"
        );

        let mut exactly_mapped_same = graph.clone();
        exactly_mapped_same
            .operations
            .push(valid_bound_same.clone());
        exactly_mapped_same
            .correction_media_operations
            .push(bound_mapping.clone());
        exactly_mapped_same
            .assignments
            .retain(|assignment| assignment.assignment_id != bound_same_assignment.assignment_id);
        exactly_mapped_same
            .assignments
            .push(bound_same_assignment.clone());
        validate_identity_graph(&exactly_mapped_same).unwrap();

        for strict_forgery in ["missing_generation", "incomplete_calibration"] {
            let mut forged_graph = exactly_mapped_same.clone();
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|operation| operation.operation_id == valid_bound_same.operation_id)
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let assignment_row = envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Assignment)
                .unwrap();
            let mut source: Assignment =
                serde_json::from_value(assignment_row.before.clone().unwrap()).unwrap();
            if strict_forgery == "missing_generation" {
                source.model_generation = Some("model-missing-same-strict-history".to_string());
            } else {
                source.envelope_hash = None;
            }
            assignment_row.before = Some(serde_json::to_value(source).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            assert!(
                validate_identity_graph(&forged_graph).is_err(),
                "forged Same strict {strict_forgery} provenance was accepted"
            );
        }

        let mut missing_mapping = exactly_mapped_same.clone();
        missing_mapping
            .correction_media_operations
            .retain(|mapping| mapping.operation_id != valid_bound_same.operation_id);
        assert!(validate_identity_graph(&missing_mapping)
            .unwrap_err()
            .contains("correction media mapping set contradicts its envelope"));

        let mut forged_historical_media = exactly_mapped_same.clone();
        let operation = forged_historical_media
            .operations
            .iter_mut()
            .find(|operation| operation.operation_id == valid_bound_same.operation_id)
            .unwrap();
        let mut envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        envelope.media_keys.push(assigned.media_key.clone());
        envelope.media_keys.sort();
        operation.after_json = serde_json::to_string(&envelope).unwrap();
        forged_historical_media.correction_media_operations.push(
            PortableCorrectionMediaOperation {
                mapping_id: exchange_correction_media_mapping_id(
                    &valid_bound_same.operation_id,
                    &assigned.media_key,
                ),
                media_key: assigned.media_key.clone(),
                media_fingerprint: media_fingerprint(&assigned.media_key),
                operation_id: valid_bound_same.operation_id.clone(),
                kind: "same".to_string(),
                created_at: valid_bound_same.created_at.clone(),
            },
        );
        assert!(validate_identity_graph(&forged_historical_media)
            .unwrap_err()
            .contains("canonical current or historical evidence"));

        let mut extra_mapping = exactly_mapped_same;
        let unrelated_media_key = assigned.media_key.clone();
        extra_mapping
            .correction_media_operations
            .push(PortableCorrectionMediaOperation {
                mapping_id: exchange_correction_media_mapping_id(
                    &valid_bound_same.operation_id,
                    &unrelated_media_key,
                ),
                media_key: unrelated_media_key,
                media_fingerprint: media_fingerprint(&assigned.media_key),
                operation_id: valid_bound_same.operation_id.clone(),
                kind: "same".to_string(),
                created_at: valid_bound_same.created_at.clone(),
            });
        assert!(validate_identity_graph(&extra_mapping)
            .unwrap_err()
            .contains("correction media mapping set contradicts its envelope"));

        let wrong_mapping_kind_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "not_sure".to_string(),
            rows: Vec::new(),
            face_ids: vec![assigned.face_id.clone()],
            media_keys: vec![assigned.media_key.clone()],
            identity_changed: false,
            catalog_changed: false,
        };
        let wrong_mapping_kind_correction = MatchOperation {
            operation_id: "operation-non-reversible-mapping".to_string(),
            kind: "correction_not_sure".to_string(),
            face_id: Some(assigned.face_id.clone()),
            person_id: Some(assigned.person_id.clone()),
            before_json: "[]".to_string(),
            after_json: serde_json::to_string(&wrong_mapping_kind_envelope).unwrap(),
            reversible: false,
            created_at: assigned.updated_at.clone(),
        };
        let mut wrong_mapping_kind = graph.clone();
        wrong_mapping_kind
            .operations
            .push(wrong_mapping_kind_correction.clone());
        wrong_mapping_kind
            .correction_media_operations
            .push(PortableCorrectionMediaOperation {
                mapping_id: exchange_correction_media_mapping_id(
                    &wrong_mapping_kind_correction.operation_id,
                    &assigned.media_key,
                ),
                media_key: assigned.media_key.clone(),
                media_fingerprint: media_fingerprint(&assigned.media_key),
                operation_id: wrong_mapping_kind_correction.operation_id.clone(),
                kind: "same".to_string(),
                created_at: wrong_mapping_kind_correction.created_at.clone(),
            });
        assert!(validate_identity_graph(&wrong_mapping_kind)
            .unwrap_err()
            .contains("not the canonical operation/media effect mapping"));

        let mut forged_same_identity = valid_bound_same.clone();
        forged_same_identity.face_id = Some(assigned.face_id.clone());
        forged_same_identity.person_id = Some("forged-person".to_string());
        let mut forged_same_envelope = bound_same_envelope.clone();
        forged_same_envelope.face_ids = vec![assigned.face_id.clone()];
        forged_same_identity.after_json = serde_json::to_string(&forged_same_envelope).unwrap();
        assert!(validate_match_operation(&forged_same_identity, &graph)
            .unwrap_err()
            .contains("identity envelope contradicts its Assignment effects"));

        let mut unsupported_same_envelope = bound_same_envelope.clone();
        let unrelated_person = graph.people.first().unwrap();
        unsupported_same_envelope.rows.push(CorrectionRowDelta {
            table: CorrectionTable::Person,
            stable_id: unrelated_person.person_id.clone(),
            before: Some(serde_json::to_value(unrelated_person).unwrap()),
            after: None,
        });
        let unsupported_same_before = unsupported_same_envelope
            .rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let mut unsupported_same = valid_bound_same.clone();
        unsupported_same.before_json = serde_json::to_string(&unsupported_same_before).unwrap();
        unsupported_same.after_json = serde_json::to_string(&unsupported_same_envelope).unwrap();
        assert!(validate_match_operation(&unsupported_same, &graph)
            .unwrap_err()
            .contains("unsupported typed row effect"));

        let unrelated_suggestion = graph
            .suggestions
            .iter()
            .find(|suggestion| suggestion.face_id != bound_same_assignment.face_id)
            .unwrap();
        let mut unrelated_same_envelope = bound_same_envelope.clone();
        unrelated_same_envelope.rows.push(CorrectionRowDelta {
            table: CorrectionTable::Suggestion,
            stable_id: unrelated_suggestion.suggestion_id.clone(),
            before: Some(serde_json::to_value(unrelated_suggestion).unwrap()),
            after: None,
        });
        let unrelated_same_before = unrelated_same_envelope
            .rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let mut unrelated_same = valid_bound_same.clone();
        unrelated_same.before_json = serde_json::to_string(&unrelated_same_before).unwrap();
        unrelated_same.after_json = serde_json::to_string(&unrelated_same_envelope).unwrap();
        assert!(validate_match_operation(&unrelated_same, &graph)
            .unwrap_err()
            .contains("auxiliary deletion targets another FaceId"));

        let mut batch_same_assignment = bound_same_assignment.clone();
        batch_same_assignment.provenance = "batch_same".to_string();
        let batch_same_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: batch_same_assignment.assignment_id.clone(),
            before: Some(serde_json::to_value(&strict_after).unwrap()),
            after: Some(serde_json::to_value(&batch_same_assignment).unwrap()),
        }];
        let batch_same_before = batch_same_rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let batch_same_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "batch_same".to_string(),
            rows: batch_same_rows,
            face_ids: vec![batch_same_assignment.face_id.clone()],
            media_keys: vec![batch_same_assignment.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let valid_batch_same = MatchOperation {
            operation_id: batch_same_assignment.operation_id.clone(),
            kind: "correction_batch_same".to_string(),
            face_id: None,
            person_id: Some(batch_same_assignment.person_id.clone()),
            before_json: serde_json::to_string(&batch_same_before).unwrap(),
            after_json: serde_json::to_string(&batch_same_envelope).unwrap(),
            reversible: true,
            created_at: batch_same_assignment.updated_at.clone(),
        };
        validate_match_operation(&valid_batch_same, &graph).unwrap();
        let mut forged_batch_same_look = valid_batch_same.clone();
        let mut forged_batch_same_look_envelope = batch_same_envelope.clone();
        forged_batch_same_look_envelope.rows[0]
            .after
            .as_mut()
            .unwrap()["look_id"] = Value::String("look-forged-batch-same".to_string());
        forged_batch_same_look_envelope.rows[0]
            .after
            .as_mut()
            .unwrap()["placement"] = Value::String("look".to_string());
        forged_batch_same_look.after_json =
            serde_json::to_string(&forged_batch_same_look_envelope).unwrap();
        assert!(validate_match_operation(&forged_batch_same_look, &graph)
            .unwrap_err()
            .contains("does not create confirmed evidence"));

        let mut forged_batch_same = valid_batch_same;
        let mut batch_same_envelope = batch_same_envelope;
        batch_same_envelope.face_ids = vec![assigned.face_id.clone()];
        forged_batch_same.after_json = serde_json::to_string(&batch_same_envelope).unwrap();
        assert!(validate_match_operation(&forged_batch_same, &graph)
            .unwrap_err()
            .contains("identity envelope contradicts its Assignment effects"));

        let different_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: assigned.assignment_id.clone(),
            before: Some(serde_json::to_value(&assigned).unwrap()),
            after: None,
        }];
        let different_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "different".to_string(),
            rows: different_rows.clone(),
            face_ids: vec![assigned.face_id.clone()],
            media_keys: vec![assigned.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let different_before = different_rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let forged_different = MatchOperation {
            operation_id: "operation-forged-different".to_string(),
            kind: "correction_different".to_string(),
            face_id: Some(assigned.face_id.clone()),
            person_id: Some(assigned.person_id.clone()),
            before_json: serde_json::to_string(&different_before).unwrap(),
            after_json: serde_json::to_string(&different_envelope).unwrap(),
            reversible: true,
            created_at: assigned.updated_at.clone(),
        };
        assert!(validate_match_operation(&forged_different, &graph)
            .unwrap_err()
            .contains("has no cannot-link effect"));

        let bound_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(&strict_after.face_id, &strict_after.person_id),
            face_id: strict_after.face_id.clone(),
            person_id: strict_after.person_id.clone(),
            operation_id: "operation-bound-different".to_string(),
            operator_owned: true,
            created_at: strict_after.updated_at.clone(),
        };
        let bound_different_rows = vec![
            CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: strict_after.assignment_id.clone(),
                before: Some(serde_json::to_value(&strict_after).unwrap()),
                after: None,
            },
            CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: bound_constraint.constraint_id.clone(),
                before: None,
                after: Some(serde_json::to_value(&bound_constraint).unwrap()),
            },
        ];
        let bound_different_before = bound_different_rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let bound_different_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "different".to_string(),
            rows: bound_different_rows,
            face_ids: vec![strict_after.face_id.clone()],
            media_keys: vec![strict_after.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let valid_bound_different = MatchOperation {
            operation_id: bound_constraint.operation_id.clone(),
            kind: "correction_different".to_string(),
            face_id: Some(bound_constraint.face_id.clone()),
            person_id: Some(bound_constraint.person_id.clone()),
            before_json: serde_json::to_string(&bound_different_before).unwrap(),
            after_json: serde_json::to_string(&bound_different_envelope).unwrap(),
            reversible: true,
            created_at: bound_constraint.created_at.clone(),
        };
        validate_match_operation(&valid_bound_different, &graph).unwrap();

        let bound_different_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &valid_bound_different.operation_id,
                &strict_after.media_key,
            ),
            media_key: strict_after.media_key.clone(),
            media_fingerprint: media_fingerprint(&strict_after.media_key),
            operation_id: valid_bound_different.operation_id.clone(),
            kind: "different".to_string(),
            created_at: valid_bound_different.created_at.clone(),
        };
        let unapplied_remove_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::Assignment,
            stable_id: assigned.assignment_id.clone(),
            before: Some(serde_json::to_value(&assigned).unwrap()),
            after: None,
        }];
        let unapplied_remove_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "remove_assignment".to_string(),
            rows: unapplied_remove_rows.clone(),
            face_ids: vec![assigned.face_id.clone()],
            media_keys: vec![assigned.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let unapplied_remove = MatchOperation {
            operation_id: "operation-unapplied-remove-assignment".to_string(),
            kind: "correction_remove_assignment".to_string(),
            face_id: Some(assigned.face_id.clone()),
            person_id: Some(assigned.person_id.clone()),
            before_json: serde_json::to_string(
                &unapplied_remove_rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            after_json: serde_json::to_string(&unapplied_remove_envelope).unwrap(),
            reversible: true,
            created_at: assigned.updated_at.clone(),
        };
        let unapplied_remove_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &unapplied_remove.operation_id,
                &assigned.media_key,
            ),
            media_key: assigned.media_key.clone(),
            media_fingerprint: media_fingerprint(&assigned.media_key),
            operation_id: unapplied_remove.operation_id.clone(),
            kind: "remove_assignment".to_string(),
            created_at: unapplied_remove.created_at.clone(),
        };
        let mut unapplied_remove_graph = graph.clone();
        unapplied_remove_graph.operations.push(unapplied_remove);
        unapplied_remove_graph
            .correction_media_operations
            .push(unapplied_remove_mapping);
        let error = validate_identity_graph(&unapplied_remove_graph).unwrap_err();
        assert!(
            error.contains("durable deletion contradicts its live terminal"),
            "unexpected unapplied removal lineage error: {error}"
        );

        let mut exactly_mapped_different = graph.clone();
        exactly_mapped_different
            .operations
            .push(valid_bound_different.clone());
        exactly_mapped_different
            .correction_media_operations
            .push(bound_different_mapping);
        exactly_mapped_different
            .assignments
            .retain(|assignment| assignment.assignment_id != strict_after.assignment_id);
        exactly_mapped_different
            .constraints
            .retain(|constraint| constraint.constraint_id != bound_constraint.constraint_id);
        exactly_mapped_different
            .constraints
            .push(bound_constraint.clone());
        validate_identity_graph(&exactly_mapped_different).unwrap();

        for strict_forgery in ["missing_generation", "incomplete_calibration"] {
            let mut forged_graph = exactly_mapped_different.clone();
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|operation| operation.operation_id == valid_bound_different.operation_id)
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let assignment_row = envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Assignment)
                .unwrap();
            let mut source: Assignment =
                serde_json::from_value(assignment_row.before.clone().unwrap()).unwrap();
            if strict_forgery == "missing_generation" {
                source.model_generation =
                    Some("model-missing-different-strict-history".to_string());
            } else {
                source.calibration_generation = None;
            }
            assignment_row.before = Some(serde_json::to_value(source).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            assert!(
                validate_identity_graph(&forged_graph).is_err(),
                "forged Different strict {strict_forgery} provenance was accepted"
            );
        }

        for source_forgery in ["operator_confirmed", "wrong_person"] {
            let mut forged_source_envelope = bound_different_envelope.clone();
            let mut forged_source: Assignment =
                serde_json::from_value(forged_source_envelope.rows[0].before.clone().unwrap())
                    .unwrap();
            if source_forgery == "operator_confirmed" {
                forged_source.state = "operator_confirmed".to_string();
                forged_source.locked = true;
                forged_source.provenance = "operator".to_string();
                forged_source.model_generation = None;
                forged_source.calibration_generation = None;
                forged_source.envelope_hash = None;
            } else {
                forged_source.person_id = "person-forged-different-source".to_string();
            }
            forged_source_envelope.rows[0].before =
                Some(serde_json::to_value(forged_source).unwrap());
            let mut forged_source_operation = valid_bound_different.clone();
            forged_source_operation.before_json = serde_json::to_string(
                &forged_source_envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            forged_source_operation.after_json =
                serde_json::to_string(&forged_source_envelope).unwrap();
            assert!(validate_match_operation(&forged_source_operation, &graph)
                .unwrap_err()
                .contains("non-qualifying Assignment source evidence"));
        }

        let mut operator_this_is_not_envelope = bound_different_envelope.clone();
        operator_this_is_not_envelope.kind = "this_is_not".to_string();
        let mut operator_this_is_not_source: Assignment = serde_json::from_value(
            operator_this_is_not_envelope.rows[0]
                .before
                .clone()
                .unwrap(),
        )
        .unwrap();
        operator_this_is_not_source.state = "operator_confirmed".to_string();
        operator_this_is_not_source.locked = true;
        operator_this_is_not_source.provenance = "operator".to_string();
        operator_this_is_not_source.model_generation = None;
        operator_this_is_not_source.calibration_generation = None;
        operator_this_is_not_source.envelope_hash = None;
        operator_this_is_not_envelope.rows[0].before =
            Some(serde_json::to_value(operator_this_is_not_source).unwrap());
        let mut operator_this_is_not = valid_bound_different.clone();
        operator_this_is_not.kind = "correction_this_is_not".to_string();
        operator_this_is_not.before_json = serde_json::to_string(
            &operator_this_is_not_envelope
                .rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        operator_this_is_not.after_json =
            serde_json::to_string(&operator_this_is_not_envelope).unwrap();
        validate_match_operation(&operator_this_is_not, &graph).unwrap();

        let mut constraint_only_envelope = bound_different_envelope.clone();
        constraint_only_envelope
            .rows
            .retain(|row| row.table != CorrectionTable::Assignment);
        let mut constraint_only_operation = valid_bound_different.clone();
        constraint_only_operation.before_json = serde_json::to_string(
            &constraint_only_envelope
                .rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        constraint_only_operation.after_json =
            serde_json::to_string(&constraint_only_envelope).unwrap();
        assert!(validate_match_operation(&constraint_only_operation, &graph)
            .unwrap_err()
            .contains("lacks a qualifying deleted suggestion or strict Assignment"));

        let suggestion_source = graph.suggestions.first().unwrap().clone();
        let suggestion_source_face = graph
            .faces
            .iter()
            .find(|face| face.face_id == suggestion_source.face_id)
            .unwrap();
        let suggestion_constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id(
                &suggestion_source.face_id,
                &suggestion_source.candidate_person_id,
            ),
            face_id: suggestion_source.face_id.clone(),
            person_id: suggestion_source.candidate_person_id.clone(),
            operation_id: "operation-suggestion-different".to_string(),
            operator_owned: true,
            created_at: suggestion_source.created_at.clone(),
        };
        let suggestion_different_rows = vec![
            CorrectionRowDelta {
                table: CorrectionTable::Suggestion,
                stable_id: suggestion_source.suggestion_id.clone(),
                before: Some(serde_json::to_value(&suggestion_source).unwrap()),
                after: None,
            },
            CorrectionRowDelta {
                table: CorrectionTable::Constraint,
                stable_id: suggestion_constraint.constraint_id.clone(),
                before: None,
                after: Some(serde_json::to_value(&suggestion_constraint).unwrap()),
            },
        ];
        let suggestion_different_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "different".to_string(),
            rows: suggestion_different_rows.clone(),
            face_ids: vec![suggestion_source.face_id.clone()],
            media_keys: vec![suggestion_source_face.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let suggestion_different_operation = MatchOperation {
            operation_id: suggestion_constraint.operation_id.clone(),
            kind: "correction_different".to_string(),
            face_id: Some(suggestion_constraint.face_id.clone()),
            person_id: Some(suggestion_constraint.person_id.clone()),
            before_json: serde_json::to_string(
                &suggestion_different_rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            after_json: serde_json::to_string(&suggestion_different_envelope).unwrap(),
            reversible: true,
            created_at: suggestion_constraint.created_at.clone(),
        };
        validate_match_operation(&suggestion_different_operation, &graph).unwrap();

        let suggestion_different_mapping = PortableCorrectionMediaOperation {
            mapping_id: exchange_correction_media_mapping_id(
                &suggestion_different_operation.operation_id,
                &suggestion_source_face.media_key,
            ),
            media_key: suggestion_source_face.media_key.clone(),
            media_fingerprint: suggestion_source_face.media_fingerprint.clone(),
            operation_id: suggestion_different_operation.operation_id.clone(),
            kind: "different".to_string(),
            created_at: suggestion_different_operation.created_at.clone(),
        };
        let mut suggestion_different_graph = graph.clone();
        suggestion_different_graph
            .operations
            .push(suggestion_different_operation.clone());
        suggestion_different_graph
            .correction_media_operations
            .push(suggestion_different_mapping);
        let mut different_provenance = SuggestionSourceProvenance {
            provenance_id: String::new(),
            operation_id: suggestion_different_operation.operation_id.clone(),
            operation_kind: "different".to_string(),
            suggestion_id: suggestion_source.suggestion_id.clone(),
            face_id: suggestion_source.face_id.clone(),
            candidate_person_id: suggestion_source.candidate_person_id.clone(),
            media_key: suggestion_source_face.media_key.clone(),
            media_fingerprint: suggestion_source.media_fingerprint.clone(),
            face_revision: suggestion_source.face_revision,
            person_revision: suggestion_source.person_revision,
            model_generation: suggestion_source.model_generation.clone(),
            job_id: suggestion_source.job_id.clone(),
            suggestion_created_at: suggestion_source.created_at.clone(),
            similarity_bits: suggestion_source.similarity.to_bits(),
            calibration_generation: suggestion_source.calibration_generation.clone(),
            envelope_hash: suggestion_source.envelope_hash.clone(),
            embedding_id: embedding_id(
                &suggestion_source.face_id,
                &suggestion_source.model_generation,
            ),
            embedding_created_at: suggestion_source.created_at.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            operation_created_at: suggestion_different_operation.created_at.clone(),
        };
        different_provenance.provenance_id = suggestion_source_provenance_id(&different_provenance);
        suggestion_different_graph
            .suggestion_source_provenance
            .push(different_provenance);
        suggestion_different_graph
            .suggestions
            .retain(|suggestion| suggestion.suggestion_id != suggestion_source.suggestion_id);
        suggestion_different_graph
            .constraints
            .retain(|constraint| constraint.constraint_id != suggestion_constraint.constraint_id);
        suggestion_different_graph
            .constraints
            .push(suggestion_constraint.clone());
        validate_identity_graph(&suggestion_different_graph).unwrap();
        let indexed_for_work = index_correction_envelope(&suggestion_different_operation)
            .unwrap()
            .unwrap();
        let correction_for_work = BTreeMap::from([(
            suggestion_different_operation.operation_id.as_str(),
            indexed_for_work,
        )]);
        let operation_for_work = BTreeMap::from([(
            suggestion_different_operation.operation_id.as_str(),
            &suggestion_different_operation,
        )]);
        let faces_for_work = suggestion_different_graph
            .faces
            .iter()
            .map(|row| (row.face_id.as_str(), row))
            .collect();
        let people_for_work = suggestion_different_graph
            .people
            .iter()
            .map(|row| (row.person_id.as_str(), row))
            .collect();
        let mut live_anchor_work = IDENTITY_BUNDLE_MAX_REFERENCES;
        for _ in 0..400 {
            validate_suggestion_source_anchor(
                &suggestion_different_operation,
                &suggestion_different_graph.suggestion_source_provenance[0],
                &faces_for_work,
                &people_for_work,
                &correction_for_work,
                &operation_for_work,
                &mut live_anchor_work,
            )
            .unwrap();
        }
        assert_eq!(live_anchor_work, IDENTITY_BUNDLE_MAX_REFERENCES);

        let mut exhausted_anchor_work = IDENTITY_BUNDLE_MAX_REFERENCES;
        assert!(validate_suggestion_source_anchor(
            &suggestion_different_operation,
            &suggestion_different_graph.suggestion_source_provenance[0],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &correction_for_work,
            &operation_for_work,
            &mut exhausted_anchor_work,
        )
        .unwrap_err()
        .contains("anchor work"));

        for coordinated_forgery in [
            "fingerprint",
            "future_revision",
            "future_person_revision",
            "zero_person_revision",
            "similarity",
            "calibration_envelope",
            "job",
            "timestamp",
        ] {
            let mut forged_graph = suggestion_different_graph.clone();
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|row| row.operation_id == suggestion_different_operation.operation_id)
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let suggestion_row = envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Suggestion)
                .unwrap();
            let mut forged_source: Suggestion =
                serde_json::from_value(suggestion_row.before.clone().unwrap()).unwrap();
            let provenance = forged_graph
                .suggestion_source_provenance
                .iter_mut()
                .find(|row| row.operation_id == suggestion_different_operation.operation_id)
                .unwrap();
            match coordinated_forgery {
                "fingerprint" => {
                    forged_source.media_fingerprint = "fingerprint-coordinated-forgery".to_string();
                    provenance.media_fingerprint = forged_source.media_fingerprint.clone();
                }
                "future_revision" => {
                    forged_source.face_revision += 10;
                    provenance.face_revision = forged_source.face_revision;
                }
                "future_person_revision" => {
                    forged_source.person_revision += 10;
                    provenance.person_revision = forged_source.person_revision;
                    provenance.provenance_id = suggestion_source_provenance_id(provenance);
                }
                "zero_person_revision" => {
                    forged_source.person_revision = 0;
                    provenance.person_revision = 0;
                    provenance.provenance_id = suggestion_source_provenance_id(provenance);
                }
                "similarity" => {
                    forged_source.similarity = 0.73;
                    provenance.similarity_bits = forged_source.similarity.to_bits();
                }
                "calibration_envelope" => {
                    forged_source.calibration_generation = Some("calibration-forged".to_string());
                    forged_source.envelope_hash = Some("b".repeat(64));
                    provenance.calibration_generation =
                        forged_source.calibration_generation.clone();
                    provenance.envelope_hash = forged_source.envelope_hash.clone();
                }
                "job" => {
                    forged_source.job_id = "job-coordinated-forgery".to_string();
                    provenance.job_id = forged_source.job_id.clone();
                }
                "timestamp" => {
                    forged_source.created_at = "2026-08-24T00:00:00Z".to_string();
                    provenance.suggestion_created_at = forged_source.created_at.clone();
                }
                _ => unreachable!(),
            }
            suggestion_row.before = Some(serde_json::to_value(forged_source).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            let rejected = if matches!(coordinated_forgery, "similarity" | "calibration_envelope") {
                let mut forged_bundle = IdentityBundleV1 {
                    manifest: IdentityBundleManifest {
                        format: IDENTITY_BUNDLE_FORMAT.to_string(),
                        version: IDENTITY_BUNDLE_VERSION,
                        schema_version: MATCH_SCHEMA_VERSION,
                        schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                        content_sha256: String::new(),
                    },
                    graph: forged_graph,
                };
                forged_bundle.manifest.content_sha256 =
                    bundle_content_sha256(&forged_bundle.graph).unwrap();
                validate_bundle(&forged_bundle).is_err()
            } else {
                validate_identity_graph(&forged_graph).is_err()
            };
            assert!(
                rejected,
                "coordinated {coordinated_forgery} mutation was accepted"
            );
        }
        let mut missing_provenance = suggestion_different_graph.clone();
        missing_provenance.suggestion_source_provenance.clear();
        assert!(validate_identity_graph(&missing_provenance)
            .unwrap_err()
            .contains("absent or ambiguous"));
        let mut extra_provenance = suggestion_different_graph.clone();
        let duplicated_provenance = extra_provenance.suggestion_source_provenance[0].clone();
        extra_provenance
            .suggestion_source_provenance
            .push(duplicated_provenance);
        assert!(validate_identity_graph(&extra_provenance).is_err());

        for source_forgery in [
            "empty_fingerprint",
            "changed_fingerprint",
            "zero_face_revision",
            "future_face_revision",
            "zero_person_revision",
            "future_person_revision",
            "missing_generation",
            "retired_generation",
            "empty_job",
            "changed_job",
            "empty_created_at",
            "created_before_embedding",
            "created_after_operation",
            "noncanonical_suggestion_id",
            "incomplete_calibration",
        ] {
            let mut forged_graph = suggestion_different_graph.clone();
            let mut retired_generation = None;
            if source_forgery == "retired_generation" {
                let mut generation = forged_graph.model_generations.first().unwrap().clone();
                generation.generation = "model-retired-different-history".to_string();
                generation.state = "retired".to_string();
                generation.validated = true;
                retired_generation = Some(generation.generation.clone());
                forged_graph.model_generations.push(generation);
            }
            let operation = forged_graph
                .operations
                .iter_mut()
                .find(|operation| {
                    operation.operation_id == suggestion_different_operation.operation_id
                })
                .unwrap();
            let mut envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&operation.after_json).unwrap();
            let suggestion_row = envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Suggestion)
                .unwrap();
            let mut source: Suggestion =
                serde_json::from_value(suggestion_row.before.clone().unwrap()).unwrap();
            match source_forgery {
                "empty_fingerprint" => source.media_fingerprint.clear(),
                "changed_fingerprint" => {
                    source.media_fingerprint = "fingerprint-forged-different-history".to_string()
                }
                "zero_face_revision" => source.face_revision = 0,
                "future_face_revision" => source.face_revision += 1,
                "zero_person_revision" => source.person_revision = 0,
                "future_person_revision" => source.person_revision += 1,
                "missing_generation" => {
                    source.model_generation = "model-missing-different-history".to_string()
                }
                "retired_generation" => source.model_generation = retired_generation.unwrap(),
                "empty_job" => source.job_id.clear(),
                "changed_job" => source.job_id = "job-forged-different-history".to_string(),
                "empty_created_at" => source.created_at.clear(),
                "created_before_embedding" => {
                    source.created_at = "2026-08-24T23:59:59Z".to_string()
                }
                "created_after_operation" => source.created_at = "2026-08-25T00:00:01Z".to_string(),
                "noncanonical_suggestion_id" => {
                    source.suggestion_id = "suggestion-forged-different-history".to_string()
                }
                "incomplete_calibration" => {
                    source.calibration_generation = Some("calibration-forged".to_string());
                    source.envelope_hash = None;
                }
                _ => unreachable!(),
            }
            suggestion_row.before = Some(serde_json::to_value(source).unwrap());
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(&envelope).unwrap();
            assert!(
                validate_identity_graph(&forged_graph).is_err(),
                "forged Different {source_forgery} source provenance was accepted"
            );
        }

        let mut wrong_candidate_envelope = suggestion_different_envelope;
        let mut wrong_candidate: Suggestion =
            serde_json::from_value(wrong_candidate_envelope.rows[0].before.clone().unwrap())
                .unwrap();
        wrong_candidate.candidate_person_id = "person-forged-different-candidate".to_string();
        wrong_candidate.suggestion_id = suggestion_id(
            &wrong_candidate.face_id,
            &wrong_candidate.candidate_person_id,
        );
        wrong_candidate_envelope.rows[0].stable_id = wrong_candidate.suggestion_id.clone();
        wrong_candidate_envelope.rows[0].before =
            Some(serde_json::to_value(wrong_candidate).unwrap());
        let mut wrong_candidate_operation = suggestion_different_operation;
        wrong_candidate_operation.before_json = serde_json::to_string(
            &wrong_candidate_envelope
                .rows
                .iter()
                .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        wrong_candidate_operation.after_json =
            serde_json::to_string(&wrong_candidate_envelope).unwrap();
        assert!(validate_match_operation(&wrong_candidate_operation, &graph)
            .unwrap_err()
            .contains("lacks a qualifying deleted suggestion or strict Assignment"));

        let mut unrelated_different_suggestion = graph.suggestions.first().unwrap().clone();
        unrelated_different_suggestion.face_id = assigned.face_id.clone();
        unrelated_different_suggestion.suggestion_id = suggestion_id(
            &unrelated_different_suggestion.face_id,
            &unrelated_different_suggestion.candidate_person_id,
        );
        let mut unrelated_different_envelope = bound_different_envelope.clone();
        unrelated_different_envelope.rows.push(CorrectionRowDelta {
            table: CorrectionTable::Suggestion,
            stable_id: unrelated_different_suggestion.suggestion_id.clone(),
            before: Some(serde_json::to_value(&unrelated_different_suggestion).unwrap()),
            after: None,
        });
        let unrelated_different_before = unrelated_different_envelope
            .rows
            .iter()
            .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
            .collect::<Vec<_>>();
        let mut unrelated_different = valid_bound_different.clone();
        unrelated_different.before_json =
            serde_json::to_string(&unrelated_different_before).unwrap();
        unrelated_different.after_json =
            serde_json::to_string(&unrelated_different_envelope).unwrap();
        assert!(validate_match_operation(&unrelated_different, &graph)
            .unwrap_err()
            .contains("auxiliary deletion targets another FaceId"));

        let mut forged_different_identity = valid_bound_different.clone();
        forged_different_identity.face_id = Some(strict_after.face_id.clone());
        forged_different_identity.person_id = Some("forged-person".to_string());
        let mut forged_different_envelope = bound_different_envelope.clone();
        forged_different_envelope.face_ids = vec![strict_after.face_id.clone()];
        forged_different_identity.after_json =
            serde_json::to_string(&forged_different_envelope).unwrap();
        assert!(validate_match_operation(&forged_different_identity, &graph)
            .unwrap_err()
            .contains("identity envelope contradicts its constraint effects"));

        let not_sure_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "not_sure".to_string(),
            rows: different_rows,
            face_ids: vec![assigned.face_id.clone()],
            media_keys: vec![assigned.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let forged_not_sure = MatchOperation {
            operation_id: "operation-forged-not-sure".to_string(),
            kind: "correction_not_sure".to_string(),
            face_id: Some(assigned.face_id),
            person_id: Some(assigned.person_id),
            before_json: "[]".to_string(),
            after_json: serde_json::to_string(&not_sure_envelope).unwrap(),
            reversible: false,
            created_at: assigned.updated_at,
        };
        assert!(validate_match_operation(&forged_not_sure, &graph)
            .unwrap_err()
            .contains("Not-sure correction must record no state effects"));

        let missing_person_not_sure_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "not_sure".to_string(),
            rows: Vec::new(),
            face_ids: vec![bound_constraint.face_id.clone()],
            media_keys: vec![assigned.media_key.clone()],
            identity_changed: false,
            catalog_changed: false,
        };
        let missing_person_not_sure = MatchOperation {
            operation_id: "operation-missing-not-sure-person".to_string(),
            kind: "correction_not_sure".to_string(),
            face_id: Some(bound_constraint.face_id),
            person_id: None,
            before_json: "[]".to_string(),
            after_json: serde_json::to_string(&missing_person_not_sure_envelope).unwrap(),
            reversible: false,
            created_at: bound_constraint.created_at,
        };
        assert!(validate_match_operation(&missing_person_not_sure, &graph)
            .unwrap_err()
            .contains("does not bind its Face/Person identity"));

        let mut unknown_field = graph.clone();
        let authorization = unknown_field
            .operations
            .iter_mut()
            .find(|operation| operation.operation_id == "operation-membership")
            .unwrap();
        let mut after: Value = serde_json::from_str(&authorization.after_json).unwrap();
        after
            .as_object_mut()
            .unwrap()
            .insert("future_authorized".to_string(), Value::Bool(true));
        authorization.after_json = serde_json::to_string(&after).unwrap();
        assert!(validate_identity_graph(&unknown_field)
            .unwrap_err()
            .contains("unsupported fields or shape"));

        let mut duplicate_field = graph.clone();
        let authorization = duplicate_field
            .operations
            .iter_mut()
            .find(|operation| operation.operation_id == "operation-membership")
            .unwrap();
        authorization.after_json = authorization.after_json.replacen(
            "\"authorized\":true",
            "\"authorized\":false,\"authorized\":true",
            1,
        );
        assert!(validate_identity_graph(&duplicate_field)
            .unwrap_err()
            .contains("duplicate object key"));
    }

    #[test]
    fn remove_person_portable_contract_binds_owned_closure_and_exact_face_set() {
        fn operation_with_envelope(
            template: &MatchOperation,
            envelope: &ExchangeCorrectionDeltaEnvelope,
        ) -> MatchOperation {
            let mut operation = template.clone();
            operation.before_json = serde_json::to_string(
                &envelope
                    .rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            operation.after_json = serde_json::to_string(envelope).unwrap();
            operation
        }

        let source_root = TestRoot::new("remove-person-portable-contract-source");
        let source_media = source_root.0.join("media-root");
        fs::create_dir_all(&source_media).unwrap();
        let source = MatchStore::open(&source_root.0).unwrap();
        seed_portable_graph(&source, &source_media);
        seed_trusted_projection_and_calibration(&source);
        let constraint_face = face("face-constraint-only", "media/constraint-only.jpg");
        source
            .upsert_json(FACE_TABLE, &constraint_face.face_id, &constraint_face)
            .unwrap();
        fs::create_dir_all(source_media.join("media")).unwrap();
        fs::write(
            source_media.join("media/constraint-only.jpg"),
            b"fixture-media/constraint-only.jpg",
        )
        .unwrap();
        let constraint = CannotLinkConstraint {
            constraint_id: cannot_link_id("face-constraint-only", "person-a"),
            face_id: "face-constraint-only".to_string(),
            person_id: "person-a".to_string(),
            operation_id: "operation-remove-person-constraint-source".to_string(),
            operator_owned: true,
            created_at: "2026-08-25T00:00:00Z".to_string(),
        };
        source
            .upsert_json(CONSTRAINT_TABLE, &constraint.constraint_id, &constraint)
            .unwrap();

        let preview = source.preview_remove_person("person-a").unwrap();
        assert!(preview.face_ids.contains(&"face-a".to_string()));
        assert!(preview.face_ids.contains(&"face-suggestion".to_string()));
        assert!(preview
            .face_ids
            .contains(&"face-constraint-only".to_string()));
        assert!(preview
            .media_keys
            .contains(&"media/constraint-only.jpg".to_string()));
        source.remove_person(&preview).unwrap();
        let bundle = source.build_identity_bundle().unwrap();
        let operation = bundle
            .graph
            .operations
            .iter()
            .find(|operation| operation.kind == "correction_remove_person")
            .unwrap()
            .clone();
        let envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        validate_correction_operation(&operation, &envelope).unwrap();
        assert_eq!(envelope.face_ids, preview.face_ids);
        assert_eq!(envelope.media_keys, preview.media_keys);
        let mut missing_constraint_media = bundle.graph.clone();
        let missing_operation = missing_constraint_media
            .operations
            .iter_mut()
            .find(|candidate| candidate.operation_id == operation.operation_id)
            .unwrap();
        let mut missing_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&missing_operation.after_json).unwrap();
        missing_envelope
            .media_keys
            .retain(|media_key| media_key != "media/constraint-only.jpg");
        missing_operation.after_json = serde_json::to_string(&missing_envelope).unwrap();
        missing_constraint_media
            .correction_media_operations
            .retain(|mapping| {
                mapping.operation_id != operation.operation_id
                    || mapping.media_key != "media/constraint-only.jpg"
            });
        let error = validate_identity_graph(&missing_constraint_media).unwrap_err();
        assert!(
            error.contains(
                "correction media set does not exactly match its canonical current or historical evidence"
            ),
            "{error}"
        );
        for table in [
            CorrectionTable::Constraint,
            CorrectionTable::Suggestion,
            CorrectionTable::TrustedSearch,
            CorrectionTable::TrustedMember,
            CorrectionTable::TemplateSet,
            CorrectionTable::Look,
        ] {
            assert!(
                envelope.rows.iter().any(|row| row.table == table),
                "producer remove-Person fixture omitted {table:?}"
            );
        }

        let bundle_path = source_root.0.join("remove-person-portable-contract.json");
        source.export_identity_bundle(&bundle_path).unwrap();
        let target_root = TestRoot::new("remove-person-portable-contract-target");
        let relocated_media = target_root.0.join("relocated-media");
        for media_key in [
            "media/a.jpg",
            "media/b.jpg",
            "media/c.jpg",
            "media/constraint-only.jpg",
        ] {
            let destination = relocated_media.join(media_key);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source_media.join(media_key), destination).unwrap();
        }
        let relocations = BTreeMap::from([(
            "root-a".to_string(),
            fs::canonicalize(&relocated_media)
                .unwrap()
                .to_string_lossy()
                .to_string(),
        )]);
        let target = MatchStore::open(&target_root.0).unwrap();
        let plan = target
            .preview_identity_bundle_import(&bundle_path, &relocations, IdentityImportMode::Replace)
            .unwrap();
        assert!(plan.unresolved_root_ids.is_empty());
        assert!(plan.unresolved_media_keys.is_empty());
        target.apply_identity_bundle_import(plan).unwrap();
        assert!(target
            .build_identity_bundle()
            .unwrap()
            .graph
            .operations
            .iter()
            .any(|candidate| candidate.operation_id == operation.operation_id));

        for hostile_kind in ["constraint", "suggestion", "trusted_search"] {
            let mut hostile = envelope.clone();
            let row = hostile
                .rows
                .iter_mut()
                .find(|row| {
                    row.table
                        == match hostile_kind {
                            "constraint" => CorrectionTable::Constraint,
                            "suggestion" => CorrectionTable::Suggestion,
                            "trusted_search" => CorrectionTable::TrustedSearch,
                            _ => unreachable!(),
                        }
                })
                .unwrap();
            match hostile_kind {
                "constraint" => {
                    let mut value: CannotLinkConstraint =
                        serde_json::from_value(row.before.clone().unwrap()).unwrap();
                    value.person_id = "person-b".to_string();
                    value.constraint_id = cannot_link_id(&value.face_id, &value.person_id);
                    row.stable_id = value.constraint_id.clone();
                    row.before = Some(serde_json::to_value(value).unwrap());
                }
                "suggestion" => {
                    let mut value: Suggestion =
                        serde_json::from_value(row.before.clone().unwrap()).unwrap();
                    value.candidate_person_id = "person-b".to_string();
                    value.suggestion_id = suggestion_id(&value.face_id, &value.candidate_person_id);
                    row.stable_id = value.suggestion_id.clone();
                    row.before = Some(serde_json::to_value(value).unwrap());
                }
                "trusted_search" => {
                    let mut value = decode_correction_trusted_search(
                        row.before.as_ref().unwrap(),
                        "hostile remove-Person TrustedSearch",
                    )
                    .unwrap();
                    value.person_id = "person-b".to_string();
                    row.before = Some(serde_json::to_value(value).unwrap());
                }
                _ => unreachable!(),
            }
            let hostile_operation = operation_with_envelope(&operation, &hostile);
            let error = validate_correction_operation(&hostile_operation, &hostile).unwrap_err();
            assert!(
                error.contains(&format!(
                    "remove-Person {} targets another Person",
                    match hostile_kind {
                        "constraint" => "Constraint",
                        "suggestion" => "Suggestion",
                        "trusted_search" => "TrustedSearch",
                        _ => unreachable!(),
                    }
                )),
                "unexpected remove-Person {hostile_kind} rejection: {error}"
            );
        }

        let mut missing_set = envelope.clone();
        missing_set
            .rows
            .retain(|row| row.table != CorrectionTable::TemplateSet);
        let missing_set_operation = operation_with_envelope(&operation, &missing_set);
        assert!(
            validate_correction_operation(&missing_set_operation, &missing_set)
                .unwrap_err()
                .contains("deleted TemplateSet closure")
        );

        let mut missing_look = envelope.clone();
        missing_look
            .rows
            .retain(|row| row.table != CorrectionTable::Look);
        let missing_look_operation = operation_with_envelope(&operation, &missing_look);
        assert!(
            validate_correction_operation(&missing_look_operation, &missing_look)
                .unwrap_err()
                .contains("deleted Look closure")
        );

        let mut unrelated_member = envelope.clone();
        let member_row = unrelated_member
            .rows
            .iter_mut()
            .find(|row| row.table == CorrectionTable::TrustedMember)
            .unwrap();
        let mut member: TrustedTemplateMembership =
            serde_json::from_value(member_row.before.clone().unwrap()).unwrap();
        member.set_id = "set-person-b".to_string();
        member.membership_id = trusted_member_id(&member.set_id, &member.face_id);
        member_row.stable_id = member.membership_id.clone();
        member_row.before = Some(serde_json::to_value(member).unwrap());
        let unrelated_member_operation = operation_with_envelope(&operation, &unrelated_member);
        assert!(
            validate_correction_operation(&unrelated_member_operation, &unrelated_member)
                .unwrap_err()
                .contains("deleted TemplateSet closure")
        );

        let mut unrelated_set = envelope.clone();
        let set_row = unrelated_set
            .rows
            .iter_mut()
            .find(|row| row.table == CorrectionTable::TemplateSet)
            .unwrap();
        let mut set: TrustedTemplateSet =
            serde_json::from_value(set_row.before.clone().unwrap()).unwrap();
        set.look_id = "look-person-b".to_string();
        set_row.before = Some(serde_json::to_value(set).unwrap());
        let unrelated_set_operation = operation_with_envelope(&operation, &unrelated_set);
        let unrelated_set_error =
            validate_correction_operation(&unrelated_set_operation, &unrelated_set).unwrap_err();
        assert!(
            unrelated_set_error.contains("remove-Person")
                && (unrelated_set_error.contains("deleted Look closure")
                    || unrelated_set_error.contains("unrelated ownership")),
            "unexpected unrelated TemplateSet rejection: {unrelated_set_error}"
        );

        let mut unrelated_person_closure = envelope.clone();
        let look_row = unrelated_person_closure
            .rows
            .iter_mut()
            .find(|row| row.table == CorrectionTable::Look)
            .unwrap();
        let mut look: Look = serde_json::from_value(look_row.before.clone().unwrap()).unwrap();
        look.person_id = "person-b".to_string();
        look_row.before = Some(serde_json::to_value(look).unwrap());
        let unrelated_person_operation =
            operation_with_envelope(&operation, &unrelated_person_closure);
        let unrelated_person_error =
            validate_correction_operation(&unrelated_person_operation, &unrelated_person_closure)
                .unwrap_err();
        assert!(
            unrelated_person_error.contains("remove-Person")
                && (unrelated_person_error.contains("Look effect targets another Person")
                    || unrelated_person_error.contains("unrelated ownership")),
            "unexpected unrelated Person closure rejection: {unrelated_person_error}"
        );

        let mut extra_face = envelope.clone();
        extra_face.face_ids.push("face-does-not-exist".to_string());
        extra_face.face_ids.sort();
        let extra_face_operation = operation_with_envelope(&operation, &extra_face);
        assert!(
            validate_correction_operation(&extra_face_operation, &extra_face)
                .unwrap_err()
                .contains("does not exactly match its typed effects")
        );
    }

    #[test]
    fn undo_restored_float_membership_uses_strict_typed_effect_equality() {
        let root = TestRoot::new("undo-membership-operation-integrity");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let mut graph = store.build_identity_bundle().unwrap().graph;

        let original = graph.trusted_memberships[0].clone();
        let lexical_before = serde_json::to_value(&original).unwrap();
        let mut lexical_after = lexical_before.clone();
        lexical_after["quality_score"] = serde_json::json!(0.9_f64);
        assert_ne!(lexical_before, lexical_after);
        let lexical_rows = vec![CorrectionRowDelta {
            table: CorrectionTable::TrustedMember,
            stable_id: original.membership_id.clone(),
            before: Some(lexical_before),
            after: Some(lexical_after),
        }];
        let lexical_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "remove_person".to_string(),
            rows: lexical_rows.clone(),
            face_ids: vec![original.face_id.clone()],
            media_keys: Vec::new(),
            identity_changed: true,
            catalog_changed: true,
        };
        let lexical_operation = MatchOperation {
            operation_id: "operation-lexical-float-noop".to_string(),
            kind: "correction_remove_person".to_string(),
            face_id: Some(original.face_id.clone()),
            person_id: graph.assignments.first().map(|row| row.person_id.clone()),
            before_json: serde_json::to_string(
                &lexical_rows
                    .iter()
                    .map(|row| (row.table.clone(), row.stable_id.clone(), row.before.clone()))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            after_json: serde_json::to_string(&lexical_envelope).unwrap(),
            reversible: true,
            created_at: original.created_at.clone(),
        };
        assert!(validate_match_operation(&lexical_operation, &graph)
            .unwrap_err()
            .contains("no-op correction row"));

        let original_suggestion = graph.suggestions[0].clone();
        let mut signed_zero_before = original_suggestion.clone();
        signed_zero_before.similarity = -0.0;
        let mut signed_zero_after = signed_zero_before.clone();
        signed_zero_after.similarity = 0.0;
        let signed_zero_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "remove_person".to_string(),
            rows: vec![CorrectionRowDelta {
                table: CorrectionTable::Suggestion,
                stable_id: signed_zero_before.suggestion_id.clone(),
                before: Some(serde_json::to_value(&signed_zero_before).unwrap()),
                after: Some(serde_json::to_value(&signed_zero_after).unwrap()),
            }],
            face_ids: vec![signed_zero_before.face_id.clone()],
            media_keys: Vec::new(),
            identity_changed: true,
            catalog_changed: true,
        };
        assert!(validate_correction_envelope(
            &lexical_operation,
            &signed_zero_envelope,
            "remove_person"
        )
        .unwrap_err()
        .contains("no-op correction row"));

        let preview = store.preview_remove_person("person-a").unwrap();
        let deletion = store.remove_person(&preview).unwrap();
        store.undo_correction(&deletion.operation_id).unwrap();
        graph = store.build_identity_bundle().unwrap().graph;
        let delete_id = deletion.operation_id;
        let undo_id = format!("undo-{delete_id}");
        let restored = graph
            .trusted_memberships
            .iter()
            .find(|membership| membership.membership_id == original.membership_id)
            .unwrap();
        assert_eq!(restored.operation_id, undo_id);
        let restored_suggestion = graph
            .suggestions
            .iter()
            .find(|suggestion| suggestion.candidate_person_id == "person-a")
            .unwrap();
        let restored_person = graph
            .people
            .iter()
            .find(|person| person.person_id == restored_suggestion.candidate_person_id)
            .unwrap();
        let restored_face = graph
            .faces
            .iter()
            .find(|face| face.face_id == restored_suggestion.face_id)
            .unwrap();
        assert_eq!(restored_suggestion, &original_suggestion);
        assert!(restored_suggestion.person_revision < restored_person.revision);
        assert_eq!(
            restored_suggestion.face_revision,
            restored_face.face_revision
        );
        validate_identity_graph(&graph).unwrap();

        let mut forged_source_after = graph.clone();
        let forged_undo = forged_source_after
            .operations
            .iter_mut()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let mut forged_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&forged_undo.after_json).unwrap();
        let membership_row = forged_envelope
            .rows
            .iter_mut()
            .find(|row| row.table == CorrectionTable::TrustedMember)
            .unwrap();
        let mut forged_before = membership_row.after.clone().unwrap();
        forged_before["face_revision"] = serde_json::json!(9_999);
        membership_row.before = Some(forged_before);
        forged_undo.after_json = serde_json::to_string(&forged_envelope).unwrap();
        assert!(validate_identity_graph(&forged_source_after)
            .unwrap_err()
            .contains("undo before-state does not equal source"));

        let mut forged_affected_set = graph.clone();
        let restored_face_id = forged_affected_set.trusted_memberships[0].face_id.clone();
        let unrelated_face_id = forged_affected_set
            .faces
            .iter()
            .find(|face| face.face_id != restored_face_id)
            .unwrap()
            .face_id
            .clone();
        let forged_undo = forged_affected_set
            .operations
            .iter_mut()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let mut forged_envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&forged_undo.after_json).unwrap();
        forged_envelope.face_ids = vec![unrelated_face_id];
        forged_undo.after_json = serde_json::to_string(&forged_envelope).unwrap();
        assert!(validate_identity_graph(&forged_affected_set)
            .unwrap_err()
            .contains("undo identity or affected sets contradict source"));

        let mut forged_row_set = graph.clone();
        {
            let forged_undo = forged_row_set
                .operations
                .iter_mut()
                .find(|operation| operation.kind == "undo_correction")
                .unwrap();
            let mut forged_envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&forged_undo.after_json).unwrap();
            let membership_row = forged_envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::TrustedMember)
                .unwrap();
            let mut forged_membership: TrustedTemplateMembership =
                serde_json::from_value(membership_row.after.clone().unwrap()).unwrap();
            forged_membership.set_id = "set-forged-undo-row".to_string();
            forged_membership.membership_id =
                trusted_member_id(&forged_membership.set_id, &forged_membership.face_id);
            membership_row.stable_id = forged_membership.membership_id.clone();
            membership_row.after = Some(serde_json::to_value(forged_membership).unwrap());
            forged_undo.after_json = serde_json::to_string(&forged_envelope).unwrap();
        }
        let forged_undo = forged_row_set
            .operations
            .iter()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let indexed = index_correction_envelope(forged_undo).unwrap().unwrap();
        let operation_by_id = forged_row_set
            .operations
            .iter()
            .map(|operation| (operation.operation_id.as_str(), operation))
            .collect::<BTreeMap<_, _>>();
        assert!(
            validate_undo_operation(forged_undo, &operation_by_id, &indexed.envelope)
                .unwrap_err()
                .contains("undo row set contradicts source")
        );

        let mut forged_inverse = graph.clone();
        {
            let forged_undo = forged_inverse
                .operations
                .iter_mut()
                .find(|operation| operation.kind == "undo_correction")
                .unwrap();
            let mut forged_envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&forged_undo.after_json).unwrap();
            let membership_row = forged_envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::TrustedMember)
                .unwrap();
            membership_row.after.as_mut().unwrap()["quality_score"] = serde_json::json!(0.8);
            forged_undo.after_json = serde_json::to_string(&forged_envelope).unwrap();
        }
        let forged_undo = forged_inverse
            .operations
            .iter()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let indexed = index_correction_envelope(forged_undo).unwrap().unwrap();
        let operation_by_id = forged_inverse
            .operations
            .iter()
            .map(|operation| (operation.operation_id.as_str(), operation))
            .collect::<BTreeMap<_, _>>();
        assert!(
            validate_undo_operation(forged_undo, &operation_by_id, &indexed.envelope,)
                .unwrap_err()
                .contains("undo after-state is not the permitted inverse")
        );

        let mut forged_suggestion_inverse = graph.clone();
        {
            let forged_undo = forged_suggestion_inverse
                .operations
                .iter_mut()
                .find(|operation| operation.kind == "undo_correction")
                .unwrap();
            let mut forged_envelope: ExchangeCorrectionDeltaEnvelope =
                serde_json::from_str(&forged_undo.after_json).unwrap();
            forged_envelope
                .rows
                .iter_mut()
                .find(|row| row.table == CorrectionTable::Suggestion)
                .unwrap()
                .after
                .as_mut()
                .unwrap()["similarity"] = serde_json::json!(0.123_f32);
            forged_undo.after_json = serde_json::to_string(&forged_envelope).unwrap();
        }
        let forged_undo = forged_suggestion_inverse
            .operations
            .iter()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let indexed = index_correction_envelope(forged_undo).unwrap().unwrap();
        let operation_by_id = forged_suggestion_inverse
            .operations
            .iter()
            .map(|operation| (operation.operation_id.as_str(), operation))
            .collect::<BTreeMap<_, _>>();
        assert!(
            validate_undo_operation(forged_undo, &operation_by_id, &indexed.envelope)
                .unwrap_err()
                .contains("undo after-state is not the permitted inverse")
        );

        let mut duplicate_field = graph.clone();
        let undo = duplicate_field
            .operations
            .iter_mut()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        undo.after_json = undo.after_json.replacen(
            "\"authorized\":true",
            "\"authorized\":false,\"authorized\":true",
            1,
        );
        assert!(validate_identity_graph(&duplicate_field)
            .unwrap_err()
            .contains("duplicate object key"));

        let mut unknown_field = graph;
        let undo = unknown_field
            .operations
            .iter_mut()
            .find(|operation| operation.kind == "undo_correction")
            .unwrap();
        let mut envelope: ExchangeCorrectionDeltaEnvelope =
            serde_json::from_str(&undo.after_json).unwrap();
        envelope
            .rows
            .iter_mut()
            .find(|row| row.table == CorrectionTable::TrustedMember)
            .unwrap()
            .after
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("future_authorized".to_string(), Value::Bool(true));
        undo.after_json = serde_json::to_string(&envelope).unwrap();
        assert!(validate_identity_graph(&unknown_field)
            .unwrap_err()
            .contains("unsupported fields or shape"));
    }

    #[test]
    fn indexed_assignment_owner_preserves_lineage_and_tolerates_rekey_metadata() {
        let root = TestRoot::new("indexed-assignment-owner");
        let store = MatchStore::open(&root.0).unwrap();
        let media_root = root.0.join("media-root");
        fs::create_dir_all(&media_root).unwrap();
        seed_portable_graph(&store, &media_root);
        let graph = store.build_identity_bundle().unwrap().graph;
        let original = graph
            .assignments
            .iter()
            .find(|assignment| assignment.face_id == "face-a")
            .unwrap()
            .clone();

        let correction_id = "operation-indexed-change-person".to_string();
        let mut recorded = original.clone();
        recorded.operation_id = correction_id.clone();
        recorded.provenance = "change_person".to_string();
        let correction_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "change_person".to_string(),
            rows: vec![CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: recorded.assignment_id.clone(),
                before: Some(serde_json::to_value(&original).unwrap()),
                after: Some(serde_json::to_value(&recorded).unwrap()),
            }],
            face_ids: vec![recorded.face_id.clone()],
            media_keys: vec![recorded.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let correction_operation = MatchOperation {
            operation_id: correction_id,
            kind: "correction_change_person".to_string(),
            face_id: Some(recorded.face_id.clone()),
            person_id: Some(recorded.person_id.clone()),
            before_json: "[]".to_string(),
            after_json: serde_json::to_string(&correction_envelope).unwrap(),
            reversible: true,
            created_at: recorded.updated_at.clone(),
        };
        let indexed = index_correction_envelope(&correction_operation)
            .unwrap()
            .unwrap();
        validate_live_assignment_owner(&recorded, &correction_operation, Some(&indexed)).unwrap();
        let mut forged_live = recorded.clone();
        forged_live.person_id = "forged-person".to_string();
        assert!(validate_live_assignment_owner(
            &forged_live,
            &correction_operation,
            Some(&indexed)
        )
        .unwrap_err()
        .contains("logical/evidence ownership"));

        let undo_id = "undo-operation-assignment-source".to_string();
        let mut undo_recorded = original;
        undo_recorded.operation_id = undo_id.clone();
        let undo_envelope = ExchangeCorrectionDeltaEnvelope {
            version: 1,
            kind: "undo_correction".to_string(),
            rows: vec![CorrectionRowDelta {
                table: CorrectionTable::Assignment,
                stable_id: undo_recorded.assignment_id.clone(),
                before: None,
                after: Some(serde_json::to_value(&undo_recorded).unwrap()),
            }],
            face_ids: vec![undo_recorded.face_id.clone()],
            media_keys: vec![undo_recorded.media_key.clone()],
            identity_changed: true,
            catalog_changed: false,
        };
        let undo_operation = MatchOperation {
            operation_id: undo_id,
            kind: "undo_correction".to_string(),
            face_id: Some(undo_recorded.face_id.clone()),
            person_id: Some(undo_recorded.person_id.clone()),
            before_json: serde_json::json!({
                "operation_id": "operation-assignment-source",
                "conflict_rows": 0,
            })
            .to_string(),
            after_json: serde_json::to_string(&undo_envelope).unwrap(),
            reversible: false,
            created_at: undo_recorded.updated_at.clone(),
        };
        let indexed = index_correction_envelope(&undo_operation).unwrap().unwrap();
        let mut rekeyed_live = undo_recorded;
        rekeyed_live.media_key = "moved/media-a.jpg".to_string();
        rekeyed_live.face_revision += 1;
        rekeyed_live.updated_at = "2026-08-25T01:00:00Z".to_string();
        validate_live_assignment_owner(&rekeyed_live, &undo_operation, Some(&indexed)).unwrap();
    }

    #[test]
    fn exact_live_delta_distinguishes_omitted_and_deleted_effects() {
        fn assert_rejected<T>(table: CorrectionTable, stable_id: &str, current: &T)
        where
            T: Serialize + serde::de::DeserializeOwned + PartialEq,
        {
            let operation = MatchOperation {
                operation_id: "undo-operation-effect-source".to_string(),
                kind: "undo_correction".to_string(),
                face_id: None,
                person_id: None,
                before_json: "null".to_string(),
                after_json: "null".to_string(),
                reversible: false,
                created_at: "2026-08-25T00:00:00Z".to_string(),
            };
            let omitted = IndexedCorrectionEnvelope {
                envelope: ExchangeCorrectionDeltaEnvelope {
                    version: 1,
                    kind: "undo_correction".to_string(),
                    rows: Vec::new(),
                    face_ids: Vec::new(),
                    media_keys: Vec::new(),
                    identity_changed: true,
                    catalog_changed: true,
                },
                rows: BTreeMap::new(),
            };
            assert!(require_exact_live_delta(
                &operation,
                &omitted,
                table.clone(),
                stable_id,
                current
            )
            .unwrap_err()
            .contains("omits its live typed row effect"));

            let deleted_row = CorrectionRowDelta {
                table: table.clone(),
                stable_id: stable_id.to_string(),
                before: Some(serde_json::to_value(current).unwrap()),
                after: None,
            };
            let deleted = IndexedCorrectionEnvelope {
                envelope: ExchangeCorrectionDeltaEnvelope {
                    version: 1,
                    kind: "undo_correction".to_string(),
                    rows: vec![deleted_row],
                    face_ids: Vec::new(),
                    media_keys: Vec::new(),
                    identity_changed: true,
                    catalog_changed: true,
                },
                rows: BTreeMap::from([(
                    (
                        correction_table_key(&table).to_string(),
                        stable_id.to_string(),
                    ),
                    0,
                )]),
            };
            assert!(
                require_exact_live_delta(&operation, &deleted, table, stable_id, current)
                    .unwrap_err()
                    .contains("deletes the live typed row effect")
            );
        }

        let membership = TrustedTemplateMembership {
            membership_id: "membership".to_string(),
            set_id: "set".to_string(),
            look_id: "look".to_string(),
            face_id: "face".to_string(),
            authorized: true,
            alignment_valid: true,
            quality_passed: true,
            pose_passed: true,
            diversity_passed: true,
            provenance: "operator".to_string(),
            model_generation: "model".to_string(),
            embedding_id: "embedding".to_string(),
            media_fingerprint: "fingerprint".to_string(),
            face_revision: 1,
            quality_score: 0.9,
            quality_threshold: 0.7,
            pose_bucket: "frontal".to_string(),
            policy_version: TRUSTED_POLICY_VERSION.to_string(),
            operation_id: "undo-operation-effect-source".to_string(),
            created_at: "2026-08-25T00:00:00Z".to_string(),
        };
        let constraint = CannotLinkConstraint {
            constraint_id: "constraint".to_string(),
            face_id: "face".to_string(),
            person_id: "person".to_string(),
            operation_id: "undo-operation-effect-source".to_string(),
            operator_owned: true,
            created_at: "2026-08-25T00:00:00Z".to_string(),
        };
        let disposition = FaceDisposition {
            face_id: "face".to_string(),
            media_key: "media.jpg".to_string(),
            disposition: "ignored".to_string(),
            operation_id: "undo-operation-effect-source".to_string(),
            face_revision: 1,
            created_at: "2026-08-25T00:00:00Z".to_string(),
            updated_at: "2026-08-25T00:00:00Z".to_string(),
        };
        assert_rejected(CorrectionTable::TrustedMember, "membership", &membership);
        assert_rejected(CorrectionTable::Constraint, "constraint", &constraint);
        assert_rejected(CorrectionTable::Disposition, "face", &disposition);
    }

    #[test]
    fn hostile_scalar_container_schema_and_symlink_boundaries_are_closed() {
        for (label, limit) in [
            ("bytes", IDENTITY_BUNDLE_MAX_BYTES),
            ("decoded", IDENTITY_BUNDLE_MAX_DECODED_BYTES),
            ("entities", IDENTITY_BUNDLE_MAX_ENTITIES),
            ("strings", IDENTITY_BUNDLE_MAX_STRING_BYTES),
            ("references", IDENTITY_BUNDLE_MAX_REFERENCES),
            ("json_tokens", IDENTITY_BUNDLE_MAX_JSON_TOKENS),
            ("xmp", IDENTITY_XMP_MAX_SIDECAR_BYTES),
        ] {
            enforce_limit(label, limit, limit).unwrap();
            assert!(enforce_limit(label, limit + 1, limit).is_err());
        }
        let mut aggregate_strings = IDENTITY_BUNDLE_MAX_STRING_BYTES - 1;
        add_string_bytes("x", &mut aggregate_strings).unwrap();
        assert_eq!(aggregate_strings, IDENTITY_BUNDLE_MAX_STRING_BYTES);
        assert!(add_string_bytes("x", &mut aggregate_strings).is_err());
        let ids = BTreeSet::from(["id".to_string()]);
        let mut references = IDENTITY_BUNDLE_MAX_REFERENCES - 1;
        require_ref("boundary", "id", &ids, &mut references).unwrap();
        assert_eq!(references, IDENTITY_BUNDLE_MAX_REFERENCES);
        assert!(require_ref("boundary", "id", &ids, &mut references).is_err());

        let oversized_unknown_array = format!(
            "{{\"unknown\":[{}]}}",
            std::iter::repeat_n("0", IDENTITY_BUNDLE_MAX_ENTITIES + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            preflight_identity_bundle_json(oversized_unknown_array.as_bytes())
                .unwrap_err()
                .contains("collection exceeds")
        );
        preflight_identity_bundle_json_with_limits(b"[0,0]", 3, 2).unwrap();
        assert!(preflight_identity_bundle_json_with_limits(b"[0,0]", 2, 2)
            .unwrap_err()
            .contains("token count exceeds"));

        let mut exact_depth = Value::Null;
        for _ in 1..IDENTITY_BUNDLE_MAX_NESTING_DEPTH {
            exact_depth = Value::Array(vec![exact_depth]);
        }
        let mut strings = 0;
        let mut depth = 0;
        inspect_json_limits(&exact_depth, 1, &mut depth, &mut strings).unwrap();
        let over_depth = Value::Array(vec![exact_depth]);
        let mut strings = 0;
        let mut depth = 0;
        assert!(inspect_json_limits(&over_depth, 1, &mut depth, &mut strings).is_err());

        for magic in [
            b"PK\x03\x04".as_slice(),
            b"\x1f\x8b".as_slice(),
            b"7z\xbc\xaf\x27\x1c".as_slice(),
            b"Rar!\x1a\x07".as_slice(),
            b"BZh".as_slice(),
            b"\xfd7zXZ\x00".as_slice(),
            b"\x28\xb5\x2f\xfd".as_slice(),
        ] {
            assert!(reject_archive_or_compression_magic(magic).is_err());
        }
        for invalid in [
            "../escape",
            "root/../escape",
            "/absolute",
            "C:/container",
            "root\\child",
        ] {
            assert!(validate_portable_path(invalid).is_err());
        }
        assert!(parse_identity_bundle_bytes(b"{]").is_err());
        assert!(parse_identity_bundle_bytes(b"{ }").is_err());

        let graph = IdentityBundleGraph::default();
        let mut bundle = IdentityBundleV1 {
            manifest: IdentityBundleManifest {
                format: IDENTITY_BUNDLE_FORMAT.to_string(),
                version: IDENTITY_BUNDLE_VERSION + 1,
                schema_version: MATCH_SCHEMA_VERSION,
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                content_sha256: bundle_content_sha256(&graph).unwrap(),
            },
            graph,
        };
        assert!(validate_bundle(&bundle).unwrap_err().contains("version"));
        bundle.manifest.version = IDENTITY_BUNDLE_VERSION;
        bundle.manifest.schema_version = MATCH_SCHEMA_VERSION + 1;
        assert!(validate_bundle(&bundle).unwrap_err().contains("schema"));

        let root = TestRoot::new("symlink");
        let input = root.0.join("input.json");
        fs::write(&input, b"{}").unwrap();
        let other = root.0.join("other.json");
        fs::write(&other, b"[]").unwrap();
        assert!(!same_open_file_identity(
            &File::open(&input).unwrap(),
            &File::open(&other).unwrap(),
        )
        .unwrap());
        let occupied = root.0.join("occupied.json");
        fs::write(&occupied, b"operator-owned").unwrap();
        assert!(write_new_regular_file(&occupied, b"replacement", 1024).is_err());
        assert_eq!(fs::read(&occupied).unwrap(), b"operator-owned");
        #[cfg(windows)]
        {
            let input_link = root.0.join("input-link.json");
            if std::os::windows::fs::symlink_file(&input, &input_link).is_ok() {
                assert!(read_bounded_regular_file(&input_link, 1024, "test input").is_err());
                let output_link = root.0.join("output-link.xmp");
                std::os::windows::fs::symlink_file(&input, &output_link).unwrap();
                assert!(validate_xmp_sidecar_path(&output_link, false).is_err());
            }
            let real_dir = root.0.join("real-dir");
            fs::create_dir_all(&real_dir).unwrap();
            let dir_link = root.0.join("dir-link");
            if std::os::windows::fs::symlink_dir(&real_dir, &dir_link).is_ok() {
                assert!(validate_relocation_root(&dir_link.to_string_lossy()).is_err());
            }
        }
    }

    #[test]
    fn publication_rejects_same_inode_byte_mutation_before_linking() {
        let root = TestRoot::new("publication-byte-mutation");
        let output = root.0.join("mutated-publication.json");
        MUTATE_NEXT_STAGED_PUBLICATION.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = match write_new_regular_file(&output, br#"{"verified":true}"#, 1024) {
            Err(error) => error,
            Ok(_) => panic!("mutated staged output was accepted"),
        };
        assert!(error.contains("content differs from the staged payload"));
        assert!(!output.exists());
        #[cfg(windows)]
        assert!(
            fs::read_dir(&root.0)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".facial-exchange-")),
            "failed publication must disposition-delete its staging leaf"
        );
    }

    #[test]
    fn recovery_publication_rejects_any_durability_warning() {
        let error = require_durable_recovery_publication(PublicationOutcome {
            warning: Some("directory metadata was not flushed".to_string()),
        })
        .unwrap_err();
        assert!(error.contains("did not prove durable commit"));
        assert!(error.contains("directory metadata was not flushed"));
        require_durable_recovery_publication(PublicationOutcome { warning: None }).unwrap();
    }

    #[test]
    fn recovery_directory_and_marker_transitions_are_durable_and_create_new() {
        let root = TestRoot::new("durable-recovery-directory-transition");
        let recovery = root.0.join("recovery-transition");
        ensure_durable_recovery_directory(&recovery, "test recovery directory").unwrap();
        ensure_durable_recovery_directory(&recovery, "test recovery directory").unwrap();
        let marker = recovery.join("prepared.jsonl");
        publish_durable_recovery_file(&marker, b"prepared\n", 1024).unwrap();
        assert!(publish_durable_recovery_file(&marker, b"replacement\n", 1024).is_err());
        assert!(remove_durable_recovery_file(&marker, "test recovery marker").unwrap());
        assert!(!remove_durable_recovery_file(&marker, "test recovery marker").unwrap());
        assert!(!marker.exists());
    }

    #[test]
    fn recovery_directory_retry_resyncs_existing_owning_parent() {
        let root = TestRoot::new("durable-recovery-directory-retry");
        let recovery = root.0.join("recovery-retry");
        FAIL_NEXT_RECOVERY_DIRECTORY_PARENT_SYNC.store(true, std::sync::atomic::Ordering::SeqCst);
        let error =
            ensure_durable_recovery_directory(&recovery, "retry recovery directory").unwrap_err();
        assert!(error.contains("directory-sync failure"), "{error}");
        assert!(recovery.is_dir());
        ensure_durable_recovery_directory(&recovery, "retry recovery directory").unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_post_rename_flush_failure_is_a_publication_failure() {
        let root = TestRoot::new("windows-post-rename-flush");
        let output = root.0.join("flush-failure.json");
        FAIL_NEXT_WINDOWS_POST_RENAME_FLUSH.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = write_new_regular_file(&output, br#"{"verified":true}"#, 1024).unwrap_err();
        assert!(error.contains("post-rename durability failure"));
        assert!(error.contains("unverified output may remain"));
        assert_eq!(fs::read(&output).unwrap(), br#"{"verified":true}"#);
    }

    #[test]
    fn xmp_rectangles_match_manual_correction_bounds() {
        let valid = MwgXmpRegion {
            face_id: "bounds-face".to_string(),
            person_id: "bounds-person".to_string(),
            name: "Bounds".to_string(),
            bounds_normalized: [0.25, 0.25, 0.5, 0.5],
        };
        let xml = String::from_utf8(render_mwg_xmp(&[valid.clone()], None).unwrap()).unwrap();
        for (field, value) in [
            ("w", "0.0"),
            ("h", "0.0"),
            ("x", "0.7500001"),
            ("y", "0.7500001"),
        ] {
            let hostile = xml.replace(
                &format!("stArea:{field}=\"0.5\""),
                &format!("stArea:{field}=\"{value}\""),
            );
            assert_ne!(hostile, xml);
            assert!(
                parse_mwg_xmp(hostile.as_bytes()).is_err(),
                "{field}={value}"
            );
        }
        for bounds in [
            [0.25, 0.25, 0.0, 0.5],
            [0.25, 0.25, 0.5, 0.0],
            [0.5, 0.0, 0.5 + f32::EPSILON, 0.5],
            [0.0, 0.5, 0.5, 0.5 + f32::EPSILON],
        ] {
            let mut invalid = valid.clone();
            invalid.bounds_normalized = bounds;
            assert!(render_mwg_xmp(&[invalid], None).is_err());
        }
        let mut edge = valid;
        edge.bounds_normalized = [0.5, 0.5, 0.5, 0.5];
        let decoded = parse_mwg_xmp(&render_mwg_xmp(&[edge], None).unwrap()).unwrap();
        assert_eq!(decoded.regions[0].bounds_normalized, [0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn xmp_reports_unprojected_nested_and_wrapper_properties() {
        let region = MwgXmpRegion {
            face_id: "nested-face".to_string(),
            person_id: "nested-person".to_string(),
            name: "Nested".to_string(),
            bounds_normalized: [0.25, 0.25, 0.5, 0.5],
        };
        let xml = String::from_utf8(render_mwg_xmp(&[region.clone()], Some([100, 100])).unwrap())
            .unwrap();
        let wrapped = xml
            .replace(
                XMP_ITEM_PREFIX,
                &format!("{XMP_ITEM_PREFIX}<rdf:Description>"),
            )
            .replace(
                XMP_ITEM_SUFFIX,
                &format!("</rdf:Description>{XMP_ITEM_SUFFIX}"),
            );
        assert!(parse_mwg_xmp(wrapped.as_bytes())
            .unwrap()
            .unsupported
            .is_empty());
        for (fixture, code) in [
            (
                xml.replace("<mwg-rs:Area ", "<mwg-rs:Area facial:Rotation=\"90\" "),
                "xmp_area_property_not_projected",
            ),
            (
                xml.replace("stArea:unit=\"normalized\"/>", "stArea:unit=\"normalized\"><facial:Rotation>90</facial:Rotation></mwg-rs:Area>"),
                "xmp_area_property_not_projected",
            ),
            (
                wrapped.replace(XMP_ITEM_PREFIX, "<rdf:li rdf:parseType=\"Resource\" facial:Extra=\"1\">"),
                "xmp_region_wrapper_property_not_projected",
            ),
            (
                wrapped.replace(XMP_ITEM_SUFFIX, "<facial:Extra>1</facial:Extra></rdf:li>"),
                "xmp_region_wrapper_property_not_projected",
            ),
            (
                xml.replace("<mwg-rs:AppliedToDimensions ", "<mwg-rs:AppliedToDimensions facial:Extra=\"1\" "),
                "xmp_structure_property_not_projected",
            ),
            (
                xml.replace("<mwg-rs:RegionList>", "<facial:Extra>1</facial:Extra><mwg-rs:RegionList>"),
                "xmp_structure_property_not_projected",
            ),
        ] {
            let parsed = parse_mwg_xmp(fixture.as_bytes()).unwrap();
            assert_eq!(parsed.regions, vec![region.clone()]);
            assert!(parsed.unsupported.iter().any(|item| item.code == code), "{code}");
        }
    }

    #[test]
    fn xmp_subset_interoperates_by_namespace_and_rejects_active_xml() {
        let region = MwgXmpRegion {
            face_id: "face-xmp".to_string(),
            person_id: "person-xmp".to_string(),
            name: "A & B".to_string(),
            bounds_normalized: [0.1, 0.2, 0.3, 0.4],
        };
        let bytes = render_mwg_xmp(std::slice::from_ref(&region), Some([100, 100])).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(
            "<mwg-rs:AppliedToDimensions stDim:w=\"100\" stDim:h=\"100\" stDim:unit=\"pixel\"/>"
        ));
        let parsed = parse_mwg_xmp(&bytes).unwrap();
        assert!(parsed.unsupported.is_empty());
        assert_eq!(parsed.regions[0].face_id, region.face_id);
        assert_eq!(parsed.regions[0].person_id, region.person_id);
        assert_eq!(parsed.regions[0].name, region.name);
        for (observed, expected) in parsed.regions[0]
            .bounds_normalized
            .iter()
            .zip(region.bounds_normalized)
        {
            assert!((*observed - expected).abs() <= f32::EPSILON);
        }
        let preview_json = serde_json::to_value(MwgXmpSidecarPreview {
            preview_token: "a".repeat(64),
            media_key: "media/xmp.jpg".to_string(),
            sidecar_path: PathBuf::from("media/xmp.xmp"),
            content_sha256: sha256_bytes(&bytes),
            sidecar_bytes: bytes.len(),
            regions: parsed.regions,
            unsupported: Vec::new(),
            canonical_xml: bytes.clone(),
        })
        .unwrap();
        assert!(preview_json.get("canonical_xml").is_none());
        assert_eq!(
            preview_json["sidecar_bytes"].as_u64(),
            Some(bytes.len() as u64)
        );
        assert_eq!(preview_json["regions"].as_array().unwrap().len(), 1);
        assert_eq!(
            preview_json["content_sha256"].as_str(),
            Some(sha256_bytes(&bytes).as_str())
        );
        let active = format!(
            "<!DOCTYPE x [<!ENTITY e SYSTEM \"file:///x\">]>{}",
            String::from_utf8(bytes.clone()).unwrap()
        );
        assert!(parse_mwg_xmp(active.as_bytes()).is_err());
        let pixel = String::from_utf8(bytes)
            .unwrap()
            .replace("stArea:unit=\"normalized\"", "stArea:unit=\"pixel\"");
        let parsed_pixel = parse_mwg_xmp(pixel.as_bytes()).unwrap();
        assert!(parsed_pixel.regions.is_empty());
        assert_eq!(
            parsed_pixel.unsupported[0].code,
            "xmp_face_region_unit_not_normalized"
        );

        let ordinary = r#"<?xml version="1.0" encoding="UTF-8"?>
<?xpacket begin="x"?>
<meta xmlns="adobe:ns:meta/" xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:m="http://www.metadataworkinggroup.com/schemas/regions/" xmlns:a="http://ns.adobe.com/xmp/sType/Area#" xmlns:d="http://ns.adobe.com/xap/1.0/sType/Dimensions#">
  <r:RDF><r:Description><m:Regions r:parseType="Resource"><m:AppliedToDimensions d:w="4032.0" d:h="3024.000000" d:unit="pixel"/><m:RegionList><r:Seq>
    <r:li r:parseType="Resource"><r:Description><m:Name>External Name</m:Name><m:Type>Face</m:Type><m:Area a:unit="normalized" a:h="0.4" a:w="0.3" a:y="0.4" a:x="0.25"/></r:Description></r:li>
  </r:Seq></m:RegionList></m:Regions></r:Description></r:RDF>
</meta>
<?xpacket end="w"?>"#;
        let first = parse_mwg_xmp(ordinary.as_bytes()).unwrap();
        let second = parse_mwg_xmp(ordinary.as_bytes()).unwrap();
        assert!(first.unsupported.is_empty());
        assert_eq!(first.regions, second.regions);
        assert_eq!(first.regions.len(), 1);
        assert_eq!(first.regions[0].name, "External Name");
        assert!(first.regions[0].face_id.starts_with("xmp-face-"));
        assert!(first.regions[0].person_id.starts_with("xmp-person-"));

        let ambiguous = ordinary.replace(
            "<r:li r:parseType=\"Resource\"><r:Description>",
            "<r:li r:parseType=\"Resource\"><r:Description m:Name=\"Conflicting Attribute\">",
        );
        let ambiguous_error = match parse_mwg_xmp(ambiguous.as_bytes()) {
            Err(error) => error,
            Ok(_) => panic!("ambiguous XMP scalar representation was accepted"),
        };
        assert!(ambiguous_error.contains("ambiguous child and attribute representations"));

        for complex in [
            ordinary.replace(
                "<m:Type>Face</m:Type>",
                "<m:Type>Face<a:semantic/></m:Type>",
            ),
            ordinary.replace(
                "<m:Name>External Name</m:Name>",
                "<m:Name r:resource=\"urn:conflict\">External Name</m:Name>",
            ),
        ] {
            let error = match parse_mwg_xmp(complex.as_bytes()) {
                Err(error) => error,
                Ok(_) => panic!("complex XMP scalar representation was accepted"),
            };
            assert!(error.contains("complex non-scalar representation"));
        }

        let unsupported_attribute = ordinary.replace(
            "<r:li r:parseType=\"Resource\"><r:Description>",
            "<r:li r:parseType=\"Resource\"><r:Description a:unprojected=\"1\">",
        );
        let unsupported_attribute = parse_mwg_xmp(unsupported_attribute.as_bytes()).unwrap();
        assert_eq!(unsupported_attribute.regions.len(), 1);
        assert!(unsupported_attribute
            .unsupported
            .iter()
            .any(|item| item.code == "xmp_region_property_not_projected"));
        assert!(enforce_limit(
            "XMP sidecar bytes",
            IDENTITY_XMP_MAX_SIDECAR_BYTES + 1,
            IDENTITY_XMP_MAX_SIDECAR_BYTES,
        )
        .is_err());
    }
}
