//! Typed, state-bound Match analysis recovery (WP-085).
//!
//! Recovery deliberately distinguishes regenerable analysis from durable
//! operator truth. The preview is a deterministic manifest over every Match
//! table; apply recomputes that manifest under the write lock and rejects any
//! drift before opening its single database transaction.

use super::exchange::{
    ensure_durable_recovery_directory, preflight_identity_bundle_json,
    project_match_operation_for_portable, publish_durable_recovery_file,
    read_guarded_recovery_file, remove_durable_recovery_file, validate_match_operation,
    IdentityBundleFileSnapshot,
};
use super::*;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

const RECOVERY_MANIFEST_PAGE: usize = 128;
const RECOVERY_CONTRACT_VERSION: u32 = 2;
const RECOVERY_MAX_ROW_BYTES: usize = 256 * 1024;
const RECOVERY_FACE_STAGE_PAGE: usize = 256;
const CLEAR_ROLLBACK_JOURNAL_VERSION: u32 = 2;
const CLEAR_ROLLBACK_JOURNAL_MAX_BYTES: usize = 512 * 1024 * 1024;
const CLEAR_ROLLBACK_PREPARED_FILE: &str = "clear-rollback-v1.prepared.jsonl";
const CLEAR_ROLLBACK_COMMITTED_FILE: &str = "clear-rollback-v1.committed.jsonl";

#[cfg(test)]
static UNLINK_RETAINED_RECOVERY_AFTER_CLEAR_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_POST_CLEAR_FINALIZATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static LEAVE_PREPARED_CLEAR_AFTER_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_CLEAR_COMMITTED_PUBLICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_CLEAR_ROLLBACK_RESET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FAIL_NEXT_CLEAR_ROLLBACK_AFTER_FIRST_PAGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, windows))]
static FAIL_NEXT_PREPARED_CLEAR_REPUBLICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, windows))]
static FAIL_NEXT_COMMITTED_CLEAR_REMOVAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MatchStateClass {
    Regenerable,
    DurableOperatorTruth,
    MixedDurableAndRegenerable,
    Configuration,
    SecurityHistory,
    RuntimeControl,
    SchemaAuthority,
    Absent,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatePartition {
    All,
    OperatorConfirmedAssignments,
    StrictAutomaticAssignments,
    DurableFaceReferenceClosure,
    UnreferencedDerivedFaces,
    MachineClustersAbsent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchTableManifest {
    pub table: String,
    pub stable_id_field: String,
    pub state_class: MatchStateClass,
    pub partition: MatchStatePartition,
    pub selected_for_rebuild: bool,
    pub row_count: usize,
    pub stable_id_digest: String,
    pub content_digest: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchRebuildAffectedCounts {
    pub derived_faces: usize,
    pub embeddings: usize,
    pub trusted_search_embeddings: usize,
    pub trusted_index_builds: usize,
    pub suggestions: usize,
    pub projections: usize,
    pub strict_automatic_assignments: usize,
    pub machine_clusters: usize,
    pub total_rows: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchRebuildPreview {
    pub contract_version: u32,
    pub preview_id: String,
    pub state_digest: String,
    pub schema_version: u64,
    pub schema_generation: String,
    pub execution_revision: u64,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub manifests: Vec<MatchTableManifest>,
    pub affected: MatchRebuildAffectedCounts,
    pub raw_media_deleted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchRebuildReceipt {
    pub operation_id: String,
    pub kind: String,
    pub preview_id: String,
    pub pre_state_digest: String,
    pub changed_rows: usize,
    pub removed: MatchRebuildAffectedCounts,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub raw_media_deleted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchClearDisposition {
    DeleteRows,
    ReinitializeExecution,
    RetainSchemaAuthority,
    Absent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchClearTableDisposition {
    pub manifest: MatchTableManifest,
    pub disposition: MatchClearDisposition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRecoveryBundle {
    pub canonical_path: PathBuf,
    pub format: String,
    pub version: u32,
    pub schema_version: u64,
    pub schema_generation: String,
    pub content_sha256: String,
    pub file_sha256: String,
    pub canonical_bytes: usize,
    pub entity_count: usize,
    pub reference_count: usize,
    pub portable_state_sha256: String,
    pub table_counts: BTreeMap<String, usize>,
    pub restore_token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchClearPreview {
    pub contract_version: u32,
    pub confirmation_token: String,
    pub state_digest: String,
    pub schema_version: u64,
    pub schema_generation: String,
    pub execution_revision: u64,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub tables: Vec<MatchClearTableDisposition>,
    pub total_rows_to_delete: usize,
    pub recovery_bundle: VerifiedRecoveryBundle,
    pub raw_media_deleted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchClearReceipt {
    pub operation_id: String,
    pub kind: String,
    pub confirmation_token: String,
    pub pre_state_digest: String,
    pub deleted_rows: usize,
    pub tables: Vec<MatchClearTableDisposition>,
    pub recovery_bundle: VerifiedRecoveryBundle,
    pub execution_revision: u64,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub raw_media_deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_warning: Option<String>,
}

#[derive(Clone)]
struct ManifestRows {
    table: &'static str,
    stable_id_field: &'static str,
    rows: Vec<(String, Value)>,
}

struct RebuildInventory {
    preview: MatchRebuildPreview,
    closure_run_id: String,
}

#[derive(Default)]
struct RecoveryScanBudget {
    scanned_rows: usize,
    scanned_bytes: usize,
}

impl RecoveryScanBudget {
    fn observe_row(&mut self, table: &str, row_bytes: usize) -> Result<(), String> {
        self.scanned_rows = self
            .scanned_rows
            .checked_add(1)
            .ok_or("Match recovery scan row count overflow")?;
        self.scanned_bytes = self
            .scanned_bytes
            .checked_add(row_bytes)
            .ok_or("Match recovery scan byte count overflow")?;
        if row_bytes > RECOVERY_MAX_ROW_BYTES {
            return Err(format!(
                "Match recovery row in {table} exceeds bounded per-row ceiling: {row_bytes}/{RECOVERY_MAX_ROW_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

struct ManifestAccumulator {
    table: &'static str,
    stable_id_field: &'static str,
    state_class: MatchStateClass,
    partition: MatchStatePartition,
    selected_for_rebuild: bool,
    row_count: usize,
    stable_id_hasher: Sha256,
    content_hasher: Sha256,
}

impl ManifestAccumulator {
    fn new(
        table: &'static str,
        stable_id_field: &'static str,
        state_class: MatchStateClass,
        partition: MatchStatePartition,
        selected_for_rebuild: bool,
    ) -> Self {
        let mut stable_id_hasher = Sha256::new();
        stable_id_hasher.update(b"facial-match-recovery-stable-ids-v2\0");
        let mut content_hasher = Sha256::new();
        content_hasher.update(b"facial-match-recovery-content-v2\0");
        Self {
            table,
            stable_id_field,
            state_class,
            partition,
            selected_for_rebuild,
            row_count: 0,
            stable_id_hasher,
            content_hasher,
        }
    }

    fn push(&mut self, stable_id: &str, row: &Value) -> Result<(), String> {
        let canonical = serde_json::to_vec(&canonicalize_json(row.clone()))
            .map_err(|error| format!("serialize Match recovery manifest row: {error}"))?;
        update_framed_digest(&mut self.stable_id_hasher, stable_id.as_bytes())?;
        update_framed_digest(&mut self.content_hasher, stable_id.as_bytes())?;
        update_framed_digest(&mut self.content_hasher, &canonical)?;
        self.row_count = self
            .row_count
            .checked_add(1)
            .ok_or("Match recovery manifest row count overflow")?;
        Ok(())
    }

    fn finish(self) -> MatchTableManifest {
        MatchTableManifest {
            table: self.table.to_string(),
            stable_id_field: self.stable_id_field.to_string(),
            state_class: self.state_class,
            partition: self.partition,
            selected_for_rebuild: self.selected_for_rebuild,
            row_count: self.row_count,
            stable_id_digest: format!("{:x}", self.stable_id_hasher.finalize()),
            content_digest: format!("{:x}", self.content_hasher.finalize()),
        }
    }
}

fn update_framed_digest(hasher: &mut Sha256, bytes: &[u8]) -> Result<(), String> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| "Match recovery digest frame length overflow".to_string())?;
    hasher.update(length.to_be_bytes());
    hasher.update(bytes);
    Ok(())
}

struct ClearInventory {
    state_digest: String,
    execution: PersistedExecutionState,
    tables: Vec<MatchClearTableDisposition>,
    total_rows_to_delete: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClearRollbackJournalHeader {
    version: u32,
    state_digest: String,
    execution: PersistedExecutionState,
    tables: Vec<MatchClearTableDisposition>,
    total_rows_to_delete: usize,
    total_rows_to_restore: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClearRollbackJournalRow {
    table: String,
    stable_id: String,
    value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClearRollbackJournalTrailer {
    row_count: usize,
    content_sha256: String,
}

struct ClearRollbackJournalPublication {
    prepared_path: PathBuf,
    committed_path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "record", content = "payload", rename_all = "snake_case")]
enum ClearRollbackJournalRecord {
    Header(ClearRollbackJournalHeader),
    Row(ClearRollbackJournalRow),
    Trailer(ClearRollbackJournalTrailer),
}

impl MatchStore {
    fn private_recovery_path_unlocked(&self, file_sha256: &str) -> Result<PathBuf, String> {
        validate_sha256("private Match recovery file SHA-256", file_sha256)?;
        let media_state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no media-state parent")?;
        Ok(media_state_root
            .join("recovery")
            .join(format!("clear-{file_sha256}.json")))
    }

    fn verify_private_recovery_lineage(
        source: &IdentityBundleFileSnapshot,
        retained: IdentityBundleFileSnapshot,
    ) -> Result<IdentityBundleFileSnapshot, String> {
        Self::revalidate_identity_bundle_snapshot_path(source)?;
        Self::revalidate_identity_bundle_snapshot_path(&retained)?;
        if retained.bytes != source.bytes
            || retained.file_sha256 != source.file_sha256
            || retained.canonical_bytes != source.canonical_bytes
            || retained.bundle != source.bundle
        {
            return Err("private Match recovery copy contradicts the verified export".to_string());
        }
        Ok(retained)
    }

    fn private_recovery_snapshot_unlocked(
        &self,
        source: &IdentityBundleFileSnapshot,
    ) -> Result<IdentityBundleFileSnapshot, String> {
        let media_state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no media-state parent")?;
        let recovery_root = media_state_root.join("recovery");
        ensure_durable_recovery_directory(&recovery_root, "private Match recovery root")?;
        let recovery_path = self.private_recovery_path_unlocked(&source.file_sha256)?;
        match std::fs::symlink_metadata(&recovery_path) {
            Ok(_) => {
                let retained = Self::read_identity_bundle_snapshot(&recovery_path)?;
                return Self::verify_private_recovery_lineage(source, retained);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect private Match recovery copy {}: {error}",
                    recovery_path.display()
                ));
            }
        }
        // A failed publication is never converted into success merely because
        // its final pathname appeared. Only an artifact proven to predate this
        // attempt may take the independent lineage-verification path above.
        Self::publish_identity_bundle_snapshot_copy(source, &recovery_path)?;
        let retained = Self::read_identity_bundle_snapshot(&recovery_path)?;
        Self::verify_private_recovery_lineage(source, retained)
    }

    fn repair_private_recovery_snapshot_unlocked(
        &self,
        source: &IdentityBundleFileSnapshot,
    ) -> Result<IdentityBundleFileSnapshot, String> {
        if let Ok(snapshot) = self.private_recovery_snapshot_unlocked(source) {
            return Ok(snapshot);
        }
        let media_state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no media-state parent")?;
        let recovery_root = media_state_root.join("recovery");
        let fallback = recovery_root.join(format!(
            "clear-{}-{}.json",
            source.file_sha256,
            uuid::Uuid::new_v4().simple()
        ));
        Self::publish_identity_bundle_snapshot_copy(source, &fallback)?;
        let retained = Self::read_identity_bundle_snapshot(&fallback)?;
        Self::verify_private_recovery_lineage(source, retained)
    }

    fn ensure_post_clear_recovery_snapshot_unlocked(
        &self,
        recovery_snapshot: &IdentityBundleFileSnapshot,
    ) -> Result<IdentityBundleFileSnapshot, String> {
        #[cfg(test)]
        {
            let marker = self
                .store
                .database_root()
                .parent()
                .ok_or("Match database root has no media-state parent")?
                .join("recovery")
                .join("force-post-clear-recovery-proof-failure");
            if marker.exists() {
                return Err("injected post-clear recovery proof failure".to_string());
            }
        }
        if Self::revalidate_identity_bundle_snapshot_path(recovery_snapshot).is_ok() {
            return recovery_snapshot.try_clone_guarded();
        }
        let repaired = self.repair_private_recovery_snapshot_unlocked(recovery_snapshot)?;
        Self::revalidate_identity_bundle_snapshot_path(&repaired)?;
        Ok(repaired)
    }

    fn scan_recovery_table_unlocked<F>(
        &self,
        table: &'static str,
        stable_id_field: &'static str,
        budget: &mut RecoveryScanBudget,
        mut visit: F,
    ) -> Result<(), String>
    where
        F: FnMut(&str, &Value, &mut RecoveryScanBudget) -> Result<(), String>,
    {
        let mut after_id = String::new();
        loop {
            let db = self.store.db();
            let sql = format!(
                "SELECT * OMIT id FROM {table} WHERE {stable_id_field} > $after_id ORDER BY {stable_id_field} ASC LIMIT {RECOVERY_MANIFEST_PAGE};"
            );
            let page_after = after_id.clone();
            let page: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(sql)
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("read Match recovery manifest page: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match recovery manifest page: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for row in &page {
                let stable_id = row
                    .get(stable_id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!("{table} recovery row lacks stable {stable_id_field}")
                    })?;
                if stable_id <= after_id.as_str() {
                    return Err(format!("{table} recovery manifest page did not advance"));
                }
                let row_bytes = serde_json::to_vec(row)
                    .map_err(|error| format!("serialize Match recovery work row: {error}"))?
                    .len();
                budget.observe_row(table, row_bytes)?;
                visit(stable_id, row, budget)?;
                after_id.clear();
                after_id.push_str(stable_id);
            }
            if page_len < RECOVERY_MANIFEST_PAGE {
                break;
            }
        }
        Ok(())
    }

    fn recovery_manifest_unlocked(
        &self,
        table: &'static str,
        stable_id_field: &'static str,
        state_class: MatchStateClass,
        partition: MatchStatePartition,
        selected_for_rebuild: bool,
        budget: &mut RecoveryScanBudget,
    ) -> Result<MatchTableManifest, String> {
        let mut manifest = ManifestAccumulator::new(
            table,
            stable_id_field,
            state_class,
            partition,
            selected_for_rebuild,
        );
        self.scan_recovery_table_unlocked(table, stable_id_field, budget, |id, row, _| {
            manifest.push(id, row)
        })?;
        Ok(manifest.finish())
    }

    fn clear_all_recovery_face_closure_unlocked(&self) -> Result<(), String> {
        let db = self.store.db();
        surreal_store::run(async move {
            db.query("DELETE match_recovery_face_closure;")
                .await
                .map_err(|error| format!("clear stale Match recovery Face closure: {error}"))?
                .check()
                .map_err(|error| format!("clear stale Match recovery Face closure: {error}"))?;
            Ok::<(), String>(())
        })
    }

    fn clear_recovery_face_closure_run_unlocked(&self, run_id: &str) -> Result<(), String> {
        let db = self.store.db();
        let run_id = run_id.to_string();
        surreal_store::run(async move {
            db.query("DELETE match_recovery_face_closure WHERE run_id = $run_id;")
                .bind(("run_id", run_id))
                .await
                .map_err(|error| format!("clear Match recovery Face closure run: {error}"))?
                .check()
                .map_err(|error| format!("clear Match recovery Face closure run: {error}"))?;
            Ok::<(), String>(())
        })
    }

    fn stage_recovery_face_ids_unlocked(
        &self,
        run_id: &str,
        face_ids: &mut Vec<String>,
    ) -> Result<(), String> {
        if face_ids.is_empty() {
            return Ok(());
        }
        face_ids.sort();
        face_ids.dedup();
        let mut owned = Vec::with_capacity(face_ids.len());
        for face_id in face_ids.drain(..) {
            let closure_id = stable_pair_id("recovery-face", run_id, &face_id);
            owned.push((
                "match_recovery_face_closure".to_string(),
                closure_id.clone(),
                serde_json::json!({
                    "closure_id": closure_id,
                    "run_id": run_id,
                    "face_id": face_id,
                }),
            ));
        }
        let borrowed = owned
            .iter()
            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
            .collect::<Vec<_>>();
        self.transactional_upserts_deletes_unlocked(&borrowed, &[])
    }

    fn scan_rebuild_face_partitions_unlocked(
        &self,
        run_id: &str,
        budget: &mut RecoveryScanBudget,
        durable_faces: &mut ManifestAccumulator,
        derived_faces: &mut ManifestAccumulator,
    ) -> Result<(), String> {
        let mut after_id = String::new();
        loop {
            let db = self.store.db();
            let page_after = after_id.clone();
            let page: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT * OMIT id FROM match_face_observation WHERE face_id > $after_id ORDER BY face_id ASC LIMIT 128;",
                    )
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("read Match rebuild Face page: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match rebuild Face page: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            let face_ids = page
                .iter()
                .map(|row| {
                    row.get("face_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or("Match rebuild Face row lacks FaceId".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let db = self.store.db();
            let bound_run_id = run_id.to_string();
            let bound_face_ids = face_ids.clone();
            let staged: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(
                        "SELECT face_id FROM match_recovery_face_closure WITH INDEX match_recovery_face_closure_run_face WHERE run_id = $run_id AND face_id IN $face_ids LIMIT 128;",
                    )
                    .bind(("run_id", bound_run_id))
                    .bind(("face_ids", bound_face_ids))
                    .await
                    .map_err(|error| format!("read Match rebuild Face closure page: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match rebuild Face closure page: {error}"))
            })?;
            let staged = staged
                .iter()
                .filter_map(|row| row.get("face_id").and_then(Value::as_str))
                .collect::<BTreeSet<_>>();
            for (row, face_id) in page.iter().zip(face_ids.iter()) {
                if face_id <= &after_id {
                    return Err("Match rebuild Face page did not advance".to_string());
                }
                let row_bytes = serde_json::to_vec(row)
                    .map_err(|error| format!("serialize Match rebuild Face row: {error}"))?
                    .len();
                budget.observe_row(FACE_TABLE, row_bytes)?;
                if staged.contains(face_id.as_str()) {
                    durable_faces.push(face_id, row)?;
                } else if row.get("operator_owned").and_then(Value::as_bool) != Some(true) {
                    derived_faces.push(face_id, row)?;
                } else {
                    return Err(format!(
                        "operator-owned Match face {face_id} escaped durable closure"
                    ));
                }
                after_id.clear();
                after_id.push_str(face_id);
            }
            if page.len() < RECOVERY_MANIFEST_PAGE {
                break;
            }
        }
        Ok(())
    }

    fn commit_rebuild_transaction_unlocked(
        &self,
        closure_run_id: &str,
        execution: &PersistedExecutionState,
    ) -> Result<(), String> {
        let inject_failure = {
            #[cfg(test)]
            {
                self.store
                    .database_root()
                    .parent()
                    .ok_or("Match database root has no media-state parent")?
                    .join("recovery")
                    .join("force-rebuild-transaction-failure")
                    .exists()
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        let injected_statement = if inject_failure {
            "THROW 'injected Match rebuild transaction failure';"
        } else {
            ""
        };
        let sql = format!(
            "BEGIN TRANSACTION;
             DELETE match_assignment WHERE state = $strict_state;
             DELETE match_face_observation WHERE operator_owned != true AND face_id NOT IN (SELECT VALUE face_id FROM match_recovery_face_closure WHERE run_id = $closure_run_id);
             DELETE match_face_embedding;
             DELETE match_trusted_search_embedding;
             DELETE match_trusted_index_build;
             DELETE match_suggestion;
             DELETE match_people_projection;
             {injected_statement}
             UPSERT match_execution:global CONTENT $execution;
             DELETE match_recovery_face_closure WHERE run_id = $closure_run_id;
             COMMIT TRANSACTION;"
        );
        let execution = execution.clone();
        let closure_run_id = closure_run_id.to_string();
        let db = self.store.db();
        surreal_store::run(async move {
            db.query(sql)
                .bind((
                    "strict_state",
                    AssignmentState::CommittedStrictAutomatic.as_str(),
                ))
                .bind(("closure_run_id", closure_run_id))
                .bind(("execution", execution))
                .await
                .map_err(|error| format!("rebuild Match analysis transaction: {error}"))?
                .check()
                .map_err(|error| format!("rebuild Match analysis transaction: {error}"))?;
            Ok::<(), String>(())
        })
    }

    pub fn preview_rebuild_match_analysis(&self) -> Result<MatchRebuildPreview, String> {
        let _guard = self.mutation_write_guard("Match rebuild preview")?;
        let inventory = self.rebuild_inventory_unlocked()?;
        let closure_run_id = inventory.closure_run_id.clone();
        let preview = inventory.preview;
        self.clear_recovery_face_closure_run_unlocked(&closure_run_id)?;
        Ok(preview)
    }

    pub fn rebuild_match_analysis(
        &self,
        preview: &MatchRebuildPreview,
    ) -> Result<MatchRebuildReceipt, String> {
        let _guard = self.mutation_write_guard("Match analysis rebuild")?;
        let current = self.rebuild_inventory_unlocked()?;
        if current.preview != *preview {
            self.clear_recovery_face_closure_run_unlocked(&current.closure_run_id)?;
            return Err("stale or mismatched rebuild_match_analysis preview".to_string());
        }

        let mut execution = self.execution_state_unlocked()?;
        execution.revision = execution
            .revision
            .checked_add(1)
            .ok_or("Match execution revision overflow")?;
        execution.identity_revision = execution
            .identity_revision
            .checked_add(1)
            .ok_or("Match identity revision overflow")?;
        execution.updated_at = now();

        if let Err(error) =
            self.commit_rebuild_transaction_unlocked(&current.closure_run_id, &execution)
        {
            let _ = self.clear_recovery_face_closure_run_unlocked(&current.closure_run_id);
            return Err(error);
        }
        let removed = preview.affected.clone();

        let mut caches = self.cache_write_recover();
        caches.identity_revision = execution.identity_revision;
        caches.catalog_revision = execution.catalog_revision;
        caches.projections.clear();
        drop(caches);

        Ok(MatchRebuildReceipt {
            operation_id: new_id("rebuild-match-analysis"),
            kind: "rebuild_match_analysis".to_string(),
            preview_id: preview.preview_id.clone(),
            pre_state_digest: preview.state_digest.clone(),
            changed_rows: preview.affected.total_rows,
            removed,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            raw_media_deleted: false,
        })
    }

    pub fn preview_clear_all_match_data(
        &self,
        recovery_path: &Path,
    ) -> Result<MatchClearPreview, String> {
        let _guard = self.mutation_write_guard("Match clear preview")?;
        let (bundle, export, reconciliation) =
            self.export_recovery_bundle_snapshot_unlocked(recovery_path)?;
        let exported_snapshot = Self::read_identity_bundle_snapshot(recovery_path)?;
        if exported_snapshot.bundle != bundle {
            return Err(
                "recovery bundle guarded snapshot differs from the locked Match export".to_string(),
            );
        }
        self.private_recovery_snapshot_unlocked(&exported_snapshot)?;
        let verified =
            verified_recovery_bundle(&exported_snapshot, &reconciliation, export.canonical_bytes)?;
        let inventory = self.clear_inventory_unlocked()?;
        let confirmation_token = clear_confirmation_token(&inventory, &verified)?;
        Ok(MatchClearPreview {
            contract_version: RECOVERY_CONTRACT_VERSION,
            confirmation_token,
            state_digest: inventory.state_digest,
            schema_version: MATCH_SCHEMA_VERSION,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            execution_revision: inventory.execution.revision,
            identity_revision: inventory.execution.identity_revision,
            catalog_revision: inventory.execution.catalog_revision,
            tables: inventory.tables,
            total_rows_to_delete: inventory.total_rows_to_delete,
            recovery_bundle: verified,
            raw_media_deleted: false,
        })
    }

    pub fn clear_all_match_data(
        &self,
        preview: &MatchClearPreview,
    ) -> Result<MatchClearReceipt, String> {
        let _guard = self.mutation_write_guard("Match clear")?;

        let snapshot = match std::fs::symlink_metadata(&preview.recovery_bundle.canonical_path) {
            Ok(_) => Self::read_identity_bundle_snapshot(&preview.recovery_bundle.canonical_path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let retained_path =
                    self.private_recovery_path_unlocked(&preview.recovery_bundle.file_sha256)?;
                Self::read_identity_bundle_snapshot(&retained_path)?
            }
            Err(error) => {
                return Err(format!(
                    "inspect identity bundle {}: {error}",
                    preview.recovery_bundle.canonical_path.display()
                ))
            }
        };
        let (current_bundle, reconciliation) = self.current_identity_bundle_snapshot_unlocked()?;
        let verified = verified_recovery_bundle_without_reconciliation(
            &snapshot,
            reconciliation.state_sha256.clone(),
            reconciliation.reference_count,
        )?;
        require_recovery_bundle_matches_current(
            &snapshot,
            &verified,
            &current_bundle,
            &reconciliation,
        )?;
        let mut preview_location_verified = verified.clone();
        preview_location_verified.canonical_path = preview.recovery_bundle.canonical_path.clone();
        if preview_location_verified != preview.recovery_bundle {
            return Err(
                "recovery bundle changed or no longer verifies after clear preview".to_string(),
            );
        }

        let inventory = self.clear_inventory_unlocked()?;
        let confirmation_token = clear_confirmation_token(&inventory, &verified)?;
        if preview.contract_version != RECOVERY_CONTRACT_VERSION
            || preview.state_digest != inventory.state_digest
            || preview.schema_version != MATCH_SCHEMA_VERSION
            || preview.schema_generation != MATCH_SCHEMA_GENERATION
            || preview.execution_revision != inventory.execution.revision
            || preview.identity_revision != inventory.execution.identity_revision
            || preview.catalog_revision != inventory.execution.catalog_revision
            || preview.tables != inventory.tables
            || preview.total_rows_to_delete != inventory.total_rows_to_delete
            || preview.confirmation_token != confirmation_token
            || preview.raw_media_deleted
        {
            return Err("stale or mismatched clear_all_match_data preview".to_string());
        }

        let retained_snapshot = self.private_recovery_snapshot_unlocked(&snapshot)?;
        self.commit_clear_inventory_unlocked(
            inventory,
            verified,
            confirmation_token,
            &retained_snapshot,
        )
    }

    pub fn clear_all_match_data_file(
        &self,
        recovery_path: &Path,
        expected_confirmation_token: &str,
    ) -> Result<MatchClearReceipt, String> {
        let _guard = self.mutation_write_guard("Match clear file")?;
        let supplied_snapshot = Self::read_identity_bundle_snapshot(recovery_path)?;
        let snapshot = self.private_recovery_snapshot_unlocked(&supplied_snapshot)?;
        let (current_bundle, reconciliation) = self.current_identity_bundle_snapshot_unlocked()?;
        let verified = verified_recovery_bundle_without_reconciliation(
            &snapshot,
            reconciliation.state_sha256.clone(),
            reconciliation.reference_count,
        )?;
        require_recovery_bundle_matches_current(
            &snapshot,
            &verified,
            &current_bundle,
            &reconciliation,
        )?;
        let inventory = self.clear_inventory_unlocked()?;
        let observed_token = clear_confirmation_token(&inventory, &verified)?;
        if observed_token != expected_confirmation_token {
            return Err("stale or mismatched clear_all_match_data confirmation token".to_string());
        }
        self.commit_clear_inventory_unlocked(inventory, verified, observed_token, &snapshot)
    }

    pub fn restore_clear_recovery_bundle(
        &self,
        path: &Path,
        relocations: &BTreeMap<String, String>,
        expected_token: &str,
    ) -> Result<IdentityImportReceipt, String> {
        let snapshot = Self::read_identity_bundle_snapshot(path)?;
        let verified = verified_recovery_bundle_without_reconciliation(
            &snapshot,
            String::new(),
            identity_bundle_reference_count(&snapshot.bundle.graph),
        )?;
        if verified.restore_token != expected_token {
            return Err("recovery bundle restore token mismatch".to_string());
        }
        let plan = self.preview_identity_bundle_import_snapshot(
            snapshot.bundle.clone(),
            relocations,
            IdentityImportMode::Replace,
        )?;
        if !plan.unresolved_root_ids.is_empty() {
            let sample = plan
                .unresolved_root_ids
                .iter()
                .take(IDENTITY_IMPORT_RECEIPT_SAMPLE_LIMIT)
                .cloned()
                .collect::<Vec<_>>();
            return Err(format!(
                "recovery bundle has {} unresolved roots; sample: {}",
                plan.unresolved_root_ids.len(),
                sample.join(",")
            ));
        }
        if plan.has_destructive_changes() {
            return Err(
                "recovery bundle restore would overwrite or delete current portable Match rows; recovery restore permits only creates or an exact already-applied graph"
                    .to_string(),
            );
        }
        self.apply_identity_recovery_bundle_import(plan)
    }

    fn clear_rollback_root_unlocked(&self) -> Result<PathBuf, String> {
        let media_state_root = self
            .store
            .database_root()
            .parent()
            .ok_or("Match database root has no media-state parent")?;
        let root = media_state_root.join("recovery");
        ensure_durable_recovery_directory(&root, "Match clear rollback root")?;
        Ok(root)
    }

    fn clear_rollback_paths_unlocked(&self) -> Result<(PathBuf, PathBuf), String> {
        let root = self.clear_rollback_root_unlocked()?;
        Ok((
            root.join(CLEAR_ROLLBACK_PREPARED_FILE),
            root.join(CLEAR_ROLLBACK_COMMITTED_FILE),
        ))
    }

    fn clear_rollback_artifact_exists(path: &Path, label: &str) -> Result<bool, String> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("inspect {label} {}: {error}", path.display())),
        }
    }

    fn invalidate_match_caches_for_pending_clear_unlocked(&self) {
        let mut caches = self.cache_write_recover();
        *caches = MatchCaches::default();
    }

    fn retain_clear_rollback_artifact_unlocked(
        &self,
        path: &Path,
        bytes: &[u8],
        label: &str,
    ) -> Result<(), String> {
        if !Self::clear_rollback_artifact_exists(path, label)? {
            publish_durable_recovery_file(path, bytes, CLEAR_ROLLBACK_JOURNAL_MAX_BYTES)?;
        }
        let observed = read_guarded_recovery_file(path, CLEAR_ROLLBACK_JOURNAL_MAX_BYTES, label)?;
        if observed != bytes {
            return Err(format!(
                "{label} does not retain the exact rollback journal"
            ));
        }
        Ok(())
    }

    fn write_clear_rollback_record(
        journal: &mut Vec<u8>,
        record: &ClearRollbackJournalRecord,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec(record)
            .map_err(|error| format!("serialize Match clear rollback journal: {error}"))?;
        let next_len = journal
            .len()
            .checked_add(encoded.len())
            .and_then(|value| value.checked_add(1))
            .ok_or("Match clear rollback journal byte count overflow")?;
        if next_len > CLEAR_ROLLBACK_JOURNAL_MAX_BYTES {
            return Err(format!(
                "Match clear rollback journal exceeds aggregate byte ceiling: {next_len}/{CLEAR_ROLLBACK_JOURNAL_MAX_BYTES} bytes"
            ));
        }
        journal.extend_from_slice(&encoded);
        journal.push(b'\n');
        Ok(())
    }

    fn clear_rollback_row_value(table: &str, value: &Value) -> Result<Value, String> {
        if table != OPERATION_TABLE {
            return Ok(value.clone());
        }
        let operation = serde_json::from_value::<MatchOperation>(value.clone())
            .map_err(|error| format!("decode Match clear rollback operation: {error}"))?;
        serde_json::to_value(project_match_operation_for_portable(&operation)?)
            .map_err(|error| format!("project Match clear rollback operation: {error}"))
    }

    fn prepare_clear_rollback_journal_unlocked(
        &self,
        inventory: &ClearInventory,
    ) -> Result<ClearRollbackJournalPublication, String> {
        let (prepared_path, committed_path) = self.clear_rollback_paths_unlocked()?;
        if Self::clear_rollback_artifact_exists(&committed_path, "committed Match clear marker")?
            || Self::clear_rollback_artifact_exists(&prepared_path, "prepared Match clear journal")?
        {
            return Err(format!(
                "existing Match clear rollback state requires restart recovery: {} or {}",
                prepared_path.display(),
                committed_path.display()
            ));
        }
        let mut journal = Vec::new();
        let total_rows_to_restore = clear_rollback_expected_rows(inventory)?;
        let header = ClearRollbackJournalHeader {
            version: CLEAR_ROLLBACK_JOURNAL_VERSION,
            state_digest: inventory.state_digest.clone(),
            execution: inventory.execution.clone(),
            tables: inventory.tables.clone(),
            total_rows_to_delete: inventory.total_rows_to_delete,
            total_rows_to_restore,
        };
        Self::write_clear_rollback_record(
            &mut journal,
            &ClearRollbackJournalRecord::Header(header.clone()),
        )?;
        let mut content_hasher = Sha256::new();
        content_hasher.update(b"facial-match-clear-rollback-v1\0");
        let canonical_header = serde_json::to_vec(&canonicalize_json(
            serde_json::to_value(&header).map_err(|error| error.to_string())?,
        ))
        .map_err(|error| error.to_string())?;
        update_framed_digest(&mut content_hasher, &canonical_header)?;
        let mut row_count = 0usize;
        let mut budget = RecoveryScanBudget::default();
        for (table, stable_id_field, _) in clear_rollback_table_specs() {
            self.scan_recovery_table_unlocked(
                table,
                stable_id_field,
                &mut budget,
                |stable_id, value, _| {
                    let rollback_value = Self::clear_rollback_row_value(table, value)?;
                    let row = ClearRollbackJournalRow {
                        table: table.to_string(),
                        stable_id: stable_id.to_string(),
                        value: rollback_value,
                    };
                    let canonical = serde_json::to_vec(&canonicalize_json(
                        serde_json::to_value(&row).map_err(|error| error.to_string())?,
                    ))
                    .map_err(|error| error.to_string())?;
                    update_framed_digest(&mut content_hasher, &canonical)?;
                    row_count = row_count
                        .checked_add(1)
                        .ok_or("Match clear rollback row count overflow")?;
                    Self::write_clear_rollback_record(
                        &mut journal,
                        &ClearRollbackJournalRecord::Row(row),
                    )
                },
            )?;
        }
        if row_count != total_rows_to_restore {
            return Err(
                "Match clear rollback journal restorable row count drifted from the guarded preview"
                    .to_string(),
            );
        }
        let trailer = ClearRollbackJournalTrailer {
            row_count,
            content_sha256: format!("{:x}", content_hasher.finalize()),
        };
        Self::write_clear_rollback_record(
            &mut journal,
            &ClearRollbackJournalRecord::Trailer(trailer),
        )?;
        publish_durable_recovery_file(&prepared_path, &journal, CLEAR_ROLLBACK_JOURNAL_MAX_BYTES)?;
        let observed = read_guarded_recovery_file(
            &prepared_path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "Match clear rollback journal",
        )?;
        if observed != journal {
            return Err(
                "published Match clear rollback journal failed exact byte verification".to_string(),
            );
        }
        Ok(ClearRollbackJournalPublication {
            prepared_path,
            committed_path,
            bytes: journal,
        })
    }

    fn validate_clear_rollback_journal_unlocked(
        &self,
        path: &Path,
    ) -> Result<ClearRollbackJournalHeader, String> {
        Ok(self.read_validated_clear_rollback_journal_unlocked(path)?.0)
    }

    fn read_validated_clear_rollback_journal_unlocked(
        &self,
        path: &Path,
    ) -> Result<(ClearRollbackJournalHeader, Vec<u8>), String> {
        let bytes = read_guarded_recovery_file(
            path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "Match clear rollback journal",
        )?;
        let mut lines = BufReader::new(bytes.as_slice()).lines();
        let first = lines
            .next()
            .ok_or("Match clear rollback journal is empty")?
            .map_err(|error| format!("read Match clear rollback header: {error}"))?;
        let header = match serde_json::from_str::<ClearRollbackJournalRecord>(&first)
            .map_err(|error| format!("decode Match clear rollback header: {error}"))?
        {
            ClearRollbackJournalRecord::Header(header)
                if header.version == CLEAR_ROLLBACK_JOURNAL_VERSION =>
            {
                header
            }
            _ => return Err("Match clear rollback journal has an invalid header".to_string()),
        };
        let allowed_tables = clear_rollback_table_specs()
            .into_iter()
            .map(|(table, _, _)| table)
            .collect::<BTreeSet<_>>();
        let mut content_hasher = Sha256::new();
        content_hasher.update(b"facial-match-clear-rollback-v1\0");
        let canonical_header = serde_json::to_vec(&canonicalize_json(
            serde_json::to_value(&header).map_err(|error| error.to_string())?,
        ))
        .map_err(|error| error.to_string())?;
        update_framed_digest(&mut content_hasher, &canonical_header)?;
        let mut row_count = 0usize;
        let mut trailer = None;
        for line in lines {
            let line = line.map_err(|error| format!("read Match clear rollback row: {error}"))?;
            let record = serde_json::from_str::<ClearRollbackJournalRecord>(&line)
                .map_err(|error| format!("decode Match clear rollback row: {error}"))?;
            match record {
                ClearRollbackJournalRecord::Row(row) if trailer.is_none() => {
                    if !allowed_tables.contains(row.table.as_str()) || row.stable_id.is_empty() {
                        return Err("Match clear rollback journal contains an invalid row owner"
                            .to_string());
                    }
                    let canonical = serde_json::to_vec(&canonicalize_json(
                        serde_json::to_value(&row).map_err(|error| error.to_string())?,
                    ))
                    .map_err(|error| error.to_string())?;
                    update_framed_digest(&mut content_hasher, &canonical)?;
                    row_count = row_count
                        .checked_add(1)
                        .ok_or("Match clear rollback row count overflow")?;
                }
                ClearRollbackJournalRecord::Trailer(observed) if trailer.is_none() => {
                    trailer = Some(observed);
                }
                _ => return Err("Match clear rollback journal record order is invalid".to_string()),
            }
        }
        let trailer = trailer.ok_or("Match clear rollback journal omits its trailer")?;
        let content_sha256 = format!("{:x}", content_hasher.finalize());
        if trailer.row_count != row_count
            || trailer.row_count != header.total_rows_to_restore
            || trailer.content_sha256 != content_sha256
        {
            return Err("Match clear rollback journal failed exact digest validation".to_string());
        }
        Ok((header, bytes))
    }

    fn restore_clear_rollback_journal_unlocked(
        &self,
        path: &Path,
        expected: &ClearInventory,
    ) -> Result<(), String> {
        let (header, journal_bytes) = self.read_validated_clear_rollback_journal_unlocked(path)?;
        if header.state_digest != expected.state_digest
            || header.execution != expected.execution
            || header.tables != expected.tables
            || header.total_rows_to_delete != expected.total_rows_to_delete
            || header.total_rows_to_restore != clear_rollback_expected_rows(expected)?
        {
            return Err("Match clear rollback journal contradicts guarded state".to_string());
        }
        #[cfg(test)]
        if FAIL_NEXT_CLEAR_ROLLBACK_RESET.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("injected Match clear rollback reset failure".to_string());
        }
        let mut delete_sql = String::from("BEGIN TRANSACTION;\n");
        for (table, _, _) in clear_delete_table_specs() {
            delete_sql.push_str(&format!("DELETE {table};\n"));
        }
        delete_sql.push_str("DELETE match_recovery_face_closure;\n");
        delete_sql.push_str("DELETE match_execution;\nCOMMIT TRANSACTION;");
        let db = self.store.db();
        surreal_store::run(async move {
            db.query(delete_sql)
                .await
                .map_err(|error| format!("reset Match state for paged rollback: {error}"))?
                .check()
                .map_err(|error| format!("reset Match state for paged rollback: {error}"))?;
            Ok::<(), String>(())
        })?;

        let mut projected_operation_manifest = ManifestAccumulator::new(
            OPERATION_TABLE,
            "operation_id",
            MatchStateClass::SecurityHistory,
            MatchStatePartition::All,
            true,
        );
        let mut page = Vec::<(String, String, Value)>::with_capacity(RECOVERY_MANIFEST_PAGE);
        for line in BufReader::new(journal_bytes.as_slice()).lines().skip(1) {
            let line = line.map_err(|error| format!("read Match rollback restore row: {error}"))?;
            match serde_json::from_str::<ClearRollbackJournalRecord>(&line)
                .map_err(|error| format!("decode Match rollback restore row: {error}"))?
            {
                ClearRollbackJournalRecord::Row(row) => {
                    if row.table == OPERATION_TABLE {
                        projected_operation_manifest.push(&row.stable_id, &row.value)?;
                    }
                    page.push((row.table, row.stable_id, row.value));
                    if page.len() >= RECOVERY_MANIFEST_PAGE {
                        let borrowed = page
                            .iter()
                            .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
                            .collect::<Vec<_>>();
                        self.transactional_upserts_deletes_unlocked(&borrowed, &[])?;
                        #[cfg(test)]
                        if FAIL_NEXT_CLEAR_ROLLBACK_AFTER_FIRST_PAGE
                            .swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            return Err(
                                "injected Match clear rollback failure after first restored page"
                                    .to_string(),
                            );
                        }
                        page.clear();
                    }
                }
                ClearRollbackJournalRecord::Trailer(_) => break,
                ClearRollbackJournalRecord::Header(_) => {
                    return Err("Match rollback restore found a repeated header".to_string());
                }
            }
        }
        if !page.is_empty() {
            let borrowed = page
                .iter()
                .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
                .collect::<Vec<_>>();
            self.transactional_upserts_deletes_unlocked(&borrowed, &[])?;
        }
        self.transactional_upserts_deletes_unlocked(
            &[(
                EXECUTION_TABLE,
                "global",
                serde_json::to_value(&header.execution).map_err(|error| error.to_string())?,
            )],
            &[],
        )?;
        let projected_operation_manifest = projected_operation_manifest.finish();
        let observed = self.clear_inventory_unlocked()?;
        if !clear_rollback_inventory_matches(&observed, expected, &projected_operation_manifest) {
            return Err(
                "pre-clear paged rollback failed durable manifest reconciliation or retained regenerable vectors"
                    .to_string(),
            );
        }
        let mut caches = self.cache_write_recover();
        *caches = MatchCaches::default();
        caches.identity_revision = header.execution.identity_revision;
        caches.catalog_revision = header.execution.catalog_revision;
        Ok(())
    }

    pub(super) fn recover_interrupted_clear(&self) -> Result<(), String> {
        let _guard = self
            .store
            .transaction_lock()
            .write()
            .map_err(|_| "Match interrupted-clear recovery lock is poisoned".to_string())?;
        self.recover_interrupted_clear_unlocked()?;
        self.clear_all_recovery_face_closure_unlocked()
    }

    pub(super) fn recover_interrupted_clear_unlocked(&self) -> Result<(), String> {
        let (prepared_path, committed_path) = self.clear_rollback_paths_unlocked()?;
        let committed_exists =
            Self::clear_rollback_artifact_exists(&committed_path, "committed Match clear marker")?;
        let prepared_exists =
            Self::clear_rollback_artifact_exists(&prepared_path, "prepared Match clear journal")?;
        if committed_exists {
            let (committed_header, committed_bytes) =
                self.read_validated_clear_rollback_journal_unlocked(&committed_path)?;
            if prepared_exists {
                let (prepared_header, prepared_bytes) =
                    self.read_validated_clear_rollback_journal_unlocked(&prepared_path)?;
                if prepared_header.state_digest != committed_header.state_digest
                    || prepared_bytes != committed_bytes
                {
                    return Err(
                        "committed Match clear marker contradicts its prepared journal".to_string(),
                    );
                }
                // The prepared artifact is removed only after the committed
                // marker has been durably published and verified. Therefore,
                // both names together still represent an unacknowledged clear:
                // restore pre-clear state after crashes or publication errors.
                let expected = ClearInventory {
                    state_digest: prepared_header.state_digest.clone(),
                    execution: prepared_header.execution.clone(),
                    tables: prepared_header.tables.clone(),
                    total_rows_to_delete: prepared_header.total_rows_to_delete,
                };
                // A crash after the rollback reset or between restore pages can
                // legitimately leave the execution row (or another inventory
                // dependency) absent.  Under verified prepared intent that is
                // evidence of an incomplete rollback, not a reason to abandon
                // recovery or discard either marker.
                let needs_restore = self.clear_inventory_unlocked().map_or(true, |current| {
                    current.state_digest != expected.state_digest
                        || current.tables != expected.tables
                        || current.total_rows_to_delete != expected.total_rows_to_delete
                });
                if needs_restore {
                    self.restore_clear_rollback_journal_unlocked(&prepared_path, &expected)?;
                }
                remove_durable_recovery_file(&prepared_path, "completed Match clear journal")?;
            }
            remove_durable_recovery_file(&committed_path, "completed Match clear marker")?;
            return Ok(());
        }
        if !prepared_exists {
            return Ok(());
        }
        let header = self.validate_clear_rollback_journal_unlocked(&prepared_path)?;
        let expected = ClearInventory {
            state_digest: header.state_digest.clone(),
            execution: header.execution.clone(),
            tables: header.tables.clone(),
            total_rows_to_delete: header.total_rows_to_delete,
        };
        let needs_restore = self.clear_inventory_unlocked().map_or(true, |current| {
            current.state_digest != expected.state_digest
                || current.tables != expected.tables
                || current.total_rows_to_delete != expected.total_rows_to_delete
        });
        if needs_restore {
            self.restore_clear_rollback_journal_unlocked(&prepared_path, &expected)?;
        }
        remove_durable_recovery_file(&prepared_path, "recovered Match clear journal")?;
        Ok(())
    }

    fn commit_clear_inventory_unlocked(
        &self,
        inventory: ClearInventory,
        mut verified: VerifiedRecoveryBundle,
        confirmation_token: String,
        recovery_snapshot: &IdentityBundleFileSnapshot,
    ) -> Result<MatchClearReceipt, String> {
        Self::revalidate_identity_bundle_snapshot_path(recovery_snapshot)?;
        let retained_proof =
            self.ensure_post_clear_recovery_snapshot_unlocked(recovery_snapshot)?;
        verified.canonical_path = retained_proof.canonical_path.clone();
        let rollback = self.prepare_clear_rollback_journal_unlocked(&inventory)?;
        let rollback_path = &rollback.prepared_path;
        let execution = PersistedExecutionState {
            desired_mode: inventory.execution.desired_mode.clone(),
            revision: inventory
                .execution
                .revision
                .checked_add(1)
                .ok_or("Match execution revision overflow")?,
            identity_revision: inventory
                .execution
                .identity_revision
                .checked_add(1)
                .ok_or("Match identity revision overflow")?,
            catalog_revision: inventory
                .execution
                .catalog_revision
                .checked_add(1)
                .ok_or("Match catalog revision overflow")?,
            updated_at: now(),
        };
        let execution_for_write = execution.clone();
        let db = self.store.db();
        let clear_result = surreal_store::run(async move {
            db.query(
                "BEGIN TRANSACTION;
                 DELETE match_person;
                 DELETE match_look;
                 DELETE match_trusted_template_set;
                 DELETE match_trusted_member;
                 DELETE match_face_observation;
                 DELETE match_face_embedding;
                 DELETE match_trusted_search_embedding;
                 DELETE match_calibration_activation;
                 DELETE match_calibration_spent_set;
                 DELETE match_trusted_index_build;
                 DELETE match_assignment;
                 DELETE match_suggestion;
                 DELETE match_face_disposition;
                 DELETE match_constraint;
                 DELETE match_operation;
                 DELETE match_correction_media_operation;
                 DELETE match_suggestion_source_provenance;
                 DELETE match_index_job;
                 DELETE match_job_asset;
                 DELETE match_video_observation;
                 DELETE match_review_context;
                 DELETE match_media_context;
                 DELETE match_video_checkpoint;
                 DELETE match_worker_quarantine;
                 DELETE match_model_generation;
                 DELETE match_people_projection;
                 DELETE match_index_root;
                 DELETE match_recovery_face_closure;
                 DELETE match_execution;
                 UPSERT match_execution:global CONTENT $execution;
                 COMMIT TRANSACTION;",
            )
            .bind(("execution", execution_for_write))
            .await
            .map_err(|error| format!("clear all Match data transaction: {error}"))?
            .check()
            .map_err(|error| format!("clear all Match data transaction: {error}"))?;
            Ok::<(), String>(())
        });
        if let Err(error) = clear_result {
            let recovered = self.recover_interrupted_clear_unlocked();
            if recovered.is_err() {
                self.invalidate_match_caches_for_pending_clear_unlocked();
            }
            return Err(format!(
                "clear all Match data transaction failed ({error}); durable pre-clear recovery: {recovered:?}"
            ));
        }
        self.invalidate_match_caches_for_pending_clear_unlocked();
        #[cfg(test)]
        if LEAVE_PREPARED_CLEAR_AFTER_COMMIT.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err("injected interrupted clear after commit".to_string());
        }
        #[cfg(test)]
        if UNLINK_RETAINED_RECOVERY_AFTER_CLEAR_COMMIT
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            if let Err(error) = std::fs::remove_file(&retained_proof.canonical_path) {
                let rollback =
                    self.restore_clear_rollback_journal_unlocked(rollback_path, &inventory);
                if rollback.is_ok() {
                    let _ = remove_durable_recovery_file(
                        rollback_path,
                        "rolled-back Match clear journal",
                    );
                } else {
                    self.invalidate_match_caches_for_pending_clear_unlocked();
                }
                return Err(format!(
                    "inject retained recovery unlink failed ({error}); exact pre-clear rollback: {rollback:?}"
                ));
            }
        }
        #[cfg(test)]
        let finalized =
            if FAIL_NEXT_POST_CLEAR_FINALIZATION.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err("injected post-commit recovery finalization failure".to_string())
            } else {
                self.ensure_post_clear_recovery_snapshot_unlocked(&retained_proof)
            };
        #[cfg(not(test))]
        let finalized = self.ensure_post_clear_recovery_snapshot_unlocked(&retained_proof);
        let finalized_proof = match finalized {
            Ok(snapshot) => snapshot,
            Err(proof_error) => {
                let rollback =
                    self.restore_clear_rollback_journal_unlocked(rollback_path, &inventory);
                if rollback.is_ok() {
                    let _ = remove_durable_recovery_file(
                        rollback_path,
                        "rolled-back Match clear journal",
                    );
                } else {
                    self.invalidate_match_caches_for_pending_clear_unlocked();
                }
                return Err(format!(
                    "clear all Match data could not retain a named recovery artifact after commit ({proof_error}); exact pre-clear rollback: {rollback:?}"
                ));
            }
        };
        verified.canonical_path = finalized_proof.canonical_path.clone();
        #[cfg(test)]
        if FAIL_NEXT_CLEAR_COMMITTED_PUBLICATION.swap(false, std::sync::atomic::Ordering::SeqCst) {
            super::exchange::fail_next_recovery_publication_after_publish();
        }
        let committed = publish_durable_recovery_file(
            &rollback.committed_path,
            &rollback.bytes,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
        )
        .and_then(|()| {
            let observed = read_guarded_recovery_file(
                &rollback.committed_path,
                CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
                "committed Match clear rollback marker",
            )?;
            if observed != rollback.bytes {
                return Err(
                    "committed Match clear rollback marker failed exact byte verification"
                        .to_string(),
                );
            }
            Ok(())
        });
        if let Err(error) = committed {
            let restored = self.restore_clear_rollback_journal_unlocked(rollback_path, &inventory);
            if restored.is_ok() {
                let _ =
                    remove_durable_recovery_file(rollback_path, "rolled-back Match clear journal");
                if let Ok(observed) = read_guarded_recovery_file(
                    &rollback.committed_path,
                    CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
                    "failed committed Match clear rollback marker",
                ) {
                    if observed == rollback.bytes {
                        let _ = remove_durable_recovery_file(
                            &rollback.committed_path,
                            "failed committed Match clear marker",
                        );
                    }
                }
            } else {
                self.invalidate_match_caches_for_pending_clear_unlocked();
            }
            return Err(format!(
                "clear all Match data could not commit its durable rollback marker ({error}); exact pre-clear rollback: {restored:?}"
            ));
        }
        let prepared_cleanup =
            remove_durable_recovery_file(rollback_path, "prepared Match clear journal").and_then(
                |removed| {
                    removed.then_some(()).ok_or_else(|| {
                        "prepared Match clear journal disappeared before cleanup".to_string()
                    })
                },
            );
        if let Err(cleanup_error) = prepared_cleanup {
            #[cfg(all(test, windows))]
            if FAIL_NEXT_PREPARED_CLEAR_REPUBLICATION
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                super::exchange::fail_next_recovery_publication_after_publish();
            }
            let prepared_retention = self.retain_clear_rollback_artifact_unlocked(
                rollback_path,
                &rollback.bytes,
                "indeterminate prepared Match clear journal",
            );
            let committed_retention = self.retain_clear_rollback_artifact_unlocked(
                &rollback.committed_path,
                &rollback.bytes,
                "indeterminate committed Match clear journal",
            );
            if prepared_retention.is_err() || committed_retention.is_err() {
                self.invalidate_match_caches_for_pending_clear_unlocked();
                return Err(format!(
                    "indeterminate Match clear finalization: prepared-marker cleanup was ambiguous ({cleanup_error}); durable rollback intent could not be established; prepared marker retention: {prepared_retention:?}; committed marker retention: {committed_retention:?}; exact rollback was not started; mandatory recovery remains pending"
                ));
            }

            let restored = self.restore_clear_rollback_journal_unlocked(rollback_path, &inventory);
            if restored.is_ok() {
                let committed_cleanup_after_restore = remove_durable_recovery_file(
                    &rollback.committed_path,
                    "restored Match clear committed marker",
                )
                .and_then(|removed| {
                    removed.then_some(()).ok_or_else(|| {
                        "restored Match clear committed marker disappeared before cleanup"
                            .to_string()
                    })
                });
                let prepared_cleanup_after_restore = if committed_cleanup_after_restore.is_ok() {
                    remove_durable_recovery_file(
                        rollback_path,
                        "restored Match clear prepared journal",
                    )
                    .map(|_| ())
                } else {
                    Err("prepared rollback intent retained because committed-marker cleanup was ambiguous"
                        .to_string())
                };
                return Err(format!(
                    "clear prepared-marker cleanup was ambiguous ({cleanup_error}); exact pre-clear rollback: Ok(()); committed marker cleanup: {committed_cleanup_after_restore:?}; prepared marker cleanup: {prepared_cleanup_after_restore:?}"
                ));
            }
            self.invalidate_match_caches_for_pending_clear_unlocked();
            return Err(format!(
                "indeterminate Match clear finalization: prepared-marker cleanup was ambiguous ({cleanup_error}); exact rollback failed ({restored:?}) after durable prepared and committed rollback intent was verified; mandatory recovery remains pending"
            ));
        }
        let mut cleanup_warning = None;
        {
            #[cfg(all(test, windows))]
            let committed_cleanup = if FAIL_NEXT_COMMITTED_CLEAR_REMOVAL
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err("injected committed Match clear marker deletion failure".to_string())
            } else {
                remove_durable_recovery_file(
                    &rollback.committed_path,
                    "committed Match clear marker",
                )
                .map(|_| ())
            };
            #[cfg(not(all(test, windows)))]
            let committed_cleanup = remove_durable_recovery_file(
                &rollback.committed_path,
                "committed Match clear marker",
            )
            .map(|_| ());
            if let Err(error) = committed_cleanup {
                cleanup_warning = Some(format!(
                    "clear committed; committed-marker cleanup requires startup reconciliation: {error}"
                ));
            }
        }
        drop(finalized_proof);
        drop(retained_proof);
        let mut caches = self.cache_write_recover();
        caches.identity_revision = execution.identity_revision;
        caches.catalog_revision = execution.catalog_revision;
        caches.projections.clear();
        caches.autocomplete.valid = false;
        drop(caches);
        Ok(MatchClearReceipt {
            operation_id: new_id("clear-all-match-data"),
            kind: "clear_all_match_data".to_string(),
            confirmation_token,
            pre_state_digest: inventory.state_digest,
            deleted_rows: inventory.total_rows_to_delete,
            tables: inventory.tables,
            recovery_bundle: verified,
            execution_revision: execution.revision,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            raw_media_deleted: false,
            cleanup_warning,
        })
    }

    /// Recovery must never erase the durable fence for a still-live worker.
    /// Called under the existing mutation guard, including final preview recheck.
    fn require_recovery_worker_exit_unlocked(&self) -> Result<(), String> {
        let db = self.store.db();
        let blocked: bool = surreal_store::run(async move {
            let mut response = db.query(
                "SELECT VALUE worker_id FROM match_worker_quarantine WHERE confirmed_dead != true LIMIT 1;"
            ).await.map_err(|error| format!("read recovery worker quarantine: {error}"))?
                .check().map_err(|error| format!("read recovery worker quarantine: {error}"))?;
            let rows: Vec<String> = response
                .take(0)
                .map_err(|error| format!("decode recovery worker quarantine: {error}"))?;
            Ok::<_, String>(!rows.is_empty())
        })?;
        if blocked {
            return Err("Match recovery blocked: quarantined worker exit is not confirmed".into());
        }
        Ok(())
    }

    fn clear_inventory_unlocked(&self) -> Result<ClearInventory, String> {
        self.require_recovery_worker_exit_unlocked()?;
        let execution = self.execution_state_unlocked()?;
        let mut budget = RecoveryScanBudget::default();
        let mut tables = Vec::new();
        let mut total_rows_to_delete = 0usize;
        for (table, stable_id_field, state_class) in clear_delete_table_specs() {
            let mut manifest = ManifestAccumulator::new(
                table,
                stable_id_field,
                state_class,
                MatchStatePartition::All,
                true,
            );
            self.scan_recovery_table_unlocked(
                table,
                stable_id_field,
                &mut budget,
                |stable_id, row, _| manifest.push(stable_id, row),
            )?;
            let manifest = manifest.finish();
            total_rows_to_delete = total_rows_to_delete
                .checked_add(manifest.row_count)
                .ok_or("Match clear affected-row count overflow")?;
            tables.push(MatchClearTableDisposition {
                manifest,
                disposition: MatchClearDisposition::DeleteRows,
            });
        }
        let execution_rows = self.recovery_global_row_unlocked(EXECUTION_TABLE)?;
        tables.push(MatchClearTableDisposition {
            manifest: manifest_for(
                &execution_rows,
                MatchStateClass::RuntimeControl,
                MatchStatePartition::All,
                false,
            )?,
            disposition: MatchClearDisposition::ReinitializeExecution,
        });
        let schema_rows = self.recovery_global_row_unlocked("match_schema_state")?;
        tables.push(MatchClearTableDisposition {
            manifest: manifest_for(
                &schema_rows,
                MatchStateClass::SchemaAuthority,
                MatchStatePartition::All,
                false,
            )?,
            disposition: MatchClearDisposition::RetainSchemaAuthority,
        });
        tables.push(MatchClearTableDisposition {
            manifest: empty_manifest(
                "match_machine_cluster",
                "cluster_id",
                MatchStateClass::Absent,
                MatchStatePartition::MachineClustersAbsent,
            )?,
            disposition: MatchClearDisposition::Absent,
        });
        tables.sort_by(|left, right| left.manifest.table.cmp(&right.manifest.table));
        let state_digest = digest_serialized(&(
            RECOVERY_CONTRACT_VERSION,
            MATCH_SCHEMA_VERSION,
            MATCH_SCHEMA_GENERATION,
            execution.revision,
            execution.identity_revision,
            execution.catalog_revision,
            &tables,
        ))?;
        Ok(ClearInventory {
            state_digest,
            execution,
            tables,
            total_rows_to_delete,
        })
    }

    fn rebuild_inventory_unlocked(&self) -> Result<RebuildInventory, String> {
        self.require_recovery_worker_exit_unlocked()?;
        let execution = self.execution_state_unlocked()?;
        let mut budget = RecoveryScanBudget::default();
        self.clear_all_recovery_face_closure_unlocked()?;
        let closure_run_id = new_id("rebuild-face-closure");
        let mut pending_face_ids = Vec::with_capacity(RECOVERY_FACE_STAGE_PAGE);
        self.scan_recovery_table_unlocked(
            FACE_TABLE,
            "face_id",
            &mut budget,
            |face_id, row, _| {
                if row.get("operator_owned").and_then(Value::as_bool) == Some(true) {
                    pending_face_ids.push(face_id.to_string());
                    if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                        self.stage_recovery_face_ids_unlocked(
                            &closure_run_id,
                            &mut pending_face_ids,
                        )?;
                    }
                }
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;

        let mut confirmed_assignments = ManifestAccumulator::new(
            ASSIGNMENT_TABLE,
            "assignment_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::OperatorConfirmedAssignments,
            false,
        );
        let mut strict_assignments = ManifestAccumulator::new(
            ASSIGNMENT_TABLE,
            "assignment_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::StrictAutomaticAssignments,
            true,
        );
        self.scan_recovery_table_unlocked(
            ASSIGNMENT_TABLE,
            "assignment_id",
            &mut budget,
            |assignment_id, row, _| {
                let assignment =
                    serde_json::from_value::<Assignment>(row.clone()).map_err(|error| {
                        format!("Match assignment {assignment_id} row is malformed: {error}")
                    })?;
                if assignment.state == AssignmentState::OperatorConfirmed.as_str() {
                    confirmed_assignments.push(assignment_id, row)?;
                    pending_face_ids.push(assignment.face_id);
                    if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                        self.stage_recovery_face_ids_unlocked(
                            &closure_run_id,
                            &mut pending_face_ids,
                        )?;
                    }
                } else if assignment.state == AssignmentState::CommittedStrictAutomatic.as_str() {
                    strict_assignments.push(assignment_id, row)?;
                } else if assignment.state == AssignmentState::Suggestion.as_str() {
                    return Err(format!(
                        "Match assignment {assignment_id} is invalid: suggestions must remain separate from committed assignments"
                    ));
                } else {
                    return Err(format!(
                        "Match assignment {assignment_id} has unknown evidence state {}",
                        assignment.state
                    ));
                }
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;

        let mut constraint_manifest = ManifestAccumulator::new(
            CONSTRAINT_TABLE,
            "constraint_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::All,
            false,
        );
        self.scan_recovery_table_unlocked(
            CONSTRAINT_TABLE,
            "constraint_id",
            &mut budget,
            |id, row, _| {
                constraint_manifest.push(id, row)?;
                let face_id = row
                    .get("face_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("Match constraint {id} lacks FaceId"))?;
                pending_face_ids.push(face_id.to_string());
                if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                    self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
                }
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;

        let mut disposition_manifest = ManifestAccumulator::new(
            FACE_DISPOSITION_TABLE,
            "face_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::All,
            false,
        );
        self.scan_recovery_table_unlocked(
            FACE_DISPOSITION_TABLE,
            "face_id",
            &mut budget,
            |id, row, _| {
                disposition_manifest.push(id, row)?;
                pending_face_ids.push(id.to_string());
                if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                    self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
                }
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;

        let mut trusted_manifest = ManifestAccumulator::new(
            TRUSTED_MEMBER_TABLE,
            "membership_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::All,
            false,
        );
        self.scan_recovery_table_unlocked(
            TRUSTED_MEMBER_TABLE,
            "membership_id",
            &mut budget,
            |id, row, _| {
                trusted_manifest.push(id, row)?;
                let face_id = row
                    .get("face_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("Match trusted membership {id} lacks FaceId"))?;
                pending_face_ids.push(face_id.to_string());
                if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                    self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
                }
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;

        let mut operation_manifest = ManifestAccumulator::new(
            OPERATION_TABLE,
            "operation_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::All,
            false,
        );
        self.scan_recovery_table_unlocked(
            OPERATION_TABLE,
            "operation_id",
            &mut budget,
            |operation_id, row, _| {
                operation_manifest.push(operation_id, row)?;
                let operation =
                    serde_json::from_value::<MatchOperation>(row.clone()).map_err(|error| {
                        format!("Match operation {operation_id} row is malformed: {error}")
                    })?;
                for field in ["before_json", "after_json"] {
                    parse_rebuild_operation_history_field(operation_id, row, field)?;
                }
                let mut graph = IdentityBundleGraph {
                    operations: vec![operation.clone()],
                    ..IdentityBundleGraph::default()
                };
                if operation.kind == "undo_correction" {
                    let before: Value = serde_json::from_str(&operation.before_json)
                        .map_err(|error| format!("decode undo history receipt: {error}"))?;
                    if let Some(source_operation_id) =
                        before.get("operation_id").and_then(Value::as_str)
                    {
                        graph
                            .operations
                            .push(self.require_unlocked::<MatchOperation>(
                                OPERATION_TABLE,
                                source_operation_id,
                                "undo source operation",
                            )?);
                    }
                }
                if operation.kind == "authorize_trusted_reference" {
                    if let Some(face_id) = operation.face_id.as_deref() {
                        if let Some(assignment) =
                            self.get_one_unlocked::<Assignment>(ASSIGNMENT_TABLE, face_id)?
                        {
                            graph.assignments.push(assignment);
                        }
                    }
                }
                validate_match_operation(&operation, &graph).map_err(|error| {
                    format!(
                        "Match durable operation {operation_id} failed typed validation: {error}"
                    )
                })?;
                Ok(())
            },
        )?;
        self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
        let mut video_context_manifests = Vec::new();
        for (table, id_field) in [
            (super::video::VIDEO_OBSERVATION_TABLE, "observation_id"),
            (super::context::CONTEXT_TABLE, "context_id"),
            (super::context::MEDIA_CONTEXT_TABLE, "media_key"),
        ] {
            let mut manifest = ManifestAccumulator::new(
                table,
                id_field,
                MatchStateClass::DurableOperatorTruth,
                MatchStatePartition::All,
                false,
            );
            self.scan_recovery_table_unlocked(table, id_field, &mut budget, |id, row, _| {
                manifest.push(id, row)?;
                if table == super::context::MEDIA_CONTEXT_TABLE {
                    serde_json::from_value::<CanonicalMediaContext>(row.clone())
                        .map_err(|e| e.to_string())?
                        .validate()?;
                    return Ok(());
                }
                let face_id = row
                    .get("face_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| format!("Match {table} row {id} lacks FaceId"))?;
                pending_face_ids.push(face_id.to_string());
                if pending_face_ids.len() >= RECOVERY_FACE_STAGE_PAGE {
                    self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
                }
                Ok(())
            })?;
            self.stage_recovery_face_ids_unlocked(&closure_run_id, &mut pending_face_ids)?;
            video_context_manifests.push(manifest.finish());
        }
        let mut durable_faces = ManifestAccumulator::new(
            FACE_TABLE,
            "face_id",
            MatchStateClass::DurableOperatorTruth,
            MatchStatePartition::DurableFaceReferenceClosure,
            false,
        );
        let mut derived_faces = ManifestAccumulator::new(
            FACE_TABLE,
            "face_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::UnreferencedDerivedFaces,
            true,
        );
        self.scan_rebuild_face_partitions_unlocked(
            &closure_run_id,
            &mut budget,
            &mut durable_faces,
            &mut derived_faces,
        )?;

        let strict_count = strict_assignments.row_count;
        let derived_face_count = derived_faces.row_count;
        let embeddings = self.recovery_manifest_unlocked(
            EMBEDDING_TABLE,
            "embedding_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::All,
            true,
            &mut budget,
        )?;
        let trusted_search = self.recovery_manifest_unlocked(
            TRUSTED_SEARCH_TABLE,
            "membership_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::All,
            true,
            &mut budget,
        )?;
        let trusted_index_builds = self.recovery_manifest_unlocked(
            TRUSTED_INDEX_BUILD_TABLE,
            "receipt_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::All,
            true,
            &mut budget,
        )?;
        let suggestions = self.recovery_manifest_unlocked(
            SUGGESTION_TABLE,
            "suggestion_id",
            MatchStateClass::Regenerable,
            MatchStatePartition::All,
            true,
            &mut budget,
        )?;
        let projections = self.recovery_manifest_unlocked(
            PROJECTION_TABLE,
            "media_key",
            MatchStateClass::Regenerable,
            MatchStatePartition::All,
            true,
            &mut budget,
        )?;
        let embeddings_count = embeddings.row_count;
        let trusted_search_count = trusted_search.row_count;
        let trusted_index_build_count = trusted_index_builds.row_count;
        let suggestions_count = suggestions.row_count;
        let projections_count = projections.row_count;
        let mut manifests = vec![
            durable_faces.finish(),
            derived_faces.finish(),
            confirmed_assignments.finish(),
            strict_assignments.finish(),
            embeddings,
            trusted_search,
            trusted_index_builds,
            suggestions,
            projections,
            constraint_manifest.finish(),
            disposition_manifest.finish(),
            trusted_manifest.finish(),
            operation_manifest.finish(),
        ];
        manifests.extend(video_context_manifests);

        for (table, stable_id_field, state_class) in [
            (
                PERSON_TABLE,
                "person_id",
                MatchStateClass::DurableOperatorTruth,
            ),
            (LOOK_TABLE, "look_id", MatchStateClass::DurableOperatorTruth),
            (
                TEMPLATE_SET_TABLE,
                "set_id",
                MatchStateClass::DurableOperatorTruth,
            ),
            (
                "match_correction_media_operation",
                "mapping_id",
                MatchStateClass::DurableOperatorTruth,
            ),
            (
                "match_suggestion_source_provenance",
                "provenance_id",
                MatchStateClass::DurableOperatorTruth,
            ),
            (ROOT_CONFIG_TABLE, "root_id", MatchStateClass::Configuration),
            (
                GENERATION_TABLE,
                "generation",
                MatchStateClass::Configuration,
            ),
            (
                CALIBRATION_TABLE,
                "calibration_generation",
                MatchStateClass::Configuration,
            ),
            (
                CALIBRATION_SPENT_TABLE,
                "activation_run_id",
                MatchStateClass::SecurityHistory,
            ),
            (JOB_TABLE, "job_id", MatchStateClass::RuntimeControl),
            (JOB_ASSET_TABLE, "asset_id", MatchStateClass::RuntimeControl),
            (
                super::video::VIDEO_CHECKPOINT_TABLE,
                "checkpoint_id",
                MatchStateClass::RuntimeControl,
            ),
            (
                "match_worker_quarantine",
                "worker_id",
                MatchStateClass::RuntimeControl,
            ),
        ] {
            manifests.push(self.recovery_manifest_unlocked(
                table,
                stable_id_field,
                state_class,
                MatchStatePartition::All,
                false,
                &mut budget,
            )?);
        }
        let execution_rows = self.recovery_global_row_unlocked(EXECUTION_TABLE)?;
        manifests.push(manifest_for(
            &execution_rows,
            MatchStateClass::RuntimeControl,
            MatchStatePartition::All,
            false,
        )?);
        let schema_rows = self.recovery_global_row_unlocked("match_schema_state")?;
        manifests.push(manifest_for(
            &schema_rows,
            MatchStateClass::SchemaAuthority,
            MatchStatePartition::All,
            false,
        )?);
        manifests.push(empty_manifest(
            "match_machine_cluster",
            "cluster_id",
            MatchStateClass::Absent,
            MatchStatePartition::MachineClustersAbsent,
        )?);
        manifests.sort_by(|left, right| {
            (&left.table, left.partition).cmp(&(&right.table, right.partition))
        });

        let affected = MatchRebuildAffectedCounts {
            derived_faces: derived_face_count,
            embeddings: embeddings_count,
            trusted_search_embeddings: trusted_search_count,
            trusted_index_builds: trusted_index_build_count,
            suggestions: suggestions_count,
            projections: projections_count,
            strict_automatic_assignments: strict_count,
            machine_clusters: 0,
            total_rows: [
                derived_face_count,
                embeddings_count,
                trusted_search_count,
                trusted_index_build_count,
                suggestions_count,
                projections_count,
                strict_count,
            ]
            .into_iter()
            .try_fold(0usize, |total, count| {
                total
                    .checked_add(count)
                    .ok_or("Match rebuild affected-row count overflow")
            })?,
        };

        let state_digest = digest_serialized(&(
            RECOVERY_CONTRACT_VERSION,
            MATCH_SCHEMA_VERSION,
            MATCH_SCHEMA_GENERATION,
            execution.revision,
            execution.identity_revision,
            execution.catalog_revision,
            &manifests,
        ))?;
        let preview_id = digest_serialized(&(
            "rebuild_match_analysis_preview",
            RECOVERY_CONTRACT_VERSION,
            &state_digest,
        ))?;
        let preview = MatchRebuildPreview {
            contract_version: RECOVERY_CONTRACT_VERSION,
            preview_id,
            state_digest,
            schema_version: MATCH_SCHEMA_VERSION,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            execution_revision: execution.revision,
            identity_revision: execution.identity_revision,
            catalog_revision: execution.catalog_revision,
            manifests,
            affected,
            raw_media_deleted: false,
        };
        Ok(RebuildInventory {
            preview,
            closure_run_id,
        })
    }

    fn recovery_global_row_unlocked(&self, table: &'static str) -> Result<ManifestRows, String> {
        let db = self.store.db();
        let sql = format!("SELECT * OMIT id FROM ONLY type::record('{table}', 'global');");
        let row: Option<Value> = surreal_store::run(async move {
            let mut response = db
                .query(sql)
                .await
                .map_err(|error| format!("read Match recovery global row: {error}"))?;
            response
                .take(0)
                .map_err(|error| format!("decode Match recovery global row: {error}"))
        })?;
        Ok(ManifestRows {
            table,
            stable_id_field: "record_id",
            rows: row
                .into_iter()
                .map(|row| ("global".to_string(), row))
                .collect(),
        })
    }

    #[cfg(test)]
    fn recovery_table_rows_unlocked(
        &self,
        table: &'static str,
        stable_id_field: &'static str,
    ) -> Result<ManifestRows, String> {
        let mut rows = Vec::new();
        let mut after_id = String::new();
        loop {
            let db = self.store.db();
            let sql = format!(
                "SELECT * OMIT id FROM {table} WHERE {stable_id_field} > $after_id ORDER BY {stable_id_field} ASC LIMIT {RECOVERY_MANIFEST_PAGE};"
            );
            let page_after = after_id.clone();
            let page: Vec<Value> = surreal_store::run(async move {
                let mut response = db
                    .query(sql)
                    .bind(("after_id", page_after))
                    .await
                    .map_err(|error| format!("read Match recovery manifest page: {error}"))?;
                response
                    .take(0)
                    .map_err(|error| format!("decode Match recovery manifest page: {error}"))
            })?;
            if page.is_empty() {
                break;
            }
            for row in page {
                let stable_id = row
                    .get(stable_id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{table} recovery row lacks stable {stable_id_field}"))?
                    .to_string();
                if stable_id <= after_id {
                    return Err(format!("{table} recovery manifest page did not advance"));
                }
                after_id = stable_id.clone();
                rows.push((stable_id, row));
            }
            if rows.len() % RECOVERY_MANIFEST_PAGE != 0 {
                break;
            }
        }
        Ok(ManifestRows {
            table,
            stable_id_field,
            rows,
        })
    }
}

fn manifest_for(
    rows: &ManifestRows,
    state_class: MatchStateClass,
    partition: MatchStatePartition,
    selected_for_rebuild: bool,
) -> Result<MatchTableManifest, String> {
    let mut manifest = ManifestAccumulator::new(
        rows.table,
        rows.stable_id_field,
        state_class,
        partition,
        selected_for_rebuild,
    );
    for (id, row) in &rows.rows {
        manifest.push(id, row)?;
    }
    Ok(manifest.finish())
}

fn empty_manifest(
    table: &'static str,
    stable_id_field: &'static str,
    state_class: MatchStateClass,
    partition: MatchStatePartition,
) -> Result<MatchTableManifest, String> {
    Ok(ManifestAccumulator::new(table, stable_id_field, state_class, partition, false).finish())
}

fn digest_serialized(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn clear_delete_table_specs() -> Vec<(&'static str, &'static str, MatchStateClass)> {
    vec![
        (
            super::context::MEDIA_CONTEXT_TABLE,
            "media_key",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            super::video::VIDEO_OBSERVATION_TABLE,
            "observation_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            super::context::CONTEXT_TABLE,
            "context_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            super::video::VIDEO_CHECKPOINT_TABLE,
            "checkpoint_id",
            MatchStateClass::RuntimeControl,
        ),
        (
            "match_worker_quarantine",
            "worker_id",
            MatchStateClass::RuntimeControl,
        ),
        (
            PERSON_TABLE,
            "person_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (LOOK_TABLE, "look_id", MatchStateClass::DurableOperatorTruth),
        (
            TEMPLATE_SET_TABLE,
            "set_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            TRUSTED_MEMBER_TABLE,
            "membership_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            FACE_TABLE,
            "face_id",
            MatchStateClass::MixedDurableAndRegenerable,
        ),
        (
            EMBEDDING_TABLE,
            "embedding_id",
            MatchStateClass::Regenerable,
        ),
        (
            TRUSTED_SEARCH_TABLE,
            "membership_id",
            MatchStateClass::Regenerable,
        ),
        (
            CALIBRATION_TABLE,
            "calibration_generation",
            MatchStateClass::Configuration,
        ),
        (
            CALIBRATION_SPENT_TABLE,
            "activation_run_id",
            MatchStateClass::SecurityHistory,
        ),
        (
            TRUSTED_INDEX_BUILD_TABLE,
            "receipt_id",
            MatchStateClass::Regenerable,
        ),
        (
            ASSIGNMENT_TABLE,
            "assignment_id",
            MatchStateClass::MixedDurableAndRegenerable,
        ),
        (
            SUGGESTION_TABLE,
            "suggestion_id",
            MatchStateClass::Regenerable,
        ),
        (
            FACE_DISPOSITION_TABLE,
            "face_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            CONSTRAINT_TABLE,
            "constraint_id",
            MatchStateClass::DurableOperatorTruth,
        ),
        (
            OPERATION_TABLE,
            "operation_id",
            MatchStateClass::SecurityHistory,
        ),
        (
            "match_correction_media_operation",
            "mapping_id",
            MatchStateClass::SecurityHistory,
        ),
        (
            "match_suggestion_source_provenance",
            "provenance_id",
            MatchStateClass::SecurityHistory,
        ),
        (JOB_TABLE, "job_id", MatchStateClass::RuntimeControl),
        (JOB_ASSET_TABLE, "asset_id", MatchStateClass::RuntimeControl),
        (
            GENERATION_TABLE,
            "generation",
            MatchStateClass::Configuration,
        ),
        (PROJECTION_TABLE, "media_key", MatchStateClass::Regenerable),
        (ROOT_CONFIG_TABLE, "root_id", MatchStateClass::Configuration),
    ]
}

fn clear_rollback_table_specs() -> Vec<(&'static str, &'static str, MatchStateClass)> {
    clear_delete_table_specs()
        .into_iter()
        .filter(|(table, _, _)| !matches!(*table, EMBEDDING_TABLE | TRUSTED_SEARCH_TABLE))
        .collect()
}

fn clear_rollback_expected_rows(inventory: &ClearInventory) -> Result<usize, String> {
    let retained = clear_rollback_table_specs()
        .into_iter()
        .map(|(table, _, _)| table)
        .collect::<BTreeSet<_>>();
    inventory
        .tables
        .iter()
        .filter(|entry| retained.contains(entry.manifest.table.as_str()))
        .try_fold(0usize, |total, entry| {
            total
                .checked_add(entry.manifest.row_count)
                .ok_or_else(|| "Match clear rollback row count overflow".to_string())
        })
}

fn clear_rollback_inventory_matches(
    observed: &ClearInventory,
    expected: &ClearInventory,
    projected_operation_manifest: &MatchTableManifest,
) -> bool {
    let retained = clear_rollback_table_specs()
        .into_iter()
        .map(|(table, _, _)| table)
        .collect::<BTreeSet<_>>();
    let excluded_vectors = [EMBEDDING_TABLE, TRUSTED_SEARCH_TABLE]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let expected_tables = expected
        .tables
        .iter()
        .filter(|entry| {
            retained.contains(entry.manifest.table.as_str())
                && entry.manifest.table != OPERATION_TABLE
        })
        .map(|entry| (entry.manifest.table.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let observed_tables = observed
        .tables
        .iter()
        .filter(|entry| {
            retained.contains(entry.manifest.table.as_str())
                && entry.manifest.table != OPERATION_TABLE
        })
        .map(|entry| (entry.manifest.table.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    expected_tables == observed_tables
        && observed.tables.iter().any(|entry| {
            entry.manifest.table == OPERATION_TABLE
                && entry.manifest == *projected_operation_manifest
        })
        && observed.tables.iter().all(|entry| {
            !excluded_vectors.contains(entry.manifest.table.as_str())
                || entry.manifest.row_count == 0
        })
        && observed.execution == expected.execution
}

fn clear_confirmation_token(
    inventory: &ClearInventory,
    bundle: &VerifiedRecoveryBundle,
) -> Result<String, String> {
    let mut location_independent_bundle = bundle.clone();
    location_independent_bundle.canonical_path = PathBuf::new();
    digest_serialized(&(
        "clear_all_match_data",
        RECOVERY_CONTRACT_VERSION,
        &inventory.state_digest,
        MATCH_SCHEMA_VERSION,
        MATCH_SCHEMA_GENERATION,
        inventory.execution.revision,
        inventory.execution.identity_revision,
        inventory.execution.catalog_revision,
        &location_independent_bundle,
        &inventory.tables,
    ))
}

fn verified_recovery_bundle(
    snapshot: &IdentityBundleFileSnapshot,
    reconciliation: &IdentityReconciliation,
    expected_canonical_bytes: usize,
) -> Result<VerifiedRecoveryBundle, String> {
    let verified = verified_recovery_bundle_without_reconciliation(
        snapshot,
        reconciliation.state_sha256.clone(),
        reconciliation.reference_count,
    )?;
    if verified.canonical_bytes != expected_canonical_bytes
        || verified.entity_count != reconciliation.entity_count
        || verified.content_sha256 != reconciliation.content_sha256
    {
        return Err(format!(
            "recovery bundle bounded verification disagrees with export reconciliation: bytes {} vs {}, entities {} vs {}, content_hash_equal {}",
            verified.canonical_bytes, expected_canonical_bytes,
            verified.entity_count, reconciliation.entity_count,
            verified.content_sha256 == reconciliation.content_sha256,
        ));
    }
    Ok(verified)
}

fn verified_recovery_bundle_without_reconciliation(
    snapshot: &IdentityBundleFileSnapshot,
    portable_state_sha256: String,
    reference_count: usize,
) -> Result<VerifiedRecoveryBundle, String> {
    let bundle = &snapshot.bundle;
    let canonical_path = snapshot.canonical_path.clone();
    let canonical_bytes = snapshot.canonical_bytes;
    let file_sha256 = snapshot.file_sha256.clone();
    let entity_count = identity_bundle_entity_count(&bundle.graph);
    let table_counts = identity_bundle_table_counts(&bundle.graph);
    let restore_token = digest_serialized(&(
        "restore_clear_recovery_bundle",
        IDENTITY_BUNDLE_FORMAT,
        IDENTITY_BUNDLE_VERSION,
        MATCH_SCHEMA_VERSION,
        MATCH_SCHEMA_GENERATION,
        &bundle.manifest.content_sha256,
        &file_sha256,
        canonical_bytes,
    ))?;
    Ok(VerifiedRecoveryBundle {
        canonical_path,
        format: bundle.manifest.format.clone(),
        version: bundle.manifest.version,
        schema_version: bundle.manifest.schema_version,
        schema_generation: bundle.manifest.schema_generation.clone(),
        content_sha256: bundle.manifest.content_sha256.clone(),
        file_sha256,
        canonical_bytes,
        entity_count,
        reference_count,
        portable_state_sha256,
        table_counts,
        restore_token,
    })
}

fn require_recovery_bundle_matches_current(
    snapshot: &IdentityBundleFileSnapshot,
    verified: &VerifiedRecoveryBundle,
    current_bundle: &IdentityBundleV1,
    current: &IdentityReconciliation,
) -> Result<(), String> {
    let current_table_counts = identity_bundle_table_counts(&current_bundle.graph);
    if snapshot.bundle.graph != current_bundle.graph
        || snapshot.bundle.manifest != current_bundle.manifest
        || verified.format != current_bundle.manifest.format
        || verified.version != current_bundle.manifest.version
        || verified.schema_version != current_bundle.manifest.schema_version
        || verified.schema_generation != current_bundle.manifest.schema_generation
        || verified.content_sha256 != current_bundle.manifest.content_sha256
        || verified.content_sha256 != current.content_sha256
        || verified.portable_state_sha256 != current.state_sha256
        || verified.entity_count != current.entity_count
        || verified.entity_count != identity_bundle_entity_count(&current_bundle.graph)
        || verified.reference_count != current.reference_count
        || verified.reference_count != identity_bundle_reference_count(&current_bundle.graph)
        || verified.table_counts != current_table_counts
    {
        return Err(
            "recovery bundle does not match the exact current portable Match graph".to_string(),
        );
    }
    Ok(())
}

fn identity_bundle_entity_count(graph: &IdentityBundleGraph) -> usize {
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

fn identity_bundle_table_counts(graph: &IdentityBundleGraph) -> BTreeMap<String, usize> {
    BTreeMap::from([
        (
            super::video::VIDEO_OBSERVATION_TABLE.to_string(),
            graph.video_observations.len(),
        ),
        (
            super::context::CONTEXT_TABLE.to_string(),
            graph.review_context.len(),
        ),
        (
            super::context::MEDIA_CONTEXT_TABLE.to_string(),
            graph.media_context.len(),
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
            "match_correction_media_operation".to_string(),
            graph.correction_media_operations.len(),
        ),
        (
            "match_suggestion_source_provenance".to_string(),
            graph.suggestion_source_provenance.len(),
        ),
        (ROOT_CONFIG_TABLE.to_string(), graph.roots.len()),
        (GENERATION_TABLE.to_string(), graph.model_generations.len()),
    ])
}

fn identity_bundle_reference_count(graph: &IdentityBundleGraph) -> usize {
    graph.looks.len()
        + graph.video_observations.len()
        + graph.review_context.len() * 2
        + graph.trusted_template_sets.len()
        + graph
            .assignments
            .iter()
            .map(|assignment| {
                3 + usize::from(assignment.look_id.is_some())
                    + usize::from(assignment.state == "committed_strict_automatic")
            })
            .sum::<usize>()
        + graph.suggestions.len() * 3
        + graph.trusted_memberships.len() * 5
        + graph.constraints.len() * 3
        + graph.dispositions.len() * 2
        + graph.correction_media_operations.len()
        + graph.suggestion_source_provenance.len() * 3
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = BTreeMap::new();
            for (key, value) in object {
                sorted.insert(key, canonicalize_json(value));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

fn parse_rebuild_operation_history_field(
    operation_id: &str,
    row: &Value,
    field: &str,
) -> Result<Value, String> {
    let serialized = row
        .get(field)
        .ok_or_else(|| format!("Match operation {operation_id} is missing {field}"))?
        .as_str()
        .ok_or_else(|| format!("Match operation {operation_id} {field} must be a string"))?;
    if serialized.len() > IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES {
        return Err(format!(
            "Match operation {operation_id} {field} exceeds byte limit {IDENTITY_BUNDLE_MAX_SINGLE_STRING_BYTES}"
        ));
    }
    preflight_identity_bundle_json(serialized.as_bytes()).map_err(|error| {
        format!("Match operation {operation_id} {field} failed bounded JSON preflight: {error}")
    })?;
    serde_json::from_str(serialized).map_err(|error| {
        format!("Match operation {operation_id} {field} contains invalid JSON: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_io::{MediaIoCoordinator, RootIdentity, RootKind};
    use std::path::{Path, PathBuf};

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial-wp085-recovery-{name}-{}",
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
    fn clear_rollback_excludes_regenerable_vector_payload_tables() {
        let deleted = clear_delete_table_specs()
            .into_iter()
            .map(|(table, _, _)| table)
            .collect::<BTreeSet<_>>();
        let restored = clear_rollback_table_specs()
            .into_iter()
            .map(|(table, _, _)| table)
            .collect::<BTreeSet<_>>();
        assert!(deleted.contains(EMBEDDING_TABLE));
        assert!(deleted.contains(TRUSTED_SEARCH_TABLE));
        assert!(!restored.contains(EMBEDDING_TABLE));
        assert!(!restored.contains(TRUSTED_SEARCH_TABLE));
        assert!(restored.contains(PERSON_TABLE));
        assert!(restored.contains(OPERATION_TABLE));
    }

    #[test]
    fn clear_rollback_projects_vectors_out_of_operation_history() {
        let embedding = FaceEmbedding {
            embedding_id: "clear-history-embedding".to_string(),
            face_id: "clear-history-face".to_string(),
            vector: vec![0.25; EMBEDDING_DIM],
            model_generation: "clear-history-model".to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: "a".repeat(64),
            face_revision: 1,
            job_id: "clear-history-job".to_string(),
            active: true,
            created_at: "2026-08-26T00:00:00Z".to_string(),
        };
        let value = serde_json::to_value(&embedding).unwrap();
        let row = CorrectionRowDelta {
            table: CorrectionTable::Embedding,
            stable_id: embedding.embedding_id.clone(),
            before: Some(value.clone()),
            after: None,
        };
        let operation = MatchOperation {
            operation_id: "clear-history-operation".to_string(),
            kind: "correction_delete_face_analysis".to_string(),
            face_id: Some(embedding.face_id.clone()),
            person_id: None,
            before_json: serde_json::to_string(&vec![(
                CorrectionTable::Embedding,
                embedding.embedding_id.clone(),
                Some(value),
            )])
            .unwrap(),
            after_json: serde_json::to_string(
                &super::super::corrections::CorrectionDeltaEnvelope {
                    version: 1,
                    kind: "delete_face_analysis".to_string(),
                    rows: vec![row],
                    face_ids: vec![embedding.face_id],
                    media_keys: vec!["media/clear-history.jpg".to_string()],
                    identity_changed: false,
                    catalog_changed: false,
                },
            )
            .unwrap(),
            reversible: true,
            created_at: "2026-08-26T00:00:00Z".to_string(),
        };
        let projected = MatchStore::clear_rollback_row_value(
            OPERATION_TABLE,
            &serde_json::to_value(operation).unwrap(),
        )
        .unwrap();
        let projected: MatchOperation = serde_json::from_value(projected).unwrap();
        assert!(!projected.before_json.contains("\"vector\":["));
        assert!(!projected.after_json.contains("\"vector\":["));
        assert!(projected.before_json.contains("\"vector_omitted\":true"));
        assert!(projected.after_json.contains("\"vector_omitted\":true"));
    }

    fn seed_people(store: &MatchStore, prefix: &str, count: usize) -> Vec<Person> {
        let timestamp = now();
        let people = (0..count)
            .map(|index| Person {
                person_id: format!("{prefix}-{index:04}"),
                name: format!("{prefix} {index:04}"),
                aliases: Vec::new(),
                cover_media_key: None,
                hidden: false,
                favorite: false,
                revision: 1,
                catalog_revision: 1,
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let rows = people
            .iter()
            .map(|person| {
                (
                    PERSON_TABLE,
                    person.person_id.as_str(),
                    serde_json::to_value(person).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        store.transactional_upserts_deletes(&rows, &[]).unwrap();
        people
    }

    fn poison_match_caches(store: &MatchStore) {
        let mut caches = store.caches.write().unwrap();
        caches.identity_revision = u64::MAX;
        caches.catalog_revision = u64::MAX;
        caches.projections.insert(
            "stale/cache-only.jpg".to_string(),
            PeopleProjection {
                media_key: "stale/cache-only.jpg".to_string(),
                media_fingerprint: "stale-cache-only".to_string(),
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                model_generation: "stale-cache-only".to_string(),
                identity_revision: u64::MAX,
                catalog_revision: u64::MAX,
                person_ids: vec!["stale-cache-person".to_string()],
                published_at: now(),
            },
        );
        caches.autocomplete = AutocompleteIndex {
            valid: true,
            catalog_revision: u64::MAX,
            entries: vec![AutocompleteEntry {
                person_id: "stale-cache-person".to_string(),
                display_name: "Stale Only Cache".to_string(),
                search: "stale only cache".to_string(),
            }],
        };
        caches.last_query_plan = Some(QueryPlanEvidence {
            observed: true,
            index: "stale-cache-only".to_string(),
            uses_hnsw: true,
            exact_rerank: true,
            model_generation: "stale-cache-only".to_string(),
            observed_at: now(),
        });
    }

    fn face(id: &str, media_key: &str, operator_owned: bool) -> FaceObservation {
        FaceObservation {
            face_id: id.to_string(),
            media_key: media_key.to_string(),
            media_fingerprint: format!("{:x}", Sha256::digest(id.as_bytes())),
            source_index: 0,
            source_width: Some(640),
            source_height: Some(480),
            exif_orientation: Some(1),
            bounds_normalized: vec![0.1, 0.2, 0.3, 0.4],
            landmarks_normalized: Vec::new(),
            alignment_valid: true,
            quality: 0.9,
            pose_bucket: "frontal".to_string(),
            operator_owned,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: now(),
            updated_at: now(),
        }
    }

    fn assignment(id: &str, person_id: &str, state: AssignmentState) -> Assignment {
        Assignment {
            assignment_id: id.to_string(),
            face_id: id.to_string(),
            person_id: person_id.to_string(),
            media_key: format!("raw/{id}.jpg"),
            look_id: None,
            placement: "unsorted".to_string(),
            state: state.as_str().to_string(),
            provenance: "recovery-fixture".to_string(),
            locked: state == AssignmentState::OperatorConfirmed,
            model_generation: Some("model-recovery".to_string()),
            calibration_generation: None,
            envelope_hash: None,
            face_revision: 1,
            person_revision: 1,
            operation_id: format!("operation-{id}"),
            created_at: now(),
            updated_at: now(),
        }
    }

    fn seed_face(store: &MatchStore, id: &str, operator_owned: bool) {
        store
            .upsert_json(
                FACE_TABLE,
                id,
                &face(id, &format!("raw/{id}.jpg"), operator_owned),
            )
            .unwrap();
    }

    #[test]
    fn wp086_rebuild_preserves_video_context_face_closure_and_control_manifests() {
        let root = workspace("wp086-rebuild-closure");
        let store = MatchStore::open(&root).unwrap();
        for id in ["video-face-observation", "context-face", "unreferenced"] {
            seed_face(&store, id, false);
        }
        let person = store.create_person("Recovery context", Vec::new()).unwrap();
        let observation = serde_json::json!({"observation_id":"observation","track_id":"track","stream_index":0,
            "playback_origin":{"pts":0,"numerator":1,"denominator":1000},
            "time":{"pts":100,"numerator":1,"denominator":1000},"frame_sha256":"a".repeat(64),
            "detection":{"source_index":0,"bounds":[0.1,0.1,0.2,0.2],"quality":0.9,"pose_bucket":"frontal","detector_generation":"model"}});
        store.upsert_json(super::super::video::VIDEO_OBSERVATION_TABLE,"observation",&serde_json::json!({
            "observation_id":"observation","face_id":"video-face-observation","track_id":"track",
            "media_key":"raw/video-face-observation.jpg","media_fingerprint":format!("{:x}",Sha256::digest(b"video-face-observation")),
            "revision":1,"closed":true,"exemplar":true,"payload":observation.to_string()
        })).unwrap();
        store.upsert_json(super::super::context::CONTEXT_TABLE,"context",&serde_json::json!({
            "context_id":"context","face_id":"context-face","person_id":person.person_id,"payload":"[]"
        })).unwrap();
        let preview = store.preview_rebuild_match_analysis().unwrap();
        assert_eq!(preview.affected.derived_faces, 1);
        for table in [
            "match_video_observation",
            "match_review_context",
            "match_video_checkpoint",
            "match_worker_quarantine",
        ] {
            assert!(preview
                .manifests
                .iter()
                .any(|m| m.table == table && !m.selected_for_rebuild));
        }
        store.rebuild_match_analysis(&preview).unwrap();
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "video-face-observation")
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "context-face")
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "unreferenced")
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .preview_rebuild_match_analysis()
                .unwrap()
                .manifests
                .iter()
                .find(|m| m.table == "match_video_observation")
                .unwrap()
                .row_count,
            1
        );
        close(&root, store);
    }

    #[test]
    fn wp086_recovery_cannot_erase_unconfirmed_worker_quarantine() {
        let root = workspace("wp086-recovery-quarantine");
        let store = MatchStore::open(&root).unwrap();
        seed_face(&store, "retained", false);
        let preview = store.preview_rebuild_match_analysis().unwrap();
        let mut row = serde_json::json!({"worker_id":"worker","job_id":"job","model_generation":"model",
            "code":"safe_unit_timeout","message":"timeout","confirmed_dead":false,"created_at":now()});
        store
            .upsert_json("match_worker_quarantine", "worker", &row)
            .unwrap();
        assert!(store
            .preview_rebuild_match_analysis()
            .unwrap_err()
            .contains("exit is not confirmed"));
        assert!(store
            .rebuild_match_analysis(&preview)
            .unwrap_err()
            .contains("exit is not confirmed"));
        assert!(store
            .clear_inventory_unlocked()
            .err()
            .unwrap()
            .contains("exit is not confirmed"));
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "retained")
            .unwrap()
            .is_some());
        row["confirmed_dead"] = serde_json::json!(true);
        store
            .upsert_json("match_worker_quarantine", "worker", &row)
            .unwrap();
        let fresh = store.preview_rebuild_match_analysis().unwrap();
        assert_ne!(preview.state_digest, fresh.state_digest);
        assert!(store.clear_inventory_unlocked().is_ok());
        store.rebuild_match_analysis(&fresh).unwrap();
        assert_eq!(
            store
                .preview_rebuild_match_analysis()
                .unwrap()
                .manifests
                .iter()
                .find(|m| m.table == "match_worker_quarantine")
                .unwrap()
                .row_count,
            1
        );
        close(&root, store);
    }

    #[test]
    fn preview_is_deterministic_and_any_state_drift_rejects_apply() {
        let root = workspace("preview-stale");
        let store = MatchStore::open(&root).unwrap();
        seed_face(&store, "derived-a", false);
        let first = store.preview_rebuild_match_analysis().unwrap();
        let second = store.preview_rebuild_match_analysis().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.affected.derived_faces, 1);
        assert!(!first.raw_media_deleted);

        seed_face(&store, "derived-b", false);
        let error = store.rebuild_match_analysis(&first).unwrap_err();
        assert_eq!(error, "stale or mismatched rebuild_match_analysis preview");
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "derived-a")
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn rebuild_streams_manifest_pages_and_atomically_deletes_large_fixture() {
        let root = workspace("large-streaming-rebuild");
        let store = MatchStore::open(&root).unwrap();
        let row_count = RECOVERY_MANIFEST_PAGE * 2 + 1;
        let timestamp = now();
        let projections = (0..row_count)
            .map(|index| PeopleProjection {
                media_key: format!("raw/large-rebuild-{index:06}.jpg"),
                media_fingerprint: format!("large-rebuild-fingerprint-{index:06}"),
                schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                model_generation: "large-rebuild-model".to_string(),
                identity_revision: 1,
                catalog_revision: 1,
                person_ids: Vec::new(),
                published_at: timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let db = store.store.db();
        surreal_store::run(async move {
            db.query(
                "BEGIN TRANSACTION;
                 FOR $row IN $projections {
                    UPSERT type::record('match_people_projection', $row.media_key) CONTENT $row;
                 };
                 COMMIT TRANSACTION;",
            )
            .bind(("projections", projections))
            .await
            .map_err(|error| format!("seed large Match rebuild fixture: {error}"))?
            .check()
            .map_err(|error| format!("seed large Match rebuild fixture: {error}"))?;
            Ok::<(), String>(())
        })
        .unwrap();

        let first = store.preview_rebuild_match_analysis().unwrap();
        let second = store.preview_rebuild_match_analysis().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.affected.projections, row_count);
        let receipt = store.rebuild_match_analysis(&first).unwrap();
        assert_eq!(receipt.removed, first.affected);
        assert_eq!(receipt.changed_rows, first.affected.total_rows);
        assert_eq!(store.count(PROJECTION_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn injected_rebuild_transaction_failure_rolls_back_every_delete() {
        let root = workspace("atomic-rebuild-rollback");
        let store = MatchStore::open(&root).unwrap();
        seed_face(&store, "rollback-derived-face", false);
        store
            .upsert_json(
                PROJECTION_TABLE,
                "raw/rollback.jpg",
                &PeopleProjection {
                    media_key: "raw/rollback.jpg".to_string(),
                    media_fingerprint: "rollback-fingerprint".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "rollback-model".to_string(),
                    identity_revision: 1,
                    catalog_revision: 1,
                    person_ids: Vec::new(),
                    published_at: now(),
                },
            )
            .unwrap();
        let preview = store.preview_rebuild_match_analysis().unwrap();
        let execution_before = store.execution_state().unwrap();
        let marker = store
            .store
            .database_root()
            .parent()
            .unwrap()
            .join("recovery")
            .join("force-rebuild-transaction-failure");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"test-only failure injection").unwrap();

        let error = store.rebuild_match_analysis(&preview).unwrap_err();
        assert!(
            error.contains("rebuild Match analysis transaction"),
            "unexpected rebuild failure: {error}"
        );
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "rollback-derived-face")
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<PeopleProjection>(PROJECTION_TABLE, "raw/rollback.jpg")
            .unwrap()
            .is_some());
        assert_eq!(store.execution_state().unwrap(), execution_before);
        std::fs::remove_file(marker).unwrap();
        close(&root, store);
    }

    #[test]
    fn recovery_scan_streams_past_old_aggregate_cap_and_bounds_each_row() {
        let mut budget = RecoveryScanBudget::default();
        for _ in 0..=1_000_000 {
            budget.observe_row("match_budget_fixture", 1).unwrap();
        }
        assert_eq!(budget.scanned_rows, 1_000_001);
        assert!(budget
            .observe_row("match_budget_fixture", RECOVERY_MAX_ROW_BYTES + 1)
            .unwrap_err()
            .contains("per-row ceiling"));
    }

    #[test]
    fn malformed_durable_operation_history_rejects_rebuild_preview_without_mutation() {
        let root = workspace("malformed-operation-history");
        let store = MatchStore::open(&root).unwrap();
        seed_face(&store, "history-only-face", false);
        let malformed = MatchOperation {
            operation_id: "malformed-history-operation".to_string(),
            kind: "correction_batch_not_sure".to_string(),
            face_id: None,
            person_id: None,
            before_json: "null".to_string(),
            after_json: "{\"face_id\":\"history-only-face\"".to_string(),
            reversible: false,
            created_at: now(),
        };
        store
            .upsert_json(OPERATION_TABLE, &malformed.operation_id, &malformed)
            .unwrap();
        let face_before = store
            .get_one::<FaceObservation>(FACE_TABLE, "history-only-face")
            .unwrap();
        let operation_before = store
            .get_one::<MatchOperation>(OPERATION_TABLE, &malformed.operation_id)
            .unwrap();

        let error = store.preview_rebuild_match_analysis().unwrap_err();
        assert!(
            error.contains("malformed-history-operation after_json failed bounded JSON preflight")
        );
        assert_eq!(
            store
                .get_one::<FaceObservation>(FACE_TABLE, "history-only-face")
                .unwrap(),
            face_before
        );
        assert_eq!(
            store
                .get_one::<MatchOperation>(OPERATION_TABLE, &malformed.operation_id)
                .unwrap(),
            operation_before
        );
        close(&root, store);
    }

    #[test]
    fn typed_invalid_durable_operation_history_rejects_rebuild_preview_without_mutation() {
        let root = workspace("typed-invalid-operation-history");
        let store = MatchStore::open(&root).unwrap();
        seed_face(&store, "typed-invalid-history-face", false);
        let typed_invalid = MatchOperation {
            operation_id: "typed-invalid-history-operation".to_string(),
            kind: "unknown_history_semantics".to_string(),
            face_id: None,
            person_id: None,
            before_json: "null".to_string(),
            after_json: serde_json::json!({
                "face_id": "typed-invalid-history-face"
            })
            .to_string(),
            reversible: false,
            created_at: now(),
        };
        store
            .upsert_json(OPERATION_TABLE, &typed_invalid.operation_id, &typed_invalid)
            .unwrap();
        let face_before = store
            .get_one::<FaceObservation>(FACE_TABLE, "typed-invalid-history-face")
            .unwrap();
        let operation_before = store
            .get_one::<MatchOperation>(OPERATION_TABLE, &typed_invalid.operation_id)
            .unwrap();

        let error = store.preview_rebuild_match_analysis().unwrap_err();
        assert!(error.contains(
            "typed-invalid-history-operation has an unknown Match operation kind unknown_history_semantics"
        ));
        assert_eq!(
            store
                .get_one::<FaceObservation>(FACE_TABLE, "typed-invalid-history-face")
                .unwrap(),
            face_before
        );
        assert_eq!(
            store
                .get_one::<MatchOperation>(OPERATION_TABLE, &typed_invalid.operation_id)
                .unwrap(),
            operation_before
        );
        close(&root, store);
    }

    #[test]
    fn clear_uses_retained_private_recovery_copy_after_export_path_disappears() {
        let root = workspace("private-clear-recovery");
        let store = MatchStore::open(&root).unwrap();
        store.create_person("Private recovery", Vec::new()).unwrap();
        let exported_path = root.join("operator-clear-export.json");
        let preview = store.preview_clear_all_match_data(&exported_path).unwrap();
        assert_ne!(preview.recovery_bundle.canonical_path, exported_path);
        assert!(preview.recovery_bundle.canonical_path.exists());
        std::fs::remove_file(&exported_path).unwrap();
        let mut forged = preview.clone();
        forged.recovery_bundle.file_sha256 = "../outside-recovery-root".to_string();
        assert!(store
            .clear_all_match_data(&forged)
            .unwrap_err()
            .contains("private Match recovery file SHA-256"));

        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert!(receipt.recovery_bundle.canonical_path.exists());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);

        let tampered_restore_path = root.join("tampered-private-recovery.json");
        std::fs::copy(
            &receipt.recovery_bundle.canonical_path,
            &tampered_restore_path,
        )
        .unwrap();
        let mut tampered_bytes = std::fs::read(&tampered_restore_path).unwrap();
        tampered_bytes.push(b'\n');
        std::fs::write(&tampered_restore_path, tampered_bytes).unwrap();
        assert!(store
            .restore_clear_recovery_bundle(
                &tampered_restore_path,
                &BTreeMap::new(),
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap_err()
            .contains("identity bundle is not canonical JSON"));
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);

        // A valid canonical bundle must still be bound to its own restore
        // token, independently of the parser rejecting malformed bytes.
        let different_bundle_path = root.join("different-canonical-recovery.json");
        store
            .export_identity_bundle(&different_bundle_path)
            .unwrap();
        MatchStore::read_identity_bundle_snapshot(&different_bundle_path).unwrap();
        assert!(store
            .restore_clear_recovery_bundle(
                &different_bundle_path,
                &BTreeMap::new(),
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap_err()
            .contains("restore token mismatch"));
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);

        let expected =
            MatchStore::read_identity_bundle_snapshot(&receipt.recovery_bundle.canonical_path)
                .unwrap();
        super::super::exchange::fail_next_identity_reconciliation();
        let restored = store
            .restore_clear_recovery_bundle(
                &receipt.recovery_bundle.canonical_path,
                &BTreeMap::new(),
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap();
        assert_eq!(
            restored.content_sha256,
            receipt.recovery_bundle.content_sha256
        );
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 1);
        assert_eq!(
            store
                .reconcile_identity_bundle(&expected.bundle)
                .unwrap_err(),
            "injected explicit identity reconciliation failure"
        );
        drop(expected);
        close(&root, store);
    }

    #[test]
    fn recovery_restore_rejects_replacing_post_clear_truth_without_mutation() {
        let root = workspace("restore-preserves-post-clear-truth");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Pre-clear recovery person", Vec::new())
            .unwrap();
        let exported_path = root.join("pre-clear-recovery.json");
        let preview = store.preview_clear_all_match_data(&exported_path).unwrap();
        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);

        let post_clear_person = store
            .create_person("Post-clear current truth", Vec::new())
            .unwrap();
        let (before_bundle, before_reconciliation) = {
            let _guard = store.store.transaction_lock().read().unwrap();
            store.current_identity_bundle_snapshot_unlocked().unwrap()
        };

        let error = store
            .restore_clear_recovery_bundle(
                &receipt.recovery_bundle.canonical_path,
                &BTreeMap::new(),
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap_err();
        assert!(
            error.contains(
                "recovery restore permits only creates or an exact already-applied graph"
            ),
            "{error}"
        );

        let (after_bundle, after_reconciliation) = {
            let _guard = store.store.transaction_lock().read().unwrap();
            store.current_identity_bundle_snapshot_unlocked().unwrap()
        };
        assert_eq!(after_bundle, before_bundle);
        assert_eq!(after_reconciliation, before_reconciliation);
        assert_eq!(
            store
                .get_one::<Person>(PERSON_TABLE, &post_clear_person.person_id)
                .unwrap(),
            Some(post_clear_person)
        );
        close(&root, store);
    }

    #[test]
    fn recovery_restore_rejects_updating_current_stable_id_without_mutation() {
        let root = workspace("restore-preserves-updated-stable-id");
        let store = MatchStore::open(&root).unwrap();
        let original = store
            .create_person("Pre-clear stable identity", Vec::new())
            .unwrap();
        let exported_path = root.join("pre-clear-stable-id-recovery.json");
        let preview = store.preview_clear_all_match_data(&exported_path).unwrap();
        let receipt = store.clear_all_match_data(&preview).unwrap();

        let mut current = original;
        current.name = "Post-clear replacement truth".to_string();
        current.revision += 1;
        current.updated_at = now();
        store
            .upsert_json(PERSON_TABLE, &current.person_id, &current)
            .unwrap();
        let (before_bundle, before_reconciliation) = {
            let _guard = store.store.transaction_lock().read().unwrap();
            store.current_identity_bundle_snapshot_unlocked().unwrap()
        };

        let error = store
            .restore_clear_recovery_bundle(
                &receipt.recovery_bundle.canonical_path,
                &BTreeMap::new(),
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap_err();
        assert!(error.contains("would overwrite or delete current portable Match rows"));

        let (after_bundle, after_reconciliation) = {
            let _guard = store.store.transaction_lock().read().unwrap();
            store.current_identity_bundle_snapshot_unlocked().unwrap()
        };
        assert_eq!(after_bundle, before_bundle);
        assert_eq!(after_reconciliation, before_reconciliation);
        assert_eq!(
            store
                .get_one::<Person>(PERSON_TABLE, &current.person_id)
                .unwrap(),
            Some(current)
        );
        close(&root, store);
    }

    #[test]
    fn private_recovery_publication_error_is_not_swallowed_when_final_path_exists() {
        let root = workspace("private-publication-durability-failure");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Private publication durability", Vec::new())
            .unwrap();
        let exported_path = root.join("private-publication-source.json");
        store
            .export_identity_recovery_bundle(&exported_path)
            .unwrap();
        let source = MatchStore::read_identity_bundle_snapshot(&exported_path).unwrap();
        super::super::exchange::fail_next_recovery_publication_after_publish();

        let error = store
            .private_recovery_snapshot_unlocked(&source)
            .unwrap_err();
        assert!(
            error.contains(
                "injected recovery publication durability failure after final path commit"
            ),
            "{error}"
        );
        let recovery_path = store
            .store
            .database_root()
            .parent()
            .unwrap()
            .join("recovery")
            .join(format!("clear-{}.json", source.file_sha256));
        assert!(recovery_path.is_file());

        // A later attempt may accept the artifact only through the separate,
        // exact lineage-verification path for a pre-existing file.
        let retained = store.private_recovery_snapshot_unlocked(&source).unwrap();
        assert_eq!(retained.bytes, source.bytes);
        assert_eq!(retained.file_sha256, source.file_sha256);
        drop(retained);
        drop(source);
        close(&root, store);
    }

    #[test]
    fn prepared_clear_journal_publication_failure_precedes_database_mutation() {
        let root = workspace("prepared-journal-publication-failure");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Prepared journal durability", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("prepared-journal-export.json"))
            .unwrap();
        super::super::exchange::fail_next_recovery_publication_after_publish();

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains(
                "injected recovery publication durability failure after final path commit"
            ),
            "{error}"
        );
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let (prepared_path, _) = store.clear_rollback_paths_unlocked().unwrap();
        assert!(prepared_path.is_file());
        let staging_debris = std::fs::read_dir(prepared_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|leaf| leaf.starts_with(".facial-exchange-") || leaf.contains(".tmp-"))
            .collect::<Vec<_>>();
        assert!(staging_debris.is_empty(), "{staging_debris:?}");
        store.recover_interrupted_clear_unlocked().unwrap();
        assert!(!prepared_path.exists());
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        close(&root, store);
    }

    #[test]
    fn committed_clear_marker_publication_failure_rolls_back_exact_state() {
        let root = workspace("committed-journal-publication-failure");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Committed journal durability", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("committed-journal-export.json"))
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        FAIL_NEXT_CLEAR_COMMITTED_PUBLICATION.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("could not commit its durable rollback marker")
                && error.contains("exact pre-clear rollback: Ok"),
            "{error}"
        );
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[test]
    fn committed_marker_publication_and_partial_rollback_failure_invalidates_caches_until_startup_recovery(
    ) {
        let root = workspace("committed-marker-partial-rollback-cache-invalidation");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Committed marker primary", Vec::new())
            .unwrap();
        let _people = seed_people(&store, "committed-marker-person", RECOVERY_MANIFEST_PAGE);
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("committed-marker-partial-rollback.json"))
            .unwrap();
        poison_match_caches(&store);
        FAIL_NEXT_CLEAR_COMMITTED_PUBLICATION.store(true, std::sync::atomic::Ordering::SeqCst);
        FAIL_NEXT_CLEAR_ROLLBACK_AFTER_FIRST_PAGE.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("could not commit its durable rollback marker")
                && error.contains("failure after first restored page"),
            "{error}"
        );
        let (prepared_path, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        let prepared_bytes = read_guarded_recovery_file(
            &prepared_path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "prepared committed-publication rollback intent",
        )
        .unwrap();
        let committed_bytes = read_guarded_recovery_file(
            &committed_path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "committed publication-failure rollback intent",
        )
        .unwrap();
        assert_eq!(prepared_bytes, committed_bytes);
        assert_eq!(
            store.count(PERSON_TABLE).unwrap(),
            RECOVERY_MANIFEST_PAGE as u64
        );
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }
        let autocomplete_error = store
            .autocomplete("stale only cache", u64::MAX, 10)
            .unwrap_err();
        assert!(
            autocomplete_error.contains("Match execution state"),
            "stale cache-only Person escaped instead of forcing a canonical rebuild: {autocomplete_error}"
        );

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert!(!prepared_path.exists());
        assert!(!committed_path.exists());
        assert_eq!(
            reopened.count(PERSON_TABLE).unwrap(),
            (RECOVERY_MANIFEST_PAGE + 1) as u64
        );
        let after = reopened.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, reopened);
    }

    #[cfg(windows)]
    #[test]
    fn prepared_clear_removal_sync_ambiguity_restores_before_error() {
        let root = workspace("prepared-removal-sync-ambiguity");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Prepared removal rollback", Vec::new())
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("prepared-removal-recovery.json"))
            .unwrap();
        super::exchange::fail_next_recovery_removal_after_delete();

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(error.contains("exact pre-clear rollback: Ok"), "{error}");
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[cfg(windows)]
    #[test]
    fn prepared_cleanup_and_rollback_failure_is_indeterminate_until_exact_recovery() {
        let root = workspace("prepared-cleanup-rollback-indeterminate");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Indeterminate rollback", Vec::new())
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("indeterminate-clear-recovery.json"))
            .unwrap();
        {
            let mut caches = store.caches.write().unwrap();
            caches.identity_revision = u64::MAX;
            caches.catalog_revision = u64::MAX;
            caches.projections.insert(
                "stale/cache.jpg".to_string(),
                PeopleProjection {
                    media_key: "stale/cache.jpg".to_string(),
                    media_fingerprint: "stale-cache".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "stale-cache".to_string(),
                    identity_revision: u64::MAX,
                    catalog_revision: u64::MAX,
                    person_ids: vec![person.person_id.clone()],
                    published_at: now(),
                },
            );
            caches.autocomplete.valid = true;
            caches.last_query_plan = Some(QueryPlanEvidence {
                observed: true,
                index: "stale-cache".to_string(),
                uses_hnsw: true,
                exact_rerank: true,
                model_generation: "stale-cache".to_string(),
                observed_at: now(),
            });
        }
        super::exchange::fail_next_recovery_removal_after_delete();
        FAIL_NEXT_CLEAR_ROLLBACK_RESET.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("indeterminate Match clear finalization"),
            "{error}"
        );
        assert!(
            error.contains("mandatory recovery remains pending"),
            "{error}"
        );
        let (prepared_path, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        assert!(prepared_path.is_file());
        assert!(committed_path.is_file());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }

        store.recover_interrupted_clear_unlocked().unwrap();
        assert!(!prepared_path.exists());
        assert!(!committed_path.exists());
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[cfg(windows)]
    #[test]
    fn prepared_intent_publication_failure_blocks_rollback_and_is_indeterminate() {
        let root = workspace("prepared-intent-republication-failure");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Prepared intent publication", Vec::new())
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("prepared-intent-recovery.json"))
            .unwrap();
        {
            let mut caches = store.caches.write().unwrap();
            caches.identity_revision = u64::MAX;
            caches.catalog_revision = u64::MAX;
            caches.projections.insert(
                "stale/publication.jpg".to_string(),
                PeopleProjection {
                    media_key: "stale/publication.jpg".to_string(),
                    media_fingerprint: "stale-publication".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "stale-publication".to_string(),
                    identity_revision: u64::MAX,
                    catalog_revision: u64::MAX,
                    person_ids: vec![person.person_id.clone()],
                    published_at: now(),
                },
            );
            caches.autocomplete.valid = true;
            caches.last_query_plan = Some(QueryPlanEvidence {
                observed: true,
                index: "stale-publication".to_string(),
                uses_hnsw: true,
                exact_rerank: true,
                model_generation: "stale-publication".to_string(),
                observed_at: now(),
            });
        }
        super::exchange::fail_next_recovery_removal_after_delete();
        FAIL_NEXT_PREPARED_CLEAR_REPUBLICATION.store(true, std::sync::atomic::Ordering::SeqCst);
        FAIL_NEXT_CLEAR_ROLLBACK_RESET.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("indeterminate Match clear finalization"),
            "{error}"
        );
        assert!(
            error.contains("durable rollback intent could not be established")
                && error.contains("exact rollback was not started"),
            "{error}"
        );
        assert!(
            FAIL_NEXT_CLEAR_ROLLBACK_RESET.swap(false, std::sync::atomic::Ordering::SeqCst),
            "rollback reset injection was consumed even though prepared intent publication failed"
        );
        let (prepared_path, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        assert!(prepared_path.is_file());
        assert!(committed_path.is_file());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }

        store.recover_interrupted_clear_unlocked().unwrap();
        assert!(!prepared_path.exists());
        assert!(!committed_path.exists());
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[cfg(windows)]
    #[test]
    fn prepared_intent_survives_partial_paged_rollback_and_startup_restores_exact_state() {
        let root = workspace("prepared-intent-partial-paged-rollback");
        let store = MatchStore::open(&root).unwrap();
        let primary_person = store
            .create_person("Paged rollback primary", Vec::new())
            .unwrap();
        let _people = seed_people(&store, "paged-rollback-person", RECOVERY_MANIFEST_PAGE);
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("partial-paged-rollback-recovery.json"))
            .unwrap();
        {
            let mut caches = store.caches.write().unwrap();
            caches.identity_revision = u64::MAX;
            caches.catalog_revision = u64::MAX;
            caches.projections.insert(
                "stale/partial-page.jpg".to_string(),
                PeopleProjection {
                    media_key: "stale/partial-page.jpg".to_string(),
                    media_fingerprint: "stale-partial-page".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "stale-partial-page".to_string(),
                    identity_revision: u64::MAX,
                    catalog_revision: u64::MAX,
                    person_ids: vec![primary_person.person_id.clone()],
                    published_at: now(),
                },
            );
            caches.autocomplete.valid = true;
            caches.last_query_plan = Some(QueryPlanEvidence {
                observed: true,
                index: "stale-partial-page".to_string(),
                uses_hnsw: true,
                exact_rerank: true,
                model_generation: "stale-partial-page".to_string(),
                observed_at: now(),
            });
        }
        super::exchange::fail_next_recovery_removal_after_delete();
        FAIL_NEXT_CLEAR_ROLLBACK_AFTER_FIRST_PAGE.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("indeterminate Match clear finalization"),
            "{error}"
        );
        assert!(
            error.contains("failure after first restored page")
                && error.contains("durable prepared and committed rollback intent was verified"),
            "{error}"
        );
        let (prepared_path, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        let prepared_bytes = read_guarded_recovery_file(
            &prepared_path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "prepared partial-page rollback intent",
        )
        .unwrap();
        let committed_bytes = read_guarded_recovery_file(
            &committed_path,
            CLEAR_ROLLBACK_JOURNAL_MAX_BYTES,
            "committed partial-page rollback intent",
        )
        .unwrap();
        assert_eq!(prepared_bytes, committed_bytes);
        assert_eq!(
            store.count(PERSON_TABLE).unwrap(),
            RECOVERY_MANIFEST_PAGE as u64
        );
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert!(!prepared_path.exists());
        assert!(!committed_path.exists());
        assert_eq!(
            reopened.count(PERSON_TABLE).unwrap(),
            (RECOVERY_MANIFEST_PAGE + 1) as u64
        );
        let after = reopened.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, reopened);
    }

    #[cfg(windows)]
    #[test]
    fn committed_clear_marker_cleanup_failure_returns_truthful_receipt_and_cache_state() {
        let root = workspace("committed-marker-cleanup-warning");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Committed cleanup warning", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("committed-cleanup-recovery.json"))
            .unwrap();
        {
            let mut caches = store.caches.write().unwrap();
            caches.identity_revision = u64::MAX;
            caches.catalog_revision = u64::MAX;
            caches.autocomplete.valid = true;
        }
        FAIL_NEXT_COMMITTED_CLEAR_REMOVAL.store(true, std::sync::atomic::Ordering::SeqCst);

        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert!(receipt
            .cleanup_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("startup reconciliation")));
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        let (_, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        assert!(committed_path.is_file());
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, receipt.identity_revision);
            assert_eq!(caches.catalog_revision, receipt.catalog_revision);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
        }
        store.recover_interrupted_clear_unlocked().unwrap();
        assert!(!committed_path.exists());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn recovery_proof_failure_precedes_clear_and_preserves_full_match_state() {
        let root = workspace("post-clear-proof-recovery");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Post-clear recovery", Vec::new())
            .unwrap();
        let media_root = root.join("portable-media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        store
            .upsert_json(
                ROOT_CONFIG_TABLE,
                "post-clear-root",
                &MatchIndexRoot {
                    root_id: "post-clear-root".to_string(),
                    path: std::fs::canonicalize(&media_root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    exclusions: vec!["excluded".to_string()],
                    enabled: true,
                    created_at: now(),
                    updated_at: now(),
                },
            )
            .unwrap();
        let spent = serde_json::json!({
            "activation_run_id": "spent-before-clear",
            "fixture_manifest_sha256": "1".repeat(64),
            "evidence_digest": "2".repeat(64),
            "person_hashes": ["3".repeat(64)],
            "acquisition_cluster_hashes": ["4".repeat(64)],
            "created_at": now()
        });
        store
            .upsert_json(CALIBRATION_SPENT_TABLE, "spent-before-clear", &spent)
            .unwrap();
        let exported_path = root.join("post-clear-recovery.json");
        let preview = store.preview_clear_all_match_data(&exported_path).unwrap();
        let expected =
            MatchStore::read_identity_bundle(&preview.recovery_bundle.canonical_path).unwrap();
        let inventory = store.clear_inventory_unlocked().unwrap();
        let mut repaired_location = preview.recovery_bundle.clone();
        repaired_location.canonical_path = root.join("different-repaired-location.json");
        assert_eq!(
            clear_confirmation_token(&inventory, &repaired_location).unwrap(),
            preview.confirmation_token
        );
        let recovery_root = store
            .store
            .database_root()
            .parent()
            .unwrap()
            .join("recovery");
        std::fs::write(
            recovery_root.join("force-post-clear-recovery-proof-failure"),
            b"test-only failure injection",
        )
        .unwrap();

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(error.contains("injected post-clear recovery proof failure"));
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let (observed, reconciliation) = {
            let _guard = store.store.transaction_lock().read().unwrap();
            store.current_identity_bundle_snapshot_unlocked().unwrap()
        };
        assert_eq!(observed, expected);
        assert_eq!(
            reconciliation.content_sha256,
            expected.manifest.content_sha256
        );
        assert_eq!(
            reconciliation.state_sha256,
            preview.recovery_bundle.portable_state_sha256
        );
        assert_eq!(
            store
                .get_one::<Value>(CALIBRATION_SPENT_TABLE, "spent-before-clear")
                .unwrap(),
            Some(spent)
        );
        std::fs::remove_file(recovery_root.join("force-post-clear-recovery-proof-failure"))
            .unwrap();
        close(&root, store);
    }

    #[test]
    fn post_commit_recovery_finalization_failure_atomically_restores_full_state() {
        let root = workspace("post-commit-recovery-rollback");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Post-commit rollback", Vec::new())
            .unwrap();
        let spent = serde_json::json!({
            "activation_run_id": "spent-post-commit-rollback",
            "fixture_manifest_sha256": "1".repeat(64),
            "evidence_digest": "2".repeat(64),
            "person_hashes": ["3".repeat(64)],
            "acquisition_cluster_hashes": ["4".repeat(64)],
            "created_at": now()
        });
        store
            .upsert_json(
                CALIBRATION_SPENT_TABLE,
                "spent-post-commit-rollback",
                &spent,
            )
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("post-commit-rollback.json"))
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        FAIL_NEXT_POST_CLEAR_FINALIZATION.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(error.contains("exact pre-clear rollback: Ok"), "{error}");
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .get_one::<Value>(CALIBRATION_SPENT_TABLE, "spent-post-commit-rollback")
                .unwrap(),
            Some(spent)
        );
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[test]
    fn post_clear_proof_and_rollback_failure_invalidates_caches_until_startup_recovery() {
        let root = workspace("post-clear-proof-rollback-cache-invalidation");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Post-clear proof pending recovery", Vec::new())
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("post-clear-proof-pending-recovery.json"))
            .unwrap();
        poison_match_caches(&store);
        FAIL_NEXT_POST_CLEAR_FINALIZATION.store(true, std::sync::atomic::Ordering::SeqCst);
        FAIL_NEXT_CLEAR_ROLLBACK_RESET.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(
            error.contains("could not retain a named recovery artifact after commit")
                && error.contains("injected Match clear rollback reset failure"),
            "{error}"
        );
        let (prepared_path, committed_path) = store.clear_rollback_paths_unlocked().unwrap();
        assert!(prepared_path.is_file());
        assert!(!committed_path.exists());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }
        let cleared_catalog_revision = store.execution_state().unwrap().catalog_revision;
        let stale_results = store
            .autocomplete("stale only cache", cleared_catalog_revision, 10)
            .unwrap();
        assert!(
            stale_results
                .iter()
                .all(|row| row.person_id != "stale-cache-person"),
            "stale cache-only Person escaped after proof failure: {stale_results:?}"
        );

        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let reopened = MatchStore::open(&root).unwrap();
        assert!(!prepared_path.exists());
        assert!(!committed_path.exists());
        assert!(reopened
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        let after = reopened.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, reopened);
    }

    #[test]
    fn prepared_clear_journal_restores_exact_state_on_restart() {
        let root = workspace("restart-clear-rollback");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Restart clear rollback", Vec::new())
            .unwrap();
        let spent = serde_json::json!({
            "activation_run_id": "spent-restart-clear-rollback",
            "fixture_manifest_sha256": "1".repeat(64),
            "evidence_digest": "2".repeat(64),
            "person_hashes": ["3".repeat(64)],
            "acquisition_cluster_hashes": ["4".repeat(64)],
            "created_at": now()
        });
        store
            .upsert_json(
                CALIBRATION_SPENT_TABLE,
                "spent-restart-clear-rollback",
                &spent,
            )
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        let rollback_path = store.clear_rollback_paths_unlocked().unwrap().0;
        let preview = store
            .preview_clear_all_match_data(&root.join("restart-clear-rollback.json"))
            .unwrap();
        poison_match_caches(&store);
        LEAVE_PREPARED_CLEAR_AFTER_COMMIT.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store.clear_all_match_data(&preview).unwrap_err(),
            "injected interrupted clear after commit"
        );
        {
            let caches = store.caches.read().unwrap();
            assert_eq!(caches.identity_revision, 0);
            assert_eq!(caches.catalog_revision, 0);
            assert!(caches.projections.is_empty());
            assert!(!caches.autocomplete.valid);
            assert!(caches.autocomplete.entries.is_empty());
            assert!(caches.last_query_plan.is_none());
        }
        assert!(rollback_path.is_file());
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();

        let reopened = MatchStore::open(&root).unwrap();
        assert!(reopened
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        assert_eq!(
            reopened
                .get_one::<Value>(CALIBRATION_SPENT_TABLE, "spent-restart-clear-rollback")
                .unwrap(),
            Some(spent)
        );
        let after = reopened.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        {
            let caches = reopened.caches.read().unwrap();
            assert_eq!(caches.identity_revision, before.execution.identity_revision);
            assert_eq!(caches.catalog_revision, before.execution.catalog_revision);
            assert!(caches.projections.is_empty());
            assert!(caches.last_query_plan.is_none());
        }
        assert!(!rollback_path.exists());
        close(&root, reopened);
    }

    #[test]
    fn ordinary_mutation_recovers_pending_clear_before_writing() {
        let root = workspace("same-process-clear-mutation-gate");
        let store = MatchStore::open(&root).unwrap();
        let before = store
            .create_person("Before pending clear", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("same-process-clear.json"))
            .unwrap();
        LEAVE_PREPARED_CLEAR_AFTER_COMMIT.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store.clear_all_match_data(&preview).unwrap_err(),
            "injected interrupted clear after commit"
        );
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);

        let after = store
            .create_person("After pending clear", Vec::new())
            .unwrap();
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &before.person_id)
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &after.person_id)
            .unwrap()
            .is_some());
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 2);
        assert!(!store.clear_rollback_paths_unlocked().unwrap().0.exists());
        close(&root, store);
    }

    #[test]
    fn ordinary_mutation_fails_closed_when_pending_clear_evidence_is_corrupt() {
        let root = workspace("corrupt-clear-mutation-gate");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Before corrupt pending clear", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("corrupt-pending-clear.json"))
            .unwrap();
        LEAVE_PREPARED_CLEAR_AFTER_COMMIT.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store.clear_all_match_data(&preview).unwrap_err(),
            "injected interrupted clear after commit"
        );
        let prepared = store.clear_rollback_paths_unlocked().unwrap().0;
        std::fs::write(&prepared, b"corrupt rollback evidence\n").unwrap();

        let error = store
            .create_person("Must not be written", Vec::new())
            .unwrap_err();
        assert!(
            error.contains("pending Match clear must recover"),
            "{error}"
        );
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn post_commit_clear_failure_restores_multiple_journal_pages() {
        let root = workspace("paged-clear-rollback");
        let store = MatchStore::open(&root).unwrap();
        for page_start in (0..257).step_by(64) {
            let page_end = (page_start + 64).min(257);
            let owned = (page_start..page_end)
                .map(|index| {
                    let id = format!("spent-paged-clear-{index:04}");
                    (
                        CALIBRATION_SPENT_TABLE.to_string(),
                        id.clone(),
                        serde_json::json!({
                            "activation_run_id": id,
                            "fixture_manifest_sha256": "1".repeat(64),
                            "evidence_digest": "2".repeat(64),
                            "person_hashes": ["3".repeat(64)],
                            "acquisition_cluster_hashes": ["4".repeat(64)],
                            "created_at": "2026-08-25T00:00:00.000Z"
                        }),
                    )
                })
                .collect::<Vec<_>>();
            let borrowed = owned
                .iter()
                .map(|(table, id, value)| (table.as_str(), id.as_str(), value.clone()))
                .collect::<Vec<_>>();
            store.transactional_upserts_deletes(&borrowed, &[]).unwrap();
        }
        let preview = store
            .preview_clear_all_match_data(&root.join("paged-clear-rollback.json"))
            .unwrap();
        let before = store.clear_inventory_unlocked().unwrap();
        FAIL_NEXT_POST_CLEAR_FINALIZATION.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = store.clear_all_match_data(&preview).unwrap_err();
        assert!(error.contains("exact pre-clear rollback: Ok"), "{error}");
        assert_eq!(store.count(CALIBRATION_SPENT_TABLE).unwrap(), 257);
        let after = store.clear_inventory_unlocked().unwrap();
        assert_eq!(after.state_digest, before.state_digest);
        assert_eq!(after.tables, before.tables);
        close(&root, store);
    }

    #[cfg(unix)]
    #[test]
    fn clear_repairs_named_recovery_after_unix_unlink_race_before_success() {
        let root = workspace("unix-recovery-unlink-race");
        let store = MatchStore::open(&root).unwrap();
        store
            .create_person("Unix recovery race", Vec::new())
            .unwrap();
        let preview = store
            .preview_clear_all_match_data(&root.join("unix-recovery.json"))
            .unwrap();
        UNLINK_RETAINED_RECOVERY_AFTER_CLEAR_COMMIT
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert!(receipt.recovery_bundle.canonical_path.is_file());
        let repaired =
            MatchStore::read_identity_bundle(&receipt.recovery_bundle.canonical_path).unwrap();
        assert_eq!(
            repaired.manifest.content_sha256,
            receipt.recovery_bundle.content_sha256
        );
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn provenance_bearing_clear_preview_executes_with_canonical_reference_count() {
        let root = workspace("suggestion-provenance-clear");
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Suggestion provenance", Vec::new())
            .unwrap();
        seed_face(&store, "provenance-face", true);
        let face: FaceObservation = store
            .require(FACE_TABLE, "provenance-face", "provenance Face")
            .unwrap();
        let timestamp = now();
        let generation = ModelGeneration {
            generation: "match-model-unconfigured".to_string(),
            state: "active".to_string(),
            validated: true,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let job_id = "provenance-clear-job".to_string();
        let asset = JobAsset {
            asset_id: job_asset_id(&job_id, &face.media_key),
            job_id: job_id.clone(),
            media_key: face.media_key.clone(),
            source_path: None,
            media_fingerprint: face.media_fingerprint.clone(),
            next_stage: JobStage::Complete.as_str().to_string(),
            completed_stages: vec![
                JobStage::Discover.as_str().to_string(),
                JobStage::Detect.as_str().to_string(),
                JobStage::Align.as_str().to_string(),
                JobStage::Embed.as_str().to_string(),
                JobStage::Persist.as_str().to_string(),
                JobStage::Suggest.as_str().to_string(),
            ],
            failure_code: None,
            failure_message: None,
            skipped_code: None,
            skipped_message: None,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: generation.generation.clone(),
            identity_revision: 1,
            catalog_revision: person.catalog_revision,
            updated_at: timestamp.clone(),
        };
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&face.face_id, &generation.generation),
            face_id: face.face_id.clone(),
            vector: vec![0.0; EMBEDDING_DIM],
            model_generation: generation.generation.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            job_id: job_id.clone(),
            active: true,
            created_at: timestamp,
        };
        let suggestion = Suggestion {
            suggestion_id: suggestion_id(&face.face_id, &person.person_id),
            face_id: face.face_id.clone(),
            candidate_person_id: person.person_id.clone(),
            similarity: 0.91,
            model_generation: generation.generation.clone(),
            calibration_generation: None,
            envelope_hash: None,
            media_fingerprint: face.media_fingerprint.clone(),
            face_revision: face.face_revision,
            person_revision: person.revision,
            job_id,
            created_at: now(),
        };
        for (table, id, row) in [
            (
                GENERATION_TABLE,
                generation.generation.as_str(),
                serde_json::to_value(&generation).unwrap(),
            ),
            (
                JOB_ASSET_TABLE,
                asset.asset_id.as_str(),
                serde_json::to_value(&asset).unwrap(),
            ),
            (
                EMBEDDING_TABLE,
                embedding.embedding_id.as_str(),
                serde_json::to_value(&embedding).unwrap(),
            ),
            (
                SUGGESTION_TABLE,
                suggestion.suggestion_id.as_str(),
                serde_json::to_value(&suggestion).unwrap(),
            ),
        ] {
            store.upsert_json(table, id, &row).unwrap();
        }
        let fence = store
            .correction_fence_for(&face.face_id, vec![person.person_id.clone()])
            .unwrap();
        store
            .same_correction(&face.face_id, &person.person_id, &fence)
            .unwrap();
        assert_eq!(store.count(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap(), 1);

        let preview = store
            .preview_clear_all_match_data(&root.join("provenance-clear-recovery.json"))
            .unwrap();
        assert_eq!(preview.recovery_bundle.reference_count, 7);
        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert_eq!(
            receipt.recovery_bundle.reference_count,
            preview.recovery_bundle.reference_count
        );
        assert_eq!(store.count(SUGGESTION_SOURCE_PROVENANCE_TABLE).unwrap(), 0);
        close(&root, store);
    }

    #[test]
    fn rebuild_preserves_complete_face_reference_closure_and_removes_every_derived_class() {
        let root = workspace("closure-derived");
        let raw_dir = root.join("raw");
        std::fs::create_dir_all(&raw_dir).unwrap();
        let raw_path = raw_dir.join("source.jpg");
        std::fs::write(&raw_path, b"raw-media-must-survive-match-rebuild").unwrap();
        let raw_hash_before = format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));

        let store = MatchStore::open(&root).unwrap();
        let person = store.create_person("Recovery Person", Vec::new()).unwrap();
        for id in [
            "manual",
            "confirmed",
            "constraint",
            "disposition",
            "trusted",
            "history-direct",
            "history-envelope",
            "history-strict",
            "derived-orphan",
            "strict-face",
        ] {
            seed_face(&store, id, id == "manual");
        }
        store
            .upsert_json(
                ASSIGNMENT_TABLE,
                "confirmed",
                &assignment(
                    "confirmed",
                    &person.person_id,
                    AssignmentState::OperatorConfirmed,
                ),
            )
            .unwrap();
        store
            .upsert_json(
                ASSIGNMENT_TABLE,
                "strict-face",
                &assignment(
                    "strict-face",
                    &person.person_id,
                    AssignmentState::CommittedStrictAutomatic,
                ),
            )
            .unwrap();
        store
            .upsert_json(
                CONSTRAINT_TABLE,
                "constraint-row",
                &CannotLinkConstraint {
                    constraint_id: "constraint-row".to_string(),
                    face_id: "constraint".to_string(),
                    person_id: person.person_id.clone(),
                    operation_id: "constraint-operation".to_string(),
                    operator_owned: true,
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                FACE_DISPOSITION_TABLE,
                "disposition",
                &FaceDisposition {
                    face_id: "disposition".to_string(),
                    media_key: "raw/disposition.jpg".to_string(),
                    disposition: "ignored".to_string(),
                    operation_id: "disposition-operation".to_string(),
                    face_revision: 1,
                    created_at: now(),
                    updated_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                TRUSTED_MEMBER_TABLE,
                "trusted-membership",
                &TrustedTemplateMembership {
                    membership_id: "trusted-membership".to_string(),
                    set_id: "set-recovery".to_string(),
                    look_id: "look-recovery".to_string(),
                    face_id: "trusted".to_string(),
                    authorized: true,
                    alignment_valid: true,
                    quality_passed: true,
                    pose_passed: true,
                    diversity_passed: true,
                    provenance: "operator-authorized".to_string(),
                    model_generation: "model-recovery".to_string(),
                    embedding_id: "embedding-trusted".to_string(),
                    media_fingerprint: "fingerprint-trusted".to_string(),
                    face_revision: 1,
                    quality_score: 0.9,
                    quality_threshold: 0.7,
                    pose_bucket: "frontal".to_string(),
                    policy_version: TRUSTED_POLICY_VERSION.to_string(),
                    operation_id: "trusted-operation".to_string(),
                    created_at: now(),
                },
            )
            .unwrap();
        let mut historical_strict_assignment = assignment(
            "history-strict",
            &person.person_id,
            AssignmentState::CommittedStrictAutomatic,
        );
        historical_strict_assignment.operation_id = "history-strict-operation".to_string();
        historical_strict_assignment.provenance = "strict_recognition_v1".to_string();
        historical_strict_assignment.calibration_generation =
            Some("calibration-recovery".to_string());
        historical_strict_assignment.envelope_hash = Some("a".repeat(64));
        for operation in [
            MatchOperation {
                operation_id: "history-direct-operation".to_string(),
                kind: "correction_not_sure".to_string(),
                face_id: Some("history-direct".to_string()),
                person_id: Some(person.person_id.clone()),
                before_json: "[]".to_string(),
                after_json: serde_json::json!({
                    "version": 1,
                    "kind": "not_sure",
                    "rows": [],
                    "face_ids": ["history-direct"],
                    "media_keys": ["raw/source.jpg"],
                    "identity_changed": false,
                    "catalog_changed": false
                })
                .to_string(),
                reversible: false,
                created_at: now(),
            },
            MatchOperation {
                operation_id: "history-envelope-operation".to_string(),
                kind: "correction_batch_not_sure".to_string(),
                face_id: None,
                person_id: Some(person.person_id.clone()),
                before_json: "[]".to_string(),
                after_json: serde_json::json!({
                    "version": 1,
                    "kind": "batch_not_sure",
                    "rows": [],
                    "face_ids": ["history-envelope"],
                    "media_keys": ["raw/source.jpg"],
                    "identity_changed": false,
                    "catalog_changed": false
                })
                .to_string(),
                reversible: false,
                created_at: now(),
            },
            MatchOperation {
                operation_id: "history-strict-operation".to_string(),
                kind: "assign_committed_strict_automatic".to_string(),
                face_id: Some("history-strict".to_string()),
                person_id: Some(person.person_id.clone()),
                before_json: "null".to_string(),
                after_json: serde_json::to_string(&historical_strict_assignment).unwrap(),
                reversible: true,
                created_at: historical_strict_assignment.created_at.clone(),
            },
        ] {
            store
                .upsert_json(OPERATION_TABLE, &operation.operation_id, &operation)
                .unwrap();
        }

        for id in ["embedding-orphan", "embedding-trusted"] {
            store
                .upsert_json(
                    EMBEDDING_TABLE,
                    id,
                    &FaceEmbedding {
                        embedding_id: id.to_string(),
                        face_id: if id == "embedding-trusted" {
                            "trusted".to_string()
                        } else {
                            "derived-orphan".to_string()
                        },
                        vector: vec![0.0; EMBEDDING_DIM],
                        model_generation: "model-recovery".to_string(),
                        schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                        media_fingerprint: "fingerprint".to_string(),
                        face_revision: 1,
                        job_id: "job-recovery".to_string(),
                        active: true,
                        created_at: now(),
                    },
                )
                .unwrap();
        }
        store
            .upsert_json(
                TRUSTED_SEARCH_TABLE,
                "trusted-membership",
                &TrustedSearchEmbedding {
                    membership_id: "trusted-membership".to_string(),
                    person_id: person.person_id.clone(),
                    look_id: "look-recovery".to_string(),
                    face_id: "trusted".to_string(),
                    embedding_id: "embedding-trusted".to_string(),
                    vector: vec![0.0; EMBEDDING_DIM],
                    model_generation: "model-recovery".to_string(),
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                TRUSTED_INDEX_BUILD_TABLE,
                "global",
                &TrustedIndexBuildReceipt {
                    receipt_id: "global".to_string(),
                    build_digest: "a".repeat(64),
                    source_row_count: 1,
                    engine_version: STRICT_ANN_ENGINE_VERSION.to_string(),
                    build_seed: 0,
                    build_order: STRICT_ANN_BUILD_ORDER.to_string(),
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                SUGGESTION_TABLE,
                "suggestion-recovery",
                &Suggestion {
                    suggestion_id: "suggestion-recovery".to_string(),
                    face_id: "derived-orphan".to_string(),
                    candidate_person_id: person.person_id.clone(),
                    similarity: 0.8,
                    model_generation: "model-recovery".to_string(),
                    calibration_generation: None,
                    envelope_hash: None,
                    media_fingerprint: "fingerprint-derived-orphan".to_string(),
                    face_revision: 1,
                    person_revision: person.revision,
                    job_id: "job-recovery".to_string(),
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                PROJECTION_TABLE,
                "raw/source.jpg",
                &PeopleProjection {
                    media_key: "raw/source.jpg".to_string(),
                    media_fingerprint: "fingerprint-projection".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "model-recovery".to_string(),
                    identity_revision: 1,
                    catalog_revision: 1,
                    person_ids: vec![person.person_id.clone()],
                    published_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                ROOT_CONFIG_TABLE,
                "root-recovery",
                &MatchIndexRoot {
                    root_id: "root-recovery".to_string(),
                    path: raw_dir.to_string_lossy().to_string(),
                    exclusions: vec!["excluded".to_string()],
                    enabled: true,
                    created_at: now(),
                    updated_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                GENERATION_TABLE,
                "model-recovery",
                &ModelGeneration {
                    generation: "model-recovery".to_string(),
                    state: "active".to_string(),
                    validated: true,
                    created_at: now(),
                    updated_at: now(),
                },
            )
            .unwrap();
        let mut activation = CalibrationActivation {
            calibration_generation: "calibration-recovery".to_string(),
            model_generation: "model-recovery".to_string(),
            envelope_hash: "a".repeat(64),
            runtime_configuration_digest: "b".repeat(64),
            activation_integrity_digest: String::new(),
            trusted_index_build_digest: "c".repeat(64),
            gallery_members_digest: "d".repeat(64),
            verifier_artifact_id: "verifier-recovery".to_string(),
            contract_sha256: "e".repeat(64),
            raw_records_sha256: "f".repeat(64),
            evidence_digest: "1".repeat(64),
            review_digest: "2".repeat(64),
            automatic_threshold: 0.9,
            suggestion_threshold: 0.8,
            runner_up_margin: 0.1,
            minimum_quality: 0.7,
            candidate_k: 5,
            rerank_k: 5,
            people_max: 10,
            looks_per_person_max: 2,
            templates_per_look_max: 3,
            total_templates_max: 30,
            verifier_verdict: "pass".to_string(),
            independent_review_verdict: "pass".to_string(),
            wp084_runtime_ready: true,
            wp087_release_ready: true,
            active: true,
            invalidation_reason: None,
            created_at: now(),
            updated_at: now(),
        };
        activation.activation_integrity_digest =
            calibration_activation_integrity_digest(&activation);
        store
            .upsert_json(CALIBRATION_TABLE, "calibration-recovery", &activation)
            .unwrap();
        let configuration_before = [
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(ROOT_CONFIG_TABLE, "root_id")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(GENERATION_TABLE, "generation")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(CALIBRATION_TABLE, "calibration_generation")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
        ];

        let preview = store.preview_rebuild_match_analysis().unwrap();
        assert_eq!(preview.affected.derived_faces, 5);
        assert_eq!(preview.affected.embeddings, 2);
        assert_eq!(preview.affected.trusted_search_embeddings, 1);
        assert_eq!(preview.affected.trusted_index_builds, 1);
        assert_eq!(preview.affected.suggestions, 1);
        assert_eq!(preview.affected.projections, 1);
        assert_eq!(preview.affected.strict_automatic_assignments, 1);
        assert_eq!(preview.affected.machine_clusters, 0);
        assert_eq!(preview.affected.total_rows, 12);
        assert!(preview.manifests.iter().any(|manifest| {
            manifest.table == "match_machine_cluster"
                && manifest.state_class == MatchStateClass::Absent
                && manifest.row_count == 0
        }));

        let receipt = store.rebuild_match_analysis(&preview).unwrap();
        assert_eq!(receipt.kind, "rebuild_match_analysis");
        assert_eq!(receipt.preview_id, preview.preview_id);
        assert_eq!(receipt.changed_rows, 12);
        assert_eq!(receipt.removed, preview.affected);
        assert!(!receipt.raw_media_deleted);
        for id in [
            "manual",
            "confirmed",
            "constraint",
            "disposition",
            "trusted",
        ] {
            assert!(
                store
                    .get_one::<FaceObservation>(FACE_TABLE, id)
                    .unwrap()
                    .is_some(),
                "durable reference closure lost {id}"
            );
        }
        for id in [
            "history-direct",
            "history-envelope",
            "history-strict",
            "derived-orphan",
            "strict-face",
        ] {
            assert!(store
                .get_one::<FaceObservation>(FACE_TABLE, id)
                .unwrap()
                .is_none());
        }
        for operation_id in [
            "history-direct-operation",
            "history-envelope-operation",
            "history-strict-operation",
        ] {
            assert!(
                store
                    .get_one::<MatchOperation>(OPERATION_TABLE, operation_id)
                    .unwrap()
                    .is_some(),
                "rebuild removed durable operation history {operation_id}"
            );
        }
        assert!(store
            .list::<FaceEmbedding>(EMBEDDING_TABLE)
            .unwrap()
            .is_empty());
        assert!(store
            .list::<TrustedSearchEmbedding>(TRUSTED_SEARCH_TABLE)
            .unwrap()
            .is_empty());
        assert_eq!(store.count(TRUSTED_INDEX_BUILD_TABLE).unwrap(), 0);
        assert!(store
            .list::<Suggestion>(SUGGESTION_TABLE)
            .unwrap()
            .is_empty());
        assert!(store
            .list::<PeopleProjection>(PROJECTION_TABLE)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .list::<Assignment>(ASSIGNMENT_TABLE)
                .unwrap()
                .into_iter()
                .map(|assignment| assignment.state)
                .collect::<Vec<_>>(),
            vec![AssignmentState::OperatorConfirmed.as_str().to_string()]
        );
        let configuration_after = [
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(ROOT_CONFIG_TABLE, "root_id")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(GENERATION_TABLE, "generation")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
            manifest_for(
                &store
                    .recovery_table_rows_unlocked(CALIBRATION_TABLE, "calibration_generation")
                    .unwrap(),
                MatchStateClass::Configuration,
                MatchStatePartition::All,
                false,
            )
            .unwrap(),
        ];
        assert_eq!(configuration_after, configuration_before);
        let raw_hash_after = format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));
        assert_eq!(raw_hash_after, raw_hash_before);
        close(&root, store);
    }

    #[test]
    fn clear_requires_verified_unchanged_bundle_survives_restart_and_restores_only_portable_graph()
    {
        let root = workspace("clear-restore");
        let raw_path = root.join("raw-source.jpg");
        std::fs::write(&raw_path, b"raw-media-must-survive-clear-and-restore").unwrap();
        let raw_hash_before = format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));
        let bundle_root = std::env::temp_dir().join(format!(
            "wp085-recovery-portability-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&bundle_root).unwrap();
        let bundle_root = std::fs::canonicalize(bundle_root).unwrap();
        let store = MatchStore::open(&root).unwrap();
        let first_person = store
            .create_person("First Recovery Person", Vec::new())
            .unwrap();
        seed_face(&store, "portable-face", true);
        store
            .upsert_json(
                EMBEDDING_TABLE,
                "excluded-embedding",
                &FaceEmbedding {
                    embedding_id: "excluded-embedding".to_string(),
                    face_id: "portable-face".to_string(),
                    vector: vec![0.0; EMBEDDING_DIM],
                    model_generation: "excluded-generation".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    media_fingerprint: "fingerprint-portable-face".to_string(),
                    face_revision: 1,
                    job_id: "excluded-job".to_string(),
                    active: true,
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                PROJECTION_TABLE,
                "raw-source.jpg",
                &PeopleProjection {
                    media_key: "raw-source.jpg".to_string(),
                    media_fingerprint: "projection-fingerprint".to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: "excluded-generation".to_string(),
                    identity_revision: 1,
                    catalog_revision: 1,
                    person_ids: vec![first_person.person_id.clone()],
                    published_at: now(),
                },
            )
            .unwrap();
        let original_media_root = bundle_root.join("original-media-root");
        std::fs::create_dir_all(&original_media_root).unwrap();
        store
            .upsert_json(
                ROOT_CONFIG_TABLE,
                "portable-root",
                &MatchIndexRoot {
                    root_id: "portable-root".to_string(),
                    path: original_media_root.to_string_lossy().to_string(),
                    exclusions: vec!["excluded".to_string()],
                    enabled: true,
                    created_at: now(),
                    updated_at: now(),
                },
            )
            .unwrap();
        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);

        let tampered_path = bundle_root.join("tampered-recovery.json");
        let tampered = store.preview_clear_all_match_data(&tampered_path).unwrap();
        let mut tampered_bytes = std::fs::read(&tampered_path).unwrap();
        tampered_bytes.push(b'\n');
        std::fs::write(&tampered_path, tampered_bytes).unwrap();
        assert!(store
            .clear_all_match_data(&tampered)
            .unwrap_err()
            .contains("identity bundle"));

        let stale_path = bundle_root.join("stale-recovery.json");
        let stale = store.preview_clear_all_match_data(&stale_path).unwrap();
        store.create_person("State Drift", Vec::new()).unwrap();
        assert_eq!(
            store.clear_all_match_data(&stale).unwrap_err(),
            "recovery bundle does not match the exact current portable Match graph"
        );

        let recovery_path = bundle_root.join("verified-recovery.json");
        let preview = store.preview_clear_all_match_data(&recovery_path).unwrap();
        assert_eq!(preview.recovery_bundle.format, IDENTITY_BUNDLE_FORMAT);
        assert_eq!(preview.recovery_bundle.version, IDENTITY_BUNDLE_VERSION);
        let recovery_bytes = std::fs::read(&recovery_path).unwrap();
        assert_eq!(
            preview.recovery_bundle.canonical_bytes,
            recovery_bytes.len()
        );
        assert_eq!(
            preview.recovery_bundle.file_sha256,
            format!("{:x}", Sha256::digest(&recovery_bytes))
        );
        assert!(!preview.raw_media_deleted);
        assert!(preview.tables.iter().any(|table| {
            table.manifest.table == "match_schema_state"
                && table.disposition == MatchClearDisposition::RetainSchemaAuthority
        }));
        assert!(preview.tables.iter().any(|table| {
            table.manifest.table == EXECUTION_TABLE
                && table.disposition == MatchClearDisposition::ReinitializeExecution
        }));
        assert!(preview.tables.iter().any(|table| {
            table.manifest.table == EMBEDDING_TABLE
                && table.disposition == MatchClearDisposition::DeleteRows
                && table.manifest.row_count == 1
        }));
        let raw_hash_at_preview =
            format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));
        assert_eq!(raw_hash_at_preview, raw_hash_before);

        let confirmation_token = preview.confirmation_token.clone();
        let unrelated_root = workspace("unrelated-clear-bundle");
        let unrelated_store = MatchStore::open(&unrelated_root).unwrap();
        unrelated_store
            .create_person("Unrelated Recovery Person", Vec::new())
            .unwrap();
        unrelated_store
            .set_desired_mode(DesiredMode::OperatorPaused)
            .unwrap();
        let unrelated_path = bundle_root.join("unrelated-recovery.json");
        unrelated_store
            .preview_clear_all_match_data(&unrelated_path)
            .unwrap();
        assert_eq!(
            store
                .clear_all_match_data_file(&unrelated_path, &confirmation_token)
                .unwrap_err(),
            "recovery bundle does not match the exact current portable Match graph"
        );
        close(&unrelated_root, unrelated_store);
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert!(store
            .clear_all_match_data_file(&recovery_path, "wrong-clear-confirmation-token")
            .unwrap_err()
            .contains("confirmation token"));
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 2);
        let receipt = store
            .clear_all_match_data_file(&recovery_path, &confirmation_token)
            .unwrap();
        assert_eq!(receipt.deleted_rows, preview.total_rows_to_delete);
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert_eq!(
            receipt.recovery_bundle.content_sha256,
            preview.recovery_bundle.content_sha256
        );
        assert_eq!(
            receipt.recovery_bundle.file_sha256,
            preview.recovery_bundle.file_sha256
        );
        assert!(!receipt.raw_media_deleted);
        for (table, _, _) in clear_delete_table_specs() {
            assert_eq!(store.count(table).unwrap(), 0, "clear left rows in {table}");
        }
        let raw_hash_after_clear =
            format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));
        assert_eq!(raw_hash_after_clear, raw_hash_before);

        let restore_token = receipt.recovery_bundle.restore_token.clone();
        let moved_bundle_dir = bundle_root.join("moved-bundle-directory");
        std::fs::create_dir_all(&moved_bundle_dir).unwrap();
        let moved_recovery_path = moved_bundle_dir.join("verified-recovery-moved.json");
        std::fs::rename(&recovery_path, &moved_recovery_path).unwrap();
        assert_ne!(
            std::fs::canonicalize(&moved_recovery_path).unwrap(),
            receipt.recovery_bundle.canonical_path
        );
        let relocated_media_root = bundle_root.join("relocated-media-root");
        std::fs::create_dir_all(&relocated_media_root).unwrap();
        let relocations = BTreeMap::from([(
            "portable-root".to_string(),
            relocated_media_root.to_string_lossy().to_string(),
        )]);
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        assert!(store
            .restore_clear_recovery_bundle(
                &moved_recovery_path,
                &relocations,
                "wrong-recovery-token",
            )
            .unwrap_err()
            .contains("restore token mismatch"));
        let restored = store
            .restore_clear_recovery_bundle(&moved_recovery_path, &relocations, &restore_token)
            .unwrap();
        assert_eq!(
            restored.content_sha256,
            receipt.recovery_bundle.content_sha256
        );
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 2);
        assert!(store
            .get_one::<FaceObservation>(FACE_TABLE, "portable-face")
            .unwrap()
            .is_some());
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        assert_eq!(store.count(PROJECTION_TABLE).unwrap(), 0);
        assert_eq!(store.count(JOB_TABLE).unwrap(), 0);
        assert_eq!(store.count(JOB_ASSET_TABLE).unwrap(), 0);
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::OperatorPaused);
        let rebuild = store.preview_rebuild_match_analysis().unwrap();
        assert_eq!(rebuild.affected.embeddings, 0);
        assert_eq!(rebuild.affected.projections, 0);
        let raw_hash_after_restore =
            format!("{:x}", Sha256::digest(std::fs::read(&raw_path).unwrap()));
        assert_eq!(raw_hash_after_restore, raw_hash_before);
        close(&root, store);
        std::fs::remove_dir_all(&bundle_root).unwrap();
    }

    #[test]
    #[ignore = "real model/worker proof; requires FACIAL_WP085_FACE_FIXTURE and FACIAL_WP085_FACE_FIXTURE_SHA256"]
    fn wp085_clear_restore_regenerates_embeddings_from_raw_media_worker() {
        use crate::{config::AppConfig, identity::IdentityEngine, service::FacialService};

        let fixture = PathBuf::from(
            std::env::var_os("FACIAL_WP085_FACE_FIXTURE")
                .expect("explicit real-worker proof requires a positive face fixture"),
        );
        let expected_hash = std::env::var("FACIAL_WP085_FACE_FIXTURE_SHA256")
            .expect("explicit real-worker proof requires the verified fixture SHA256");
        validate_sha256("fixture SHA256", &expected_hash).unwrap();
        let raw = std::fs::read(&fixture).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&raw)), expected_hash);
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("w600k_r50.onnx");
        assert!(model.is_file(), "real inference model must be provisioned");
        let root = workspace("real-worker-clear-restore");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let extension = fixture
            .extension()
            .and_then(|value| value.to_str())
            .unwrap();
        let source = media_root.join(format!("positive-face.{extension}"));
        std::fs::write(&source, &raw).unwrap();
        drop(raw);
        let manifest = root.join(".facial/models/match-inference-manifest-v1.json");
        let engine = IdentityEngine::provision(&model, None, &manifest).unwrap();
        let generation = engine.generation().to_string();
        drop(engine);
        let config = AppConfig {
            settings_path_override: None,
            repo_root: root.clone(),
            workspace_root: root.clone(),
            worktrees_root: root.join("worktrees"),
            model_registry_path: root.join("data/model_registry.json"),
            debug_log_path: root.join("data/events.jsonl"),
            plugins_root: root.join("plugins"),
            api_root: root.join("data/api"),
            ingest_in_place_default: false,
            max_debug_events: 50,
            font_size_pt: 19.0,
            copy_location: None,
            identity_model_path: None,
            identity_detector_path: None,
            identity_manifest_path: Some(manifest),
            identity_reference_dir: None,
            identity_negative_dir: None,
            identity_threshold: 0.5,
            identity_margin: 0.1,
            identity_count_threshold: 0.9,
            framing_closeup_min: 0.09,
            framing_threequarter_min: 0.03,
            theme_mode: "paper".to_string(),
            landmark_model_path: None,
            media_thumb_cache_mb: 64,
        };
        let service = FacialService::new(config.clone());
        let configured = service
            .match_configure_root(&media_root.to_string_lossy(), Vec::new())
            .unwrap();
        let root_id = configured["root_id"].as_str().unwrap().to_string();
        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Recovery worker fixture", vec!["Fixture alias".to_string()])
            .unwrap();
        store
            .create_look(&person.person_id, "Fixture look")
            .unwrap();
        // Read the operator-owned rows directly, independently of bundle producers.
        let operator_graph = |store: &MatchStore| {
            [
                PERSON_TABLE,
                LOOK_TABLE,
                ASSIGNMENT_TABLE,
                CONSTRAINT_TABLE,
                OPERATION_TABLE,
            ]
            .into_iter()
            .map(|table| {
                let mut rows = store.list::<Value>(table).unwrap();
                rows.sort_by_key(Value::to_string);
                (table, rows)
            })
            .collect::<BTreeMap<_, _>>()
        };
        let original_graph = operator_graph(&store);
        let run_worker = |service: &FacialService, store: &MatchStore| {
            store.set_desired_mode(DesiredMode::Running).unwrap();
            let job = service.match_start_job(&root_id).unwrap();
            let job_id = job["job_id"].as_str().unwrap().to_string();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                let current = store.job(&job_id).unwrap();
                match current.lifecycle().unwrap() {
                    JobLifecycle::Completed => break,
                    JobLifecycle::Failed | JobLifecycle::Partial | JobLifecycle::Cancelled => {
                        panic!(
                            "real worker failed: {:?}; assets: {:?}",
                            current,
                            store.job_assets(&job_id)
                        );
                    }
                    _ => {}
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "real worker timed out: {current:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let embeddings = store.list::<FaceEmbedding>(EMBEDDING_TABLE).unwrap();
            assert!(
                !embeddings.is_empty(),
                "positive fixture produced no embeddings"
            );
            for embedding in &embeddings {
                assert_eq!(embedding.job_id, job_id);
                assert_eq!(embedding.model_generation, generation);
                assert_eq!(embedding.vector.len(), EMBEDDING_DIM);
                assert!(embedding.vector.iter().all(|value| value.is_finite()));
                assert!(embedding.vector.iter().any(|value| value.abs() > 0.001));
                let face: FaceObservation = store
                    .require(FACE_TABLE, &embedding.face_id, "worker Face")
                    .unwrap();
                assert!(!face.operator_owned);
                assert!(face.alignment_valid);
                assert_eq!(face.media_fingerprint, embedding.media_fingerprint);
            }
            (job_id, embeddings)
        };
        let (before_job, before_embeddings) = run_worker(&service, &store);
        assert_eq!(operator_graph(&store), original_graph);
        drop(service);
        let recovery_path = root.join("verified-recovery.json");
        let preview = store.preview_clear_all_match_data(&recovery_path).unwrap();
        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        assert_eq!(store.count(PERSON_TABLE).unwrap(), 0);
        drop(store);
        surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
        let store = MatchStore::open(&root).unwrap();
        let relocations =
            BTreeMap::from([(root_id.clone(), media_root.to_string_lossy().to_string())]);
        store
            .restore_clear_recovery_bundle(
                &recovery_path,
                &relocations,
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap();
        assert_eq!(
            store.count(EMBEDDING_TABLE).unwrap(),
            0,
            "restore must not synthesize or restore vectors"
        );
        assert_eq!(operator_graph(&store), original_graph);
        let service = FacialService::new(config);
        let (after_job, after_embeddings) = run_worker(&service, &store);
        assert_ne!(after_job, before_job);
        assert_eq!(after_embeddings.len(), before_embeddings.len());
        for regenerated in &after_embeddings {
            let prior = before_embeddings
                .iter()
                .find(|prior| prior.face_id == regenerated.face_id)
                .expect("same raw face must retain its stable identity");
            assert_eq!(prior.media_fingerprint, regenerated.media_fingerprint);
            assert!(
                prior
                    .vector
                    .iter()
                    .zip(&regenerated.vector)
                    .all(|(left, right)| (left - right).abs() < 0.0001),
                "same hash-pinned model and raw media must regenerate the same vector"
            );
        }
        assert_eq!(operator_graph(&store), original_graph);
        assert_eq!(
            format!("{:x}", Sha256::digest(std::fs::read(&source).unwrap())),
            expected_hash
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(std::fs::read(&fixture).unwrap())),
            expected_hash
        );
        drop(service);
        close(&root, store);
    }

    #[test]
    fn wp085_clear_restore_allows_independent_regeneration_of_excluded_index_state() {
        let root = workspace("clear-restore-regenerate");
        let bundle_root = std::env::temp_dir().join(format!(
            "wp085-recovery-regeneration-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let media_root = bundle_root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let media_root = std::fs::canonicalize(media_root).unwrap();
        let recovery_path = bundle_root.join("verified-recovery.json");

        let store = MatchStore::open(&root).unwrap();
        let person = store
            .create_person("Regenerated Person", Vec::new())
            .unwrap();
        let generation = "model-recovery-regeneration";
        store.register_model_generation(generation, true).unwrap();
        let configured_root = store.configure_index_root(&media_root, Vec::new()).unwrap();

        seed_face(&store, "pre-clear-derived", false);
        let pre_clear_face: FaceObservation = store
            .require(FACE_TABLE, "pre-clear-derived", "pre-clear derived Face")
            .unwrap();
        store
            .upsert_json(
                EMBEDDING_TABLE,
                "pre-clear-excluded-embedding",
                &FaceEmbedding {
                    embedding_id: "pre-clear-excluded-embedding".to_string(),
                    face_id: pre_clear_face.face_id.clone(),
                    vector: {
                        let mut vector = vec![0.0; EMBEDDING_DIM];
                        vector[0] = 1.0;
                        vector
                    },
                    model_generation: generation.to_string(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    media_fingerprint: pre_clear_face.media_fingerprint.clone(),
                    face_revision: pre_clear_face.face_revision,
                    job_id: "pre-clear-job".to_string(),
                    active: true,
                    created_at: now(),
                },
            )
            .unwrap();
        store
            .upsert_json(
                PROJECTION_TABLE,
                &pre_clear_face.media_key,
                &PeopleProjection {
                    media_key: pre_clear_face.media_key.clone(),
                    media_fingerprint: pre_clear_face.media_fingerprint.clone(),
                    schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
                    model_generation: generation.to_string(),
                    identity_revision: 1,
                    catalog_revision: person.catalog_revision,
                    person_ids: vec![person.person_id.clone()],
                    published_at: now(),
                },
            )
            .unwrap();

        let preview = store.preview_clear_all_match_data(&recovery_path).unwrap();
        let receipt = store.clear_all_match_data(&preview).unwrap();
        assert_eq!(store.count(FACE_TABLE).unwrap(), 0);
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        assert_eq!(store.count(PROJECTION_TABLE).unwrap(), 0);

        let relocations = BTreeMap::from([(
            configured_root.root_id.clone(),
            media_root.to_string_lossy().to_string(),
        )]);
        store
            .restore_clear_recovery_bundle(
                &recovery_path,
                &relocations,
                &receipt.recovery_bundle.restore_token,
            )
            .unwrap();
        assert!(store
            .get_one::<Person>(PERSON_TABLE, &person.person_id)
            .unwrap()
            .is_some());
        assert!(store
            .get_one::<ModelGeneration>(GENERATION_TABLE, generation)
            .unwrap()
            .is_some());
        assert_eq!(
            store.index_root(&configured_root.root_id).unwrap().path,
            media_root.to_string_lossy()
        );
        assert_eq!(store.count(FACE_TABLE).unwrap(), 1);
        assert_eq!(
            store
                .get_one::<FaceObservation>(FACE_TABLE, &pre_clear_face.face_id)
                .unwrap(),
            Some(pre_clear_face.clone())
        );
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 0);
        assert_eq!(store.count(PROJECTION_TABLE).unwrap(), 0);

        // Match's real start-job service activates the restored usable model
        // before creating the job/fence; projection writes require it active.
        store.activate_model_generation(generation).unwrap();
        store.set_desired_mode(DesiredMode::Running).unwrap();
        let job = store
            .start_index_job(&configured_root.root_id, generation)
            .unwrap();
        let job = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let media_key = "regenerated/source.jpg";
        let media_fingerprint = format!(
            "{:x}",
            Sha256::digest(b"independently-regenerated-match-analysis")
        );
        let asset = store
            .enqueue_asset(&job.job_id, media_key, &media_fingerprint)
            .unwrap();
        let fence = RevisionFence {
            job_id: job.job_id.clone(),
            media_key: media_key.to_string(),
            media_fingerprint: media_fingerprint.clone(),
            schema_generation: job.schema_generation.clone(),
            model_generation: job.model_generation.clone(),
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
        };
        let coordinator = MediaIoCoordinator::new();
        let acquire = |stage| {
            store
                .acquire_background_stage(
                    &coordinator,
                    RootIdentity::new("wp085-recovery-regeneration", 1, RootKind::Local),
                    &fence,
                    stage,
                    ResourceRequest {
                        admitted_items: 1,
                        queued_items: 1,
                        queued_bytes: 1024 * 1024,
                        cpu_inference: u64::from(matches!(
                            stage,
                            JobStage::Detect
                                | JobStage::Align
                                | JobStage::Embed
                                | JobStage::Suggest
                        )),
                        decoded_bytes: if matches!(stage, JobStage::Detect | JobStage::Align) {
                            8 * 1024 * 1024
                        } else {
                            0
                        },
                        surreal_writes: 1,
                        ..ResourceRequest::default()
                    },
                )
                .unwrap()
        };

        let permit = acquire(JobStage::Discover);
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Discover, &fence, &permit)
            .unwrap();

        let mut regenerated_face = face("pending-derived-id", media_key, false);
        regenerated_face.media_fingerprint = media_fingerprint.clone();
        regenerated_face.source_index = 7;
        let permit = acquire(JobStage::Detect);
        let regenerated_face = store
            .create_derived_face(regenerated_face, &fence, &permit)
            .unwrap();
        let permit = acquire(JobStage::Detect);
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Detect, &fence, &permit)
            .unwrap();
        let permit = acquire(JobStage::Align);
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Align, &fence, &permit)
            .unwrap();

        let mut vector = vec![0.0; EMBEDDING_DIM];
        vector[7] = 1.0;
        let regenerated_embedding = FaceEmbedding {
            embedding_id: embedding_id(&regenerated_face.face_id, generation),
            face_id: regenerated_face.face_id.clone(),
            vector: vector.clone(),
            model_generation: generation.to_string(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            media_fingerprint: media_fingerprint.clone(),
            face_revision: regenerated_face.face_revision,
            job_id: job.job_id.clone(),
            active: true,
            created_at: now(),
        };
        let permit = acquire(JobStage::Embed);
        store
            .put_embedding(regenerated_embedding.clone(), &fence, &permit)
            .unwrap();
        let permit = acquire(JobStage::Embed);
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Embed, &fence, &permit)
            .unwrap();

        let regenerated_projection = PeopleProjection {
            media_key: media_key.to_string(),
            media_fingerprint: media_fingerprint.clone(),
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            model_generation: generation.to_string(),
            identity_revision: fence.identity_revision,
            catalog_revision: fence.catalog_revision,
            person_ids: vec![person.person_id.clone()],
            published_at: now(),
        };
        let permit = acquire(JobStage::Persist);
        store
            .publish_projection(regenerated_projection.clone(), &fence, &permit)
            .unwrap();
        let permit = acquire(JobStage::Persist);
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Persist, &fence, &permit)
            .unwrap();
        for stage in [JobStage::Suggest, JobStage::Complete] {
            let permit = acquire(stage);
            store
                .commit_asset_stage(&asset.asset_id, stage, &fence, &permit)
                .unwrap();
        }

        assert_ne!(regenerated_face.face_id, pre_clear_face.face_id);
        assert_eq!(store.count(FACE_TABLE).unwrap(), 2);
        assert_eq!(store.count(EMBEDDING_TABLE).unwrap(), 1);
        assert_eq!(store.count(PROJECTION_TABLE).unwrap(), 1);
        assert_eq!(
            store
                .get_one::<FaceEmbedding>(EMBEDDING_TABLE, &regenerated_embedding.embedding_id,)
                .unwrap(),
            Some(regenerated_embedding)
        );
        assert_eq!(
            store.cached_projection(media_key).unwrap(),
            Some(regenerated_projection)
        );
        let neighbors = store.nearest(&vector, generation, 8, 8).unwrap();
        assert_eq!(neighbors.candidate_count, 1);
        assert_eq!(neighbors.neighbors[0].face_id, regenerated_face.face_id);
        assert_eq!(store.job(&job.job_id).unwrap().completed, 1);
        assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());

        close(&root, store);
        std::fs::remove_dir_all(&bundle_root).unwrap();
    }
}
