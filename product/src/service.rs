use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use chrono::Utc;
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    config::AppConfig,
    debug::{DebugBus, DebugEvent},
    identity::{verify_match_calibration, IdentityEngine},
    lanes::{
        LaneBatchAggregate, LaneBatchResult, LaneMode, LaneRecord, LaneScanResult, LaneStore,
        LaneUpdate,
    },
    models::{IngestResult, ModelRecord, PluginRunResult, RunSummary},
    plugin_host::PluginHost,
};

#[cfg(test)]
#[path = "match_cpu_throughput_tests.rs"]
mod match_cpu_throughput_tests;
#[cfg(test)]
#[path = "match_pipeline_tests.rs"]
mod match_pipeline_tests;
#[path = "service_match_video.rs"]
mod match_video_jobs;

const MATCH_MAX_SOURCE_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MATCH_DECODED_WORKING_BYTES_PER_PIXEL: u64 = 24;
const MATCH_MAX_DECODED_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const MATCH_RESOURCE_BACKOFF_INITIAL_MS: u64 = 10;
const MATCH_RESOURCE_BACKOFF_MAX_MS: u64 = 250;

fn match_maintenance_value<T: serde::Serialize>(value: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

/// Lift the deliberately truth-free XMP parser receipt into the durable GUI
/// receipt contract. XMP IDs and names remain hints: each region needs a fresh
/// canonical media read and an explicit manual-face correction before it can
/// affect Match identity state.
fn xmp_import_staging_contract(
    action_id: &str,
    api_root: &Path,
    receipt: crate::match_store::MwgXmpImportReceipt,
) -> Result<serde_json::Value, String> {
    if receipt.applied_match_rows != 0 || receipt.original_media_rows_mutated != 0 {
        return Err(
            "XMP staging import violated its zero-Match-truth/zero-original-mutation boundary"
                .to_string(),
        );
    }
    let staged_region_count = receipt.staged_regions.len();
    let region_mappings = receipt
        .staged_regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            json!({
                "staged_region_index": index,
                "status": "awaiting_operator_mapping",
                "xmp_identity_hints_only": {
                    "face_id": region.face_id,
                    "person_id": region.person_id,
                    "name": region.name,
                },
                "open_media_faces_intent": {
                    "kind": "match_intent",
                    "action": "open_media_faces",
                    "id_source": "operator_selected_canonical_media_key",
                },
                "manual_face_correction": {
                    "kind": "match_correction",
                    "action": "manual_face",
                    "face_ids": [],
                    "field_values_from_staging": {
                        "normalized_bounds": {
                            "left": region.bounds_normalized[0],
                            "top": region.bounds_normalized[1],
                            "width": region.bounds_normalized[2],
                            "height": region.bounds_normalized[3],
                        }
                    },
                    "required_field_sources": {
                        "person_id": "operator_selected_existing_or_created_person_id_or_null",
                        "media_key": "open_media_faces.request.id",
                        "media_fingerprint": "open_media_faces.terminal_result.media_fingerprint",
                        "normalized_bounds.source_width": "open_media_faces.terminal_result.source_geometry.source_width",
                        "normalized_bounds.source_height": "open_media_faces.terminal_result.source_geometry.source_height",
                        "exif_orientation": "open_media_faces.terminal_result.source_geometry.exif_orientation",
                        "expected_revisions.schema_generation": "open_media_faces.terminal_result.schema_generation",
                        "expected_revisions.model_generation": "open_media_faces.terminal_result.model_generation",
                        "expected_revisions.catalog_revision": "open_media_faces.terminal_result.catalog_revision",
                        "expected_revisions.person_revisions": "fresh canonical Person revision when person_id is present; otherwise empty",
                        "expected_revisions.face_revisions": "empty_for_manual_face",
                    },
                    "confirmed": false,
                }
            })
        })
        .collect::<Vec<_>>();
    let receipt_relative_path = format!("receipts/{action_id}.json");
    let audit_relative_path = format!("intents/applied/{action_id}.json");
    Ok(json!({
        "kind": "xmp_region_staging",
        "contract_version": 1,
        "stage_status": "staged_only",
        "match_truth_applied": false,
        "identity_truth_applied": false,
        "source": {
            "sidecar_path": receipt.sidecar_path,
            "content_sha256": receipt.content_sha256,
            "preview_token": receipt.preview_token,
        },
        "staged_region_count": staged_region_count,
        "staged_regions": receipt.staged_regions,
        "applied_match_rows": receipt.applied_match_rows,
        "original_media_rows_mutated": receipt.original_media_rows_mutated,
        "staging_artifact": {
            "action_id": action_id,
            "terminal_status": "applied",
            "receipt_relative_path": receipt_relative_path,
            "receipt_path": api_root.join("receipts").join(format!("{action_id}.json")),
            "audit_relative_path": audit_relative_path,
            "result_pointer": "/result",
            "restart_behavior": "reuse_terminal_receipt_without_reapplying_sidecar",
        },
        "next_action_contract": {
            "kind": "xmp_region_manual_mapping",
            "contract_version": 1,
            "one_manual_face_correction_per_region": true,
            "xmp_identity_values_are_hints_only": true,
            "person_resolution": {
                "existing": {"kind": "match_intent", "action": "open_people"},
                "create": {"kind": "match_intent", "action": "create_person"},
                "rule": "operator_must_select_or_create_the_canonical_person;_never_apply_xmp_person_id_or_name_as_truth",
            },
            "regions": region_mappings,
        }
    }))
}

fn verify_person_preview_fence(
    request: &crate::api::MatchCorrectionRequest,
    preview: &crate::match_store::PersonEditPreview,
) -> Result<(), String> {
    if request.operation_id.as_deref() != Some(preview.preview_id.as_str())
        || preview.catalog_revision != request.expected_revisions.catalog_revision
        || preview.person_revisions != request.expected_revisions.person_revisions
    {
        return Err("stale Match Person-operation preview fence".to_string());
    }
    if !request.face_ids.is_empty() {
        let mut expected = request.face_ids.clone();
        expected.sort();
        if preview.face_ids != expected {
            return Err("Person-operation preview FaceIds differ from the request".to_string());
        }
    }
    Ok(())
}

fn match_batch_action(
    action: crate::api::MatchCorrectionAction,
) -> Option<crate::match_store::BatchCorrectionAction> {
    use crate::api::MatchCorrectionAction as Action;
    use crate::match_store::BatchCorrectionAction as Batch;
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

fn normalized_region_iou(left: crate::match_store::NormalizedRegion, right: [f32; 4]) -> f32 {
    let x0 = left.x.max(right[0]);
    let y0 = left.y.max(right[1]);
    let x1 = (left.x + left.width).min(right[0] + right[2]);
    let y1 = (left.y + left.height).min(right[1] + right[3]);
    let intersection = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = left.width * left.height + right[2] * right[3] - intersection;
    if union > 0.0 {
        intersection / union
    } else {
        0.0
    }
}

fn manual_embedding_admitted(
    landmark_validity: Option<crate::landmarks::ManualLandmarkValidity>,
    detector_alignment_valid: bool,
) -> bool {
    landmark_validity.is_some_and(crate::landmarks::ManualLandmarkValidity::is_valid)
        && detector_alignment_valid
}

fn source_point_to_display(
    x: f32,
    y: f32,
    orientation: crate::match_store::ExifOrientation,
) -> (f32, f32) {
    use crate::match_store::ExifOrientation as Orientation;
    match orientation {
        Orientation::Normal => (x, y),
        Orientation::MirrorHorizontal => (1.0 - x, y),
        Orientation::Rotate180 => (1.0 - x, 1.0 - y),
        Orientation::MirrorVertical => (x, 1.0 - y),
        Orientation::Transpose => (y, x),
        Orientation::Rotate90Clockwise => (1.0 - y, x),
        Orientation::Transverse => (1.0 - y, 1.0 - x),
        Orientation::Rotate90CounterClockwise => (y, 1.0 - x),
    }
}

fn source_region_to_display(
    region: crate::match_store::NormalizedRegion,
    orientation: crate::match_store::ExifOrientation,
) -> crate::match_store::NormalizedRegion {
    let corners = [
        source_point_to_display(region.x, region.y, orientation),
        source_point_to_display(region.x + region.width, region.y, orientation),
        source_point_to_display(region.x, region.y + region.height, orientation),
        source_point_to_display(
            region.x + region.width,
            region.y + region.height,
            orientation,
        ),
    ];
    let min_x = corners.iter().map(|point| point.0).fold(1.0, f32::min);
    let max_x = corners.iter().map(|point| point.0).fold(0.0, f32::max);
    let min_y = corners.iter().map(|point| point.1).fold(1.0, f32::min);
    let max_y = corners.iter().map(|point| point.1).fold(0.0, f32::max);
    crate::match_store::NormalizedRegion {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

fn manual_authority_error_code(error: &str) -> &'static str {
    if error.contains("disabled") {
        "root_disabled"
    } else if error.contains("no canonical JobAsset") {
        "job_asset_missing"
    } else if error.contains("no canonical source path") {
        "source_path_missing"
    } else if error.contains("stale") {
        "stale_fingerprint"
    } else if error.contains("readable media fingerprint") {
        "unreadable_fingerprint"
    } else {
        "authority_unavailable"
    }
}

/// True for file extensions the batch identity gate will decode.
fn is_image_ext(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff")
    )
}

/// Quote a CSV field if it contains a comma, quote, or newline (RFC 4180).
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render one gate row object as a CSV line matching the batch header order.
fn gate_csv_line(row: &serde_json::Value) -> String {
    let field = |k: &str| -> String {
        match &row[k] {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        }
    };
    let bx = |k: &str| -> String {
        match &row["face_box"] {
            serde_json::Value::Object(m) => m.get(k).map(|v| v.to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    };
    let cols = [
        field("image"),
        field("verdict"),
        field("source"),
        field("reference_similarity"),
        field("negative_similarity"),
        field("margin"),
        field("face_count"),
        bx("x"),
        bx("y"),
        bx("w"),
        bx("h"),
        field("face_frac"),
        field("face_score"),
        field("framing"),
        field("face_crop_sharpness"),
        field("yaw_estimate"),
        field("yaw_ratio"),
        field("hair_color"),
        field("hair_confidence"),
        field("eyes_open"),
        field("ear_left"),
        field("ear_right"),
        field("landmark_conf_min"),
        field("image_w"),
        field("image_h"),
        field("align"),
        field("error"),
    ];
    let line: Vec<String> = cols.iter().map(|c| csv_escape(c)).collect();
    format!("{}\n", line.join(","))
}

fn slugify(value: &str) -> String {
    let mut out = value.to_lowercase();
    out = out
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "project".to_string()
    } else {
        let cleaned = Regex::new(r"-+")
            .unwrap()
            .replace_all(&out, "-")
            .to_string();
        cleaned.trim_matches('-').to_string()
    }
}

struct ModelRegistry {
    path: PathBuf,
    items: HashMap<String, ModelRecord>,
    models: Vec<ModelRecord>,
}

impl ModelRegistry {
    fn load(path: PathBuf, debug: &mut DebugBus) -> Self {
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(payload) => {
                        let mut items = HashMap::new();
                        let mut models = Vec::new();
                        let list = payload
                            .get("models")
                            .and_then(|value| value.as_array())
                            .cloned()
                            .unwrap_or_default();
                        for item in list {
                            if let Ok(record) = serde_json::from_value::<ModelRecord>(item) {
                                items.insert(record.id.clone(), record.clone());
                                models.push(record);
                            }
                        }
                        return Self {
                            path,
                            items,
                            models,
                        };
                    }
                    Err(err) => {
                        debug.emit(
                            "WARN",
                            "ModelRegistry",
                            &format!("model registry parse error: {err}"),
                            None,
                        );
                    }
                },
                Err(err) => {
                    debug.emit(
                        "WARN",
                        "ModelRegistry",
                        &format!("model registry read error: {err}"),
                        None,
                    );
                }
            }
        }
        let mut reg = Self {
            path,
            items: HashMap::new(),
            models: Vec::new(),
        };
        reg.ensure_defaults(debug);
        reg
    }

    fn ensure_defaults(&mut self, debug: &mut DebugBus) {
        if self.items.is_empty() {
            let seed = ModelRecord {
                id: "face-selection-combined".to_string(),
                name: "facial default".to_string(),
                description: "Default entry for headshot and model tooling bundling.".to_string(),
                source_path: "".to_string(),
                status: "active".to_string(),
                tags: vec!["starter".to_string(), "combined".to_string()],
            };
            let _ = self.add(seed, debug);
        }
    }

    fn persist(&self, debug: &mut DebugBus) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let payload = json!({
            "models": self.models
        });
        let _ = fs::write(
            &self.path,
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
        debug.emit("INFO", "ModelRegistry", "model registry persisted", None);
    }

    fn add(&mut self, record: ModelRecord, debug: &mut DebugBus) -> bool {
        if self.items.contains_key(&record.id) {
            return false;
        }
        self.items.insert(record.id.clone(), record.clone());
        self.models.push(record);
        self.persist(debug);
        true
    }

    fn list(&self) -> Vec<ModelRecord> {
        let mut list = self.models.clone();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    fn by_id(&self, id: &str) -> Option<ModelRecord> {
        self.items.get(id).cloned()
    }

    fn remove(&mut self, id: &str, debug: &mut DebugBus) -> bool {
        if self.items.remove(id).is_some() {
            self.models.retain(|m| m.id != id);
            self.persist(debug);
            true
        } else {
            false
        }
    }
}

struct WorktreeManager {
    root: PathBuf,
}

impl WorktreeManager {
    fn create(&self, project_name: &str) -> io::Result<PathBuf> {
        let slug = slugify(project_name);
        let run_id = Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let run_slug = Uuid::new_v4().to_string()[..8].to_string();
        let target = self.root.join(&slug).join(format!("{run_id}_{run_slug}"));
        fs::create_dir_all(&target)?;
        Ok(target)
    }
}

struct MatchStoreRuntime {
    workspace_root: PathBuf,
    store: Option<crate::match_store::MatchStore>,
    error_code: Option<String>,
    initializing: bool,
    external_holds: Arc<crate::match_store::MatchExternalHolds>,
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    active_index_jobs: std::collections::BTreeSet<String>,
    pending_index_jobs: std::collections::BTreeSet<String>,
    index_workers: std::collections::BTreeMap<String, std::thread::JoinHandle<()>>,
    last_cpu_executor_acknowledgement: Option<crate::match_worker::CpuExecutorAcknowledgement>,
}

impl MatchStoreRuntime {
    fn dormant(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            store: None,
            error_code: None,
            initializing: false,
            external_holds: Arc::new(crate::match_store::MatchExternalHolds::default()),
            cancelled: Arc::new(AtomicBool::new(false)),
            worker: None,
            active_index_jobs: std::collections::BTreeSet::new(),
            pending_index_jobs: std::collections::BTreeSet::new(),
            index_workers: std::collections::BTreeMap::new(),
            last_cpu_executor_acknowledgement: None,
        }
    }
}

fn match_store_runtime(workspace_root: PathBuf) -> Arc<Mutex<MatchStoreRuntime>> {
    Arc::new(Mutex::new(MatchStoreRuntime::dormant(workspace_root)))
}

fn match_init_error_code(error: &str) -> &'static str {
    if error.contains("incompatible Match schema marker") {
        "match_schema_incompatible"
    } else {
        "match_store_initialization_failed"
    }
}

fn start_match_store_initialization(runtime: &Arc<Mutex<MatchStoreRuntime>>) -> Result<(), String> {
    let (workspace_root, cancelled) = {
        let mut state = runtime
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
        if state.store.is_some() || state.error_code.is_some() || state.initializing {
            return Ok(());
        }
        state.initializing = true;
        (state.workspace_root.clone(), Arc::clone(&state.cancelled))
    };
    let worker_runtime = Arc::clone(&runtime);
    let spawn_result = std::thread::Builder::new()
        .name("facial-match-store-init".to_string())
        .spawn(move || {
            if cancelled.load(Ordering::Acquire) {
                if let Ok(mut state) = worker_runtime.lock() {
                    state.initializing = false;
                }
                return;
            }
            let result = crate::match_store::MatchStore::open(&workspace_root);
            let Ok(mut state) = worker_runtime.lock() else {
                return;
            };
            if cancelled.load(Ordering::Acquire) {
                state.initializing = false;
                return;
            }
            state.initializing = false;
            match result {
                Ok(store) => {
                    state.store =
                        Some(store.with_external_holds(Arc::clone(&state.external_holds)));
                }
                Err(error) => {
                    state.error_code = Some(match_init_error_code(&error).to_string());
                }
            }
        });
    match spawn_result {
        Ok(worker) => {
            let mut state = runtime
                .lock()
                .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
            state.worker = Some(worker);
        }
        Err(_) => {
            if let Ok(mut state) = runtime.lock() {
                state.initializing = false;
                state.error_code = Some("match_store_worker_spawn_failed".to_string());
            }
        }
    }
    Ok(())
}

fn cancel_match_store_runtime(
    runtime: &Arc<Mutex<MatchStoreRuntime>>,
) -> Vec<std::thread::JoinHandle<()>> {
    if let Ok(mut state) = runtime.lock() {
        state.cancelled.store(true, Ordering::Release);
        state.store = None;
        state.initializing = false;
        let mut workers = state.worker.take().into_iter().collect::<Vec<_>>();
        workers.extend(std::mem::take(&mut state.index_workers).into_values());
        state.active_index_jobs.clear();
        state.pending_index_jobs.clear();
        workers
    } else {
        Vec::new()
    }
}

/// Destructive identity maintenance is admitted only after the ordinary Match
/// pause flow has let every worker settle. Silently cancelling a worker would
/// strand a durable job in `running`; allowing one to continue could republish
/// stale derived rows after a rebuild or clear.
fn require_match_index_workers_quiescent(
    runtime: &Arc<Mutex<MatchStoreRuntime>>,
) -> Result<(), String> {
    let (store, workers_present) = {
        let state = runtime
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
        if state.initializing {
            return Err(
                "Match store must be initialized before maintenance quiescence".to_string(),
            );
        }
        let store = state.store.clone().ok_or_else(|| {
            "Match store must be initialized before maintenance quiescence".to_string()
        })?;
        let workers_present = !state.active_index_jobs.is_empty()
            || !state.pending_index_jobs.is_empty()
            || !state.index_workers.is_empty();
        (store, workers_present)
    };
    if store.desired_mode()? != crate::match_store::DesiredMode::OperatorPaused {
        return Err(
            "Match maintenance requires Pause all (persisted operator_paused desired mode)"
                .to_string(),
        );
    }
    if workers_present {
        return Err(
            "Match maintenance requires Pause all and no active/pending index workers".to_string(),
        );
    }
    Ok(())
}

/// Complete one worker iteration while holding the sole runtime mutex. A
/// pending wake is consumed without dropping the active ownership token; when
/// no wake exists, active ownership and the join handle are retired together.
/// Producers therefore observe either an active worker they can wake or no
/// active worker they can create, never the lost-wake gap between two locks.
fn settle_match_worker_iteration(runtime: &mut MatchStoreRuntime, job_id: &str) -> bool {
    if runtime.pending_index_jobs.remove(job_id) && !runtime.cancelled.load(Ordering::Acquire) {
        true
    } else {
        runtime.active_index_jobs.remove(job_id);
        runtime.index_workers.remove(job_id);
        false
    }
}

fn match_stage_request(
    stage: crate::match_store::JobStage,
    queued_bytes: u64,
    decoded_bytes: u64,
) -> crate::match_store::ResourceRequest {
    use crate::match_store::JobStage;
    crate::match_store::ResourceRequest {
        worker_memory_bytes: 0,
        admitted_items: 1,
        queued_items: 1,
        queued_bytes: queued_bytes.max(64 * 1024),
        cpu_inference: u64::from(matches!(
            stage,
            JobStage::Detect | JobStage::Align | JobStage::Embed | JobStage::Suggest
        )),
        decoded_bytes: if matches!(stage, JobStage::Detect | JobStage::Align) {
            decoded_bytes
        } else {
            0
        },
        gpu_vram_bytes: 0,
        surreal_writes: 1,
        vector_index_builds: 0,
    }
}

fn match_snapshot_resource_request(queued_bytes: u64) -> crate::match_store::ResourceRequest {
    crate::match_store::ResourceRequest {
        admitted_items: 1,
        queued_items: 1,
        queued_bytes: queued_bytes.max(1),
        ..crate::match_store::ResourceRequest::default()
    }
}

fn match_revision_fence(
    job: &crate::match_store::IndexJob,
    asset: &crate::match_store::JobAsset,
) -> crate::match_store::RevisionFence {
    crate::match_store::RevisionFence {
        job_id: job.job_id.clone(),
        media_key: asset.media_key.clone(),
        media_fingerprint: asset.media_fingerprint.clone(),
        schema_generation: job.schema_generation.clone(),
        model_generation: job.model_generation.clone(),
        identity_revision: job.identity_revision,
        catalog_revision: job.catalog_revision,
    }
}

fn match_worker_fence(
    job: &crate::match_store::IndexJob,
    asset: Option<&crate::match_store::JobAsset>,
    admission_epoch: u64,
) -> crate::match_worker::WorkerFence {
    crate::match_worker::WorkerFence {
        job_id: job.job_id.clone(),
        asset_id: asset
            .map(|asset| asset.asset_id.clone())
            .unwrap_or_default(),
        media_key: asset
            .map(|asset| asset.media_key.clone())
            .unwrap_or_default(),
        media_fingerprint: asset
            .map(|asset| asset.media_fingerprint.clone())
            .unwrap_or_default(),
        schema_generation: job.schema_generation.clone(),
        model_generation: job.model_generation.clone(),
        identity_revision: job.identity_revision,
        catalog_revision: job.catalog_revision,
        admission_epoch,
        track_id: None,
        timestamp_ms: None,
    }
}

fn match_compute_request(
    queued_bytes: u64,
    decoded_bytes: u64,
) -> crate::match_store::ResourceRequest {
    let mut request = match_stage_request(
        crate::match_store::JobStage::Detect,
        queued_bytes,
        decoded_bytes,
    );
    request.surreal_writes = 0;
    request.vector_index_builds = 0;
    request
}

fn match_cpu_compute_request(
    policy: crate::match_worker::CpuExecutionPolicy,
    queued_bytes: u64,
    decoded_bytes: u64,
) -> crate::match_store::ResourceRequest {
    let mut request = match_compute_request(queued_bytes, decoded_bytes);
    request.cpu_inference = policy.active_units();
    request
}

fn match_admission_interrupted(error: &str) -> bool {
    error.contains("paused, held, or terminal")
        || error.contains("worker admission paused or held")
        || error.contains("worker cancelled")
        || error.contains("admission changed")
        || error.contains("admission epoch changed")
}

fn match_database_outcome_unknown(error: &str) -> bool {
    error.contains("commit_outcome_unknown")
}

fn match_database_requires_recovery(error: &str) -> bool {
    match_database_outcome_unknown(error)
        || error.contains("database_owner_exit_pending")
        || error.contains("database_owner_epoch_changed")
        || (error.contains("safe_unit_timeout") && error.contains("database"))
}

/// Compute leases are released or retained by the owned Job exit reaper before
/// entering this bounded cleanup/receipt path. A failed child cannot grant itself publication rights.
fn schedule_confirmed_match_worker_exit(
    store: crate::match_store::MatchStore,
    worker_id: String,
    observe_exit: crate::match_worker::OwnedWorkerExitObserver,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("match-worker-exit-receipt".into())
        .spawn(move || loop {
            match observe_exit() {
                Ok(true) => {
                    if let Err(error) = store.acknowledge_worker_exit(&worker_id) {
                        eprintln!("Match worker exit receipt failed: {error}");
                    }
                    break;
                }
                Ok(false) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(error) => {
                    // Leave the durable row unconfirmed when observation fails.
                    eprintln!("Match owned worker exit observation failed: {error}");
                    break;
                }
            }
        })
        .map_err(|error| format!("could not supervise Match worker exit receipt: {error}"))
}

fn settle_match_worker_failure(
    store: &crate::match_store::MatchStore,
    job: &crate::match_store::IndexJob,
    fence: Option<&crate::match_store::RevisionFence>,
    worker: &crate::match_worker::IsolatedMatchWorker,
    error: &crate::match_worker::WorkerError,
) -> Result<(), String> {
    let code = match error.code.as_str() {
        "safe_unit_timeout" => "safe_unit_timeout",
        "worker_stale_result" => "stale_worker_result",
        "worker_generation_mismatch" | "worker_invalid_output" => "worker_protocol",
        _ => "worker_failed",
    };
    store
        .record_worker_quarantine(
            &job.job_id,
            fence,
            worker.worker_id(),
            &job.model_generation,
            code,
            &error.message,
            worker.confirmed_dead(),
        )
        .map_err(|failure| format!("{code}: worker quarantine persistence failed: {failure}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while !worker.confirmed_dead() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if worker.confirmed_dead() {
        store
            .acknowledge_worker_exit(worker.worker_id())
            .map_err(|failure| format!("{code}: worker exit receipt failed: {failure}"))?;
    } else {
        let observer = worker
            .owned_exit_observer()
            .map_err(|failure| format!("{code}: {}: {}", failure.code, failure.message))?;
        schedule_confirmed_match_worker_exit(store.clone(), worker.worker_id().into(), observer)?;
    }
    Err(format!(
        "{code}: isolated Match worker quarantined; explicit retry requires confirmed exit"
    ))
}

struct MatchFileSnapshot {
    fingerprint: String,
    final_path: PathBuf,
    bytes: Vec<u8>,
}

fn match_source_fingerprint_matches(snapshot: &str, persisted: &str) -> bool {
    crate::match_store::canonical_media_sha256(snapshot)
        .is_some_and(|digest| crate::match_store::canonical_media_sha256(persisted) == Some(digest))
}

fn match_file_snapshot(path: &Path, expected_root: &Path) -> Result<MatchFileSnapshot, String> {
    let expected_path = path
        .canonicalize()
        .map_err(|error| format!("canonicalize Match source for hashing: {error}"))?;
    if !expected_path.starts_with(expected_root) {
        return Err("Match source escaped its configured root before hashing".to_string());
    }
    let mut file = fs::File::open(&expected_path)
        .map_err(|error| format!("open Match source for hashing: {error}"))?;
    let opened_path = crate::identity::opened_file_final_path(&file, "match_source")?;
    if opened_path != expected_path || !opened_path.starts_with(expected_root) {
        return Err("Match source handle identity changed outside its configured root".to_string());
    }
    let before = file
        .metadata()
        .map_err(|error| format!("read Match source metadata before hashing: {error}"))?;
    if !before.is_file() || before.len() > MATCH_MAX_SOURCE_FILE_BYTES {
        return Err(format!(
            "Match source exceeds the bounded {} byte snapshot limit",
            MATCH_MAX_SOURCE_FILE_BYTES
        ));
    }
    let capacity = usize::try_from(before.len())
        .map_err(|_| "Match source length does not fit this runtime".to_string())?;
    let mut bytes = Vec::with_capacity(capacity);
    {
        let mut bounded = (&mut file).take(MATCH_MAX_SOURCE_FILE_BYTES + 1);
        bounded
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read Match source for hashing: {error}"))?;
    }
    if bytes.len() as u64 > MATCH_MAX_SOURCE_FILE_BYTES {
        return Err(format!(
            "Match source exceeds the bounded {} byte snapshot limit",
            MATCH_MAX_SOURCE_FILE_BYTES
        ));
    }
    let after = file
        .metadata()
        .map_err(|error| format!("read Match source metadata after hashing: {error}"))?;
    let final_path = crate::identity::opened_file_final_path(&file, "match_source")?;
    if final_path != opened_path || before.len() != after.len() || after.len() != bytes.len() as u64
    {
        return Err("Match source changed while hashing".to_string());
    }
    Ok(MatchFileSnapshot {
        // Portable identity and manual-correction provenance use the raw,
        // lowercase SHA256; this exact snapshot also feeds indexed Face rows.
        fingerprint: format!("{:x}", Sha256::digest(&bytes)),
        final_path,
        bytes,
    })
}

/// Read only the owned snapshot's image header and conservatively charge the
/// decoder, its largest supported dynamic pixel representation, the RGB8
/// inference image, and kernel scratch headroom before any pixel allocation.
/// The per-image ceiling keeps two concurrent CPU kernels within the default
/// 512 MiB aggregate decoded-image budget.
fn match_decoded_working_bytes(encoded: &[u8], source_path: &Path) -> Result<u64, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(encoded))
        .with_guessed_format()
        .map_err(|error| {
            format!(
                "inspect Match image format before decode for {}: {error}",
                source_path.display()
            )
        })?;
    let (width, height) = reader.into_dimensions().map_err(|error| {
        format!(
            "inspect Match image dimensions before decode for {}: {error}",
            source_path.display()
        )
    })?;
    if width == 0 || height == 0 {
        return Err("Match decoded image dimensions must be non-zero".to_string());
    }
    let working_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(MATCH_DECODED_WORKING_BYTES_PER_PIXEL))
        .ok_or_else(|| "Match decoded image dimensions overflow the resource budget".to_string())?;
    if working_bytes > MATCH_MAX_DECODED_WORKING_BYTES {
        return Err(format!(
            "Match decoded image requires {working_bytes} working bytes, exceeding the bounded {MATCH_MAX_DECODED_WORKING_BYTES} byte per-image limit"
        ));
    }
    Ok(working_bytes)
}

fn retry_match_resource_pressure<T>(
    cancelled: &AtomicBool,
    mut can_continue: impl FnMut() -> Result<bool, String>,
    mut operation: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut backoff_ms = MATCH_RESOURCE_BACKOFF_INITIAL_MS;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("Match indexing worker cancelled".to_string());
        }
        if !can_continue()? {
            return Err("Match stage admission is paused, held, or terminal".to_string());
        }
        match operation() {
            Err(error) if error == "resource_pressure" => {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms
                    .saturating_mul(2)
                    .min(MATCH_RESOURCE_BACKOFF_MAX_MS);
            }
            result => return result,
        }
    }
}

fn retry_admitted_match_discovery_write<T>(
    cancelled: &AtomicBool,
    mut operation: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut backoff_ms = MATCH_RESOURCE_BACKOFF_INITIAL_MS;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("Match indexing worker cancelled".to_string());
        }
        match operation() {
            Err(error) if error == "resource_pressure" => {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms
                    .saturating_mul(2)
                    .min(MATCH_RESOURCE_BACKOFF_MAX_MS);
            }
            result => return result,
        }
    }
}

/// Wait for shared background filesystem capacity, then admit one discovery
/// observation immediately before the filesystem operation starts. Returning
/// `None` means pause/hold/cancellation won the race while the I/O request was
/// queued, so the caller must not touch the filesystem.
fn begin_match_discovery_io(
    store: &crate::match_store::MatchStore,
    job_id: &str,
    coordinator: &crate::media_io::MediaIoCoordinator,
    root_identity: &crate::media_io::RootIdentity,
    cancelled: &AtomicBool,
) -> Result<
    Option<(
        crate::media_io::IoPermit,
        crate::match_store::MatchDiscoveryObservation,
    )>,
    String,
> {
    use crate::media_io::{PermitOutcome, WorkClass};

    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let io = coordinator
        .enqueue(root_identity.clone(), WorkClass::Background)
        .wait()
        .map_err(|error| format!("wait for Match discovery filesystem admission: {error}"))?;
    if !match_worker_can_continue(store, job_id, cancelled)? {
        io.finish(PermitOutcome::Cancelled);
        return Ok(None);
    }
    match store.begin_discovery_observation(job_id) {
        Ok(observation) => Ok(Some((io, observation))),
        Err(error)
            if error.contains("paused, held, or terminal")
                || error.contains("observation is paused") =>
        {
            io.finish(PermitOutcome::Cancelled);
            Ok(None)
        }
        Err(error) => {
            io.finish(PermitOutcome::Error);
            Err(error)
        }
    }
}

/// Each discovery operation uses the child watchdog while retaining its original
/// observation token. Pausing may settle that token; no new operation starts.
fn admitted_match_discovery_worker<T>(
    store: &crate::match_store::MatchStore,
    job: &crate::match_store::IndexJob,
    worker: &mut crate::match_worker::IsolatedMatchWorker,
    coordinator: &crate::media_io::MediaIoCoordinator,
    root: &crate::media_io::RootIdentity,
    cancelled: &AtomicBool,
    request_bytes: u64,
    action: impl FnOnce(
        &mut crate::match_worker::IsolatedMatchWorker,
        &crate::match_worker::WorkerFence,
    ) -> Result<T, crate::match_worker::WorkerError>,
) -> Result<
    (
        T,
        crate::match_store::MatchDiscoveryObservation,
        crate::match_store::MatchResourceLease,
    ),
    String,
> {
    use crate::media_io::PermitOutcome;
    let (resources, request_resources) = retry_match_resource_pressure(
        cancelled,
        || match_worker_can_continue(store, &job.job_id, cancelled),
        || {
            let resources = store.acquire_discovery_snapshot_resources(
                match_snapshot_resource_request(crate::match_discovery::CURSOR_BYTES),
            )?;
            let extra = request_bytes.saturating_sub(crate::match_discovery::CURSOR_BYTES);
            let request_resources = if extra > 0 {
                Some(
                    store
                        .governor()
                        .try_acquire(crate::match_store::ResourceRequest {
                            queued_bytes: extra,
                            ..Default::default()
                        })?,
                )
            } else {
                None
            };
            // Failed extra admission drops the cursor lease before retrying.
            Ok((resources, request_resources))
        },
    )?;
    let Some((io, observation)) =
        begin_match_discovery_io(store, &job.job_id, coordinator, root, cancelled)?
    else {
        return Err("Match discovery admission is paused, held, or terminal".into());
    };
    let fence = match_worker_fence(job, None, store.external_admission_epoch());
    let result = action(worker, &fence);
    if let Some(request_resources) = request_resources {
        worker.finish_compute_resources(request_resources);
    }
    io.finish(if result.is_ok() {
        PermitOutcome::Success
    } else {
        PermitOutcome::Error
    });
    match result {
        Ok(value) => Ok((value, observation, resources)),
        Err(error) => {
            worker.finish_compute_resources(resources);
            if error.quarantined {
                settle_match_worker_failure(store, job, None, worker, &error)?;
            }
            Err(format!("{}: {}", error.code, error.message))
        }
    }
}

fn admitted_match_file_snapshot(
    store: &crate::match_store::MatchStore,
    job_id: &str,
    source_path: &Path,
    expected_root: &Path,
    coordinator: &crate::media_io::MediaIoCoordinator,
    root_identity: &crate::media_io::RootIdentity,
    cancelled: &AtomicBool,
) -> Result<MatchFileSnapshot, String> {
    use crate::media_io::PermitOutcome;

    let Some((io, _observation)) =
        begin_match_discovery_io(store, job_id, coordinator, root_identity, cancelled)?
    else {
        return Err("Match stage admission is paused, held, or terminal".to_string());
    };
    let snapshot = match_file_snapshot(source_path, expected_root);
    io.finish(if snapshot.is_ok() {
        PermitOutcome::Success
    } else {
        PermitOutcome::Error
    });
    snapshot
}

fn match_media_key(root_id: &str, relative: &Path) -> String {
    let relative_text = relative.to_string_lossy().replace('\\', "/");
    let digest = format!("{:x}", Sha256::digest(relative_text.as_bytes()));
    let extension = relative
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "image".to_string());
    format!("{root_id}/{digest}.{extension}")
}

fn record_match_walk_failure(
    store: &crate::match_store::MatchStore,
    discovery_permit: &crate::match_store::MatchDiscoveryPermit,
    job_id: &str,
    root_id: &str,
    root_path: &Path,
    error_path: Option<&Path>,
    error: &str,
) -> Result<String, String> {
    let sanitized_error = error.replace('\r', " ").replace('\n', " ");
    let relative = error_path
        .and_then(|path| path.strip_prefix(root_path).ok())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            let digest = format!("{:x}", Sha256::digest(sanitized_error.as_bytes()));
            PathBuf::from(format!("walk-error-{}.entry", &digest[..16]))
        });
    let relative_text = relative.to_string_lossy().replace('\\', "/");
    let media_key = match_media_key(root_id, &relative);
    store.record_discovery_failure(
        job_id,
        &media_key,
        error_path,
        "io",
        &format!("{relative_text}: walk Match indexing source: {sanitized_error}"),
        discovery_permit,
    )?;
    Ok(media_key)
}

fn match_path_is_excluded(relative: &Path, exclusions: &[String]) -> bool {
    crate::match_discovery::is_excluded(relative, exclusions)
}

fn match_failure_code(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("safe_unit_timeout") {
        "safe_unit_timeout"
    } else if lower.starts_with("worker_failed") {
        "worker_failed"
    } else if lower.starts_with("worker_protocol") {
        "worker_protocol"
    } else if lower.starts_with("stale_worker_result") {
        "stale_worker_result"
    } else if lower.contains("decode") || lower.contains("image") {
        "decode"
    } else if lower.contains("detect") || lower.contains("face") {
        "detect"
    } else if lower.contains("align") {
        "align"
    } else if lower.contains("embed") || lower.contains("model") {
        "embed"
    } else if lower.contains("suggest") || lower.contains("strict") {
        "suggest"
    } else if lower.contains("persist") || lower.contains("surreal") {
        "persist"
    } else if lower.contains("resource_pressure") {
        "resource_pressure"
    } else if lower.contains("io") || lower.contains("read") || lower.contains("open") {
        "io"
    } else {
        "internal"
    }
}

fn match_worker_can_continue(
    store: &crate::match_store::MatchStore,
    job_id: &str,
    cancelled: &AtomicBool,
) -> Result<bool, String> {
    use crate::match_store::JobLifecycle;
    let _database_unit = store.begin_database_unit()?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    let job = store.job(job_id)?;
    if job.lifecycle()? == JobLifecycle::Pausing {
        store.set_job_lifecycle(job_id, JobLifecycle::Paused)?;
        return Ok(false);
    }
    store.can_attempt_automatic(job.lifecycle()?)
}

fn run_match_asset(
    store: &crate::match_store::MatchStore,
    job: &crate::match_store::IndexJob,
    asset: &crate::match_store::JobAsset,
    source_path: &Path,
    expected_root: &Path,
    worker: &mut crate::match_worker::IsolatedMatchWorker,
    coordinator: &crate::media_io::MediaIoCoordinator,
    root_identity: &crate::media_io::RootIdentity,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    use crate::match_store::{FaceEmbedding, FaceObservation, JobStage, PeopleProjection};

    if crate::media_explorer::is_video_path(&source_path.to_string_lossy()) {
        return match_video_jobs::run_match_video_asset(
            store,
            job,
            asset,
            source_path,
            expected_root,
            worker,
            coordinator,
            root_identity,
            cancelled,
        );
    }

    const STAGE_WRITE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
    let fence = match_revision_fence(job, asset);
    let acquire = |stage, queued_bytes, decoded_bytes| {
        retry_match_resource_pressure(
            cancelled,
            || match_worker_can_continue(store, &job.job_id, cancelled),
            || {
                store.acquire_background_stage(
                    coordinator,
                    root_identity.clone(),
                    &fence,
                    stage,
                    match_stage_request(stage, queued_bytes, decoded_bytes),
                )
            },
        )
    };

    let mut stage = asset.next_stage()?;
    if stage == JobStage::Discover {
        let permit = acquire(JobStage::Discover, 64 * 1024, 0)?;
        store.commit_asset_stage(&asset.asset_id, JobStage::Discover, &fence, &permit)?;
        stage = JobStage::Detect;
    }

    let timestamp = crate::match_store::now();
    let mut persisted_faces: Vec<FaceObservation> = Vec::new();
    if stage == JobStage::Detect {
        // The runtime identity kernel is fused, so admit every resource stage
        // it performs before entering the kernel and publish all three durable
        // stage transitions atomically. A restart therefore sees either Detect
        // still pending or Persist next; it can never replay a claimed-complete
        // Detect/Align/Embed stage to reconstruct ephemeral vectors.
        const FUSED_OUTPUT_BUDGET_BYTES: u64 = STAGE_WRITE_BUDGET_BYTES;
        let snapshot = match match_video_jobs::image_asset_snapshot(
            store,
            job,
            asset,
            worker,
            source_path,
            expected_root,
            coordinator,
            root_identity,
            cancelled,
        ) {
            Ok(snapshot)
                if snapshot.final_path == source_path
                    && match_source_fingerprint_matches(
                        &snapshot.fingerprint,
                        &asset.media_fingerprint,
                    ) =>
            {
                snapshot
            }
            Ok(_) => {
                let permit = acquire(JobStage::Detect, FUSED_OUTPUT_BUDGET_BYTES, 1)?;
                store.record_asset_failure(
                    &asset.asset_id,
                    "io",
                    "Match source identity or fingerprint changed before inference",
                    &fence,
                    &permit,
                )?;
                return Ok(());
            }
            Err(error) => {
                if match_admission_interrupted(&error)
                    || store.job(&job.job_id)?.lifecycle()?
                        == crate::match_store::JobLifecycle::Failed
                {
                    return Err(error);
                }
                let permit = acquire(JobStage::Detect, FUSED_OUTPUT_BUDGET_BYTES, 1)?;
                store.record_asset_failure(
                    &asset.asset_id,
                    match_failure_code(&error),
                    &error,
                    &fence,
                    &permit,
                )?;
                return Ok(());
            }
        };
        let header_permit = retry_match_resource_pressure(
            cancelled,
            || match_worker_can_continue(store, &job.job_id, cancelled),
            || {
                store.acquire_worker_compute(
                    coordinator,
                    root_identity.clone(),
                    &job.job_id,
                    Some(&fence),
                    match_compute_request(MATCH_MAX_SOURCE_FILE_BYTES, 1),
                    &mut None,
                )
            },
        )?;
        let header_fence = match_worker_fence(job, Some(asset), header_permit.admission_epoch());
        let header = worker.inspect_pinned_image(&snapshot.final_path, &header_fence);
        header_permit.finish_after_worker(
            if header.is_ok() {
                crate::media_io::PermitOutcome::Success
            } else {
                crate::media_io::PermitOutcome::Error
            },
            worker,
        );
        let header = match header {
            Ok(header) => header,
            Err(error) if error.quarantined => {
                return settle_match_worker_failure(store, job, Some(&fence), worker, &error)
            }
            Err(error) => {
                let permit = acquire(JobStage::Detect, FUSED_OUTPUT_BUDGET_BYTES, 1)?;
                store.record_asset_failure(
                    &asset.asset_id,
                    "decode",
                    &error.message,
                    &fence,
                    &permit,
                )?;
                return Ok(());
            }
        };
        if cancelled.load(Ordering::Acquire)
            || store.external_admission_epoch() != header_fence.admission_epoch
        {
            return Err("Match admission changed after isolated header inspection".into());
        }
        let compute_permit = retry_match_resource_pressure(
            cancelled,
            || match_worker_can_continue(store, &job.job_id, cancelled),
            || {
                store.acquire_worker_compute(
                    coordinator,
                    root_identity.clone(),
                    &job.job_id,
                    Some(&fence),
                    match_cpu_compute_request(
                        worker.cpu_policy(),
                        header.file_size.max(FUSED_OUTPUT_BUDGET_BYTES),
                        header.working_bytes,
                    ),
                    &mut None,
                )
            },
        )?;
        let worker_fence = match_worker_fence(job, Some(asset), compute_permit.admission_epoch());
        let source_exif_orientation = header.exif_orientation;
        if cancelled.load(Ordering::Acquire)
            || store.external_admission_epoch() != worker_fence.admission_epoch
        {
            compute_permit.finish(crate::media_io::PermitOutcome::Cancelled);
            return Err("Match admission changed before isolated inference".into());
        }
        let result = worker.embed_pinned_image(&snapshot.final_path, &worker_fence);
        compute_permit.finish_after_worker(
            if result.is_ok() {
                crate::media_io::PermitOutcome::Success
            } else {
                crate::media_io::PermitOutcome::Error
            },
            worker,
        );
        if let Err(error) = &result {
            if error.quarantined {
                return settle_match_worker_failure(store, job, Some(&fence), worker, error);
            }
        }
        if cancelled.load(Ordering::Acquire)
            || store.external_admission_epoch() != worker_fence.admission_epoch
            || !match_worker_can_continue(store, &job.job_id, cancelled)?
        {
            return Err(
                "Match worker cancelled or admission changed before publication".to_string(),
            );
        }
        // Inference owns no database writer lease. Acquire bounded publication
        // resources only after the child has returned and every fence agrees.
        let detect_permit = acquire(JobStage::Detect, FUSED_OUTPUT_BUDGET_BYTES, 1)?;
        let align_permit = store.acquire_fused_inference_audit_stage(
            &fence,
            JobStage::Align,
            FUSED_OUTPUT_BUDGET_BYTES,
        )?;
        let embed_permit = store.acquire_fused_inference_audit_stage(
            &fence,
            JobStage::Embed,
            FUSED_OUTPUT_BUDGET_BYTES,
        )?;
        let batch = match result {
            Ok(batch) => batch,
            Err(error) if error.code == "missing_face" => {
                align_permit.finish(crate::media_io::PermitOutcome::Success);
                embed_permit.finish(crate::media_io::PermitOutcome::Success);
                store.record_asset_skipped(
                    &asset.asset_id,
                    "no_face",
                    "no detectable face met the governed detector threshold",
                    &fence,
                    &detect_permit,
                )?;
                return Ok(());
            }
            Err(error) => {
                align_permit.finish(crate::media_io::PermitOutcome::Error);
                embed_permit.finish(crate::media_io::PermitOutcome::Error);
                store.record_asset_failure(
                    &asset.asset_id,
                    match_failure_code(&error.code),
                    &format!("{}: {}", error.code, error.message),
                    &fence,
                    &detect_permit,
                )?;
                return Ok(());
            }
        };
        if batch.faces.is_empty() {
            align_permit.finish(crate::media_io::PermitOutcome::Success);
            embed_permit.finish(crate::media_io::PermitOutcome::Success);
            store.record_asset_skipped(
                &asset.asset_id,
                "no_face",
                "no detectable face met the governed detector threshold",
                &fence,
                &detect_permit,
            )?;
            return Ok(());
        }
        let source_width = batch.image_w;
        let source_height = batch.image_h;
        let mut embeddings = Vec::with_capacity(batch.faces.len());
        for (source_index, detected) in batch.faces.into_iter().enumerate() {
            let source_index = source_index as u32;
            let pose_bucket = if detected.alignment_valid {
                crate::identity::yaw_bucket(&detected.landmarks)
                    .0
                    .to_string()
            } else {
                "invalid".to_string()
            };
            let face_id = crate::match_store::derived_face_id(
                &asset.media_key,
                &asset.media_fingerprint,
                source_index,
                &job.schema_generation,
            );
            let face = FaceObservation {
                face_id: face_id.clone(),
                media_key: asset.media_key.clone(),
                media_fingerprint: asset.media_fingerprint.clone(),
                source_index,
                source_width: Some(source_width),
                source_height: Some(source_height),
                exif_orientation: Some(source_exif_orientation),
                bounds_normalized: detected.bbox_normalized.to_vec(),
                landmarks_normalized: detected
                    .landmarks_normalized
                    .into_iter()
                    .map(|point| point.to_vec())
                    .collect(),
                alignment_valid: detected.alignment_valid,
                quality: detected.detection_score,
                pose_bucket,
                operator_owned: false,
                schema_generation: job.schema_generation.clone(),
                face_revision: 1,
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            };
            embeddings.push(FaceEmbedding {
                embedding_id: crate::match_store::embedding_id(&face_id, &job.model_generation),
                face_id,
                vector: detected.embedding,
                model_generation: job.model_generation.clone(),
                schema_generation: job.schema_generation.clone(),
                media_fingerprint: asset.media_fingerprint.clone(),
                face_revision: face.face_revision,
                job_id: job.job_id.clone(),
                active: true,
                created_at: timestamp.clone(),
            });
            persisted_faces.push(face);
        }
        persisted_faces = store.commit_fused_identity_inference(
            persisted_faces,
            embeddings,
            &fence,
            &detect_permit,
            &align_permit,
            &embed_permit,
        )?;
        stage = JobStage::Persist;
    }

    if matches!(stage, JobStage::Align | JobStage::Embed) {
        return Err(
            "legacy non-atomic Match inference cursor requires schema migration".to_string(),
        );
    }

    if stage == JobStage::Persist {
        let permit = acquire(JobStage::Persist, STAGE_WRITE_BUDGET_BYTES, 0)?;
        store.publish_projection(
            PeopleProjection {
                media_key: asset.media_key.clone(),
                media_fingerprint: asset.media_fingerprint.clone(),
                schema_generation: job.schema_generation.clone(),
                model_generation: job.model_generation.clone(),
                identity_revision: job.identity_revision,
                catalog_revision: job.catalog_revision,
                person_ids: Vec::new(),
                published_at: timestamp.clone(),
            },
            &fence,
            &permit,
        )?;
        stage = JobStage::Suggest;
    }

    if stage == JobStage::Suggest {
        if persisted_faces.is_empty() {
            persisted_faces = store.derived_faces_for_asset(asset)?;
        }
        if store.has_active_strict_calibration(&job.model_generation)? {
            for face in &persisted_faces {
                let permit = acquire(JobStage::Suggest, STAGE_WRITE_BUDGET_BYTES, 0)?;
                store.recognize_and_persist_strict(&face.face_id, &fence, &permit)?;
            }
        }
        let permit = acquire(JobStage::Suggest, STAGE_WRITE_BUDGET_BYTES, 0)?;
        store.commit_asset_stage(&asset.asset_id, JobStage::Suggest, &fence, &permit)?;
        stage = JobStage::Complete;
    }
    if stage == JobStage::Complete {
        let permit = acquire(JobStage::Complete, STAGE_WRITE_BUDGET_BYTES, 0)?;
        store.commit_asset_stage(&asset.asset_id, JobStage::Complete, &fence, &permit)?;
    }
    Ok(())
}

fn run_match_index_job(
    store: crate::match_store::MatchStore,
    job_id: String,
    worker: &mut Option<crate::match_worker::IsolatedMatchWorker>,
    manifest_path: &Path,
    coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    cancelled: Arc<AtomicBool>,
    cpu_policy: crate::match_worker::CpuExecutionPolicy,
) -> Result<(), String> {
    run_match_index_job_with_cpu_policy(
        store,
        job_id,
        worker,
        manifest_path,
        coordinator,
        cancelled,
        cpu_policy,
    )
}

fn run_match_index_job_with_cpu_policy(
    store: crate::match_store::MatchStore,
    job_id: String,
    worker: &mut Option<crate::match_worker::IsolatedMatchWorker>,
    manifest_path: &Path,
    coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    cancelled: Arc<AtomicBool>,
    cpu_policy: crate::match_worker::CpuExecutionPolicy,
) -> Result<(), String> {
    use crate::match_store::JobLifecycle;
    use crate::media_io::{PermitOutcome, RootIdentity, RootKind};

    let mut job = store.job(&job_id)?;
    if job.lifecycle()? == JobLifecycle::Retrying {
        job = store.set_job_lifecycle(&job_id, JobLifecycle::Running)?;
    }
    if !match_worker_can_continue(&store, &job_id, &cancelled)? {
        return Ok(());
    }
    let root = store.index_root(&job.root_key)?;
    let root_path = PathBuf::from(&root.path);
    let root_identity = RootIdentity::new(
        root.root_id.clone(),
        job.identity_revision,
        RootKind::Unknown,
    );
    if worker.as_ref().is_some_and(|child| {
        child.cpu_policy() != cpu_policy || !child.is_prepared_for(&job.model_generation)
    }) {
        if !worker.as_mut().unwrap().shutdown_and_confirm() {
            return Err("worker policy or generation change requires confirmed exit".into());
        }
        worker.take();
    }
    if worker
        .as_ref()
        .is_none_or(|worker| !worker.is_prepared_for(&job.model_generation))
    {
        if let Some(prior) = worker.as_mut() {
            prior.release_dead_preparation_resources();
        }
        if crate::match_store::WORKER_MEMORY_LIMIT_BYTES == 0 {
            return Err("worker_memory_policy_unset".into());
        }
        let preparation_buffers = retry_match_resource_pressure(
            &cancelled,
            || match_worker_can_continue(&store, &job_id, &cancelled),
            || {
                store
                    .governor()
                    .try_acquire(crate::match_store::ResourceRequest {
                        queued_bytes: crate::identity::PREPARATION_BUFFER_BYTES,
                        worker_memory_bytes: crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
                        ..Default::default()
                    })
            },
        )?;
        let compute = retry_match_resource_pressure(
            &cancelled,
            || match_worker_can_continue(&store, &job_id, &cancelled),
            || {
                store.acquire_worker_compute(
                    &coordinator,
                    root_identity.clone(),
                    &job_id,
                    None,
                    match_cpu_compute_request(cpu_policy, 64 * 1024, 1),
                    &mut None,
                )
            },
        )?;
        let admitted = match_worker_fence(&job, None, compute.admission_epoch());
        let fresh = crate::match_worker::IsolatedMatchWorker::spawn_with_resources(
            preparation_buffers,
            worker.as_ref(),
        );
        *worker = Some(fresh.map_err(|error| format!("worker_failed: {}", error.message))?);
        if cancelled.load(Ordering::Acquire)
            || store.external_admission_epoch() != admitted.admission_epoch
        {
            compute.finish(PermitOutcome::Cancelled);
            worker.take();
            return Ok(());
        }
        let compute = if cpu_policy == crate::match_worker::CpuExecutionPolicy::PrivateTwoThread {
            let child = worker.as_mut().ok_or("isolated Match worker absent")?;
            let initialized = child.initialize_production_cpu(cpu_policy, &compute, &admitted);
            compute.finish_after_worker(
                if initialized.is_ok() {
                    PermitOutcome::Success
                } else {
                    PermitOutcome::Error
                },
                child,
            );
            if let Err(error) = initialized {
                if error.quarantined {
                    return settle_match_worker_failure(&store, &job, None, child, &error);
                }
                worker.take();
                return Err(format!("{}: {}", error.code, error.message));
            }
            match retry_match_resource_pressure(
                &cancelled,
                || match_worker_can_continue(&store, &job_id, &cancelled),
                || {
                    store.acquire_worker_compute(
                        &coordinator,
                        root_identity.clone(),
                        &job_id,
                        None,
                        match_cpu_compute_request(cpu_policy, 64 * 1024, 1),
                        &mut None,
                    )
                },
            ) {
                Ok(compute) => compute,
                Err(error) => {
                    worker.take();
                    return Err(error);
                }
            }
        } else {
            // Baseline keeps its original preparation permit without an init operation.
            compute
        };
        if cancelled.load(Ordering::Acquire)
            || compute.admission_epoch() != admitted.admission_epoch
            || store.external_admission_epoch() != compute.admission_epoch()
        {
            compute.finish(PermitOutcome::Cancelled);
            worker.take();
            return Ok(());
        }
        let child = worker.as_mut().ok_or("isolated Match worker absent")?;
        let prepared = child.begin_preparation(
            manifest_path,
            crate::match_acceleration::ProbeRuntime::Cpu,
            false,
            &admitted,
        );
        compute.finish_after_worker(
            if prepared.is_ok() {
                PermitOutcome::Success
            } else {
                PermitOutcome::Error
            },
            child,
        );
        if let Err(error) = prepared {
            if error.quarantined {
                return settle_match_worker_failure(&store, &job, None, child, &error);
            }
            let message = format!("{}: {}", error.code, error.message);
            worker.take();
            return Err(message);
        }
        if cancelled.load(Ordering::Acquire)
            || store.external_admission_epoch() != admitted.admission_epoch
            || !match_worker_can_continue(&store, &job_id, &cancelled)?
        {
            worker.take();
            return Ok(());
        }
        let mut preparation_complete = false;
        for _ in 0..128 {
            if !match_worker_can_continue(&store, &job_id, &cancelled)? {
                worker.take();
                return Ok(());
            }
            let compute = match retry_match_resource_pressure(
                &cancelled,
                || match_worker_can_continue(&store, &job_id, &cancelled),
                || {
                    store.acquire_worker_compute(
                        &coordinator,
                        root_identity.clone(),
                        &job_id,
                        None,
                        match_cpu_compute_request(cpu_policy, 64 * 1024, 1),
                        &mut None,
                    )
                },
            ) {
                Ok(compute) => compute,
                Err(error) => {
                    worker.take();
                    return Err(error);
                }
            };
            let current = match_worker_fence(&job, None, compute.admission_epoch());
            if current != admitted || cancelled.load(Ordering::Acquire) {
                compute.finish(PermitOutcome::Cancelled);
                worker.take();
                return Ok(());
            }
            let child = worker.as_mut().ok_or("isolated Match worker absent")?;
            let step = child.preparation_step(&current);
            compute.finish_after_worker(
                if step.is_ok() {
                    PermitOutcome::Success
                } else {
                    PermitOutcome::Error
                },
                child,
            );
            match step {
                Err(error) if error.quarantined => {
                    return settle_match_worker_failure(&store, &job, None, child, &error)
                }
                Err(error) => {
                    worker.take();
                    return Err(format!("{}: {}", error.code, error.message));
                }
                Ok(complete) => {
                    if cancelled.load(Ordering::Acquire)
                        || store.external_admission_epoch() != admitted.admission_epoch
                        || !match_worker_can_continue(&store, &job_id, &cancelled)?
                    {
                        worker.take();
                        return Ok(());
                    }
                    if complete {
                        preparation_complete = true;
                        break;
                    }
                }
            }
        }
        if !preparation_complete {
            worker.take();
            return Err("worker_input_limit: preparation checkpoint count exceeded".into());
        }
    }
    crate::match_discovery::validate_begin(&root_path, &root.exclusions)?;
    let (_, _, root_resources) = admitted_match_discovery_worker(
        &store,
        &job,
        worker.as_mut().ok_or("isolated Match worker absent")?,
        &coordinator,
        &root_identity,
        &cancelled,
        crate::match_discovery::BEGIN_REQUEST_BYTES,
        |worker, fence| worker.begin_discovery(&root_path, &root.exclusions, fence),
    )?;
    worker
        .as_mut()
        .ok_or("isolated Match worker absent")?
        .retain_discovery_resources(root_resources);
    let acquire_observed_discovery_write =
        |media_key: &str,
         queued_bytes: u64,
         observation: &crate::match_store::MatchDiscoveryObservation| {
            retry_admitted_match_discovery_write(&cancelled, || {
                store.acquire_discovery_write_for_observation(
                    &job_id,
                    media_key,
                    match_stage_request(crate::match_store::JobStage::Discover, queued_bytes, 0),
                    observation,
                )
            })
        };
    let mut unresolved_discovery = store
        .job_assets(&job_id)?
        .into_iter()
        .filter(|asset| {
            asset.media_fingerprint.starts_with("unavailable:") && asset.failure_code.is_some()
        })
        .map(|asset| asset.media_key)
        .collect::<BTreeSet<_>>();

    loop {
        let (entry, walk_observation, _walk_resources) = admitted_match_discovery_worker(
            &store,
            &job,
            worker.as_mut().ok_or("isolated Match worker absent")?,
            &coordinator,
            &root_identity,
            &cancelled,
            crate::match_discovery::CURSOR_BYTES,
            |worker, fence| worker.discovery_next(fence),
        )?;
        let Some(entry) = entry else {
            break;
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let sanitized_error = error.to_string().replace('\r', " ").replace('\n', " ");
                let relative = error
                    .path()
                    .and_then(|path| path.strip_prefix(&root_path).ok())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| {
                        let digest = format!("{:x}", Sha256::digest(sanitized_error.as_bytes()));
                        PathBuf::from(format!("walk-error-{}.entry", &digest[..16]))
                    });
                let walk_media_key = match_media_key(&root.root_id, &relative);
                let discovery_permit = acquire_observed_discovery_write(
                    &walk_media_key,
                    64 * 1024,
                    &walk_observation,
                )?;
                let media_key = record_match_walk_failure(
                    &store,
                    &discovery_permit,
                    &job_id,
                    &root.root_id,
                    &root_path,
                    error.path(),
                    &error.to_string(),
                )?;
                unresolved_discovery.insert(media_key);
                continue;
            }
        };
        let relative = entry
            .path()
            .strip_prefix(&root_path)
            .map_err(|error| format!("resolve Match root-relative discovery path: {error}"))?;
        let video_source = crate::media_explorer::is_video_path(&entry.path().to_string_lossy());
        if !entry.is_file || (!is_image_ext(entry.path()) && !video_source) {
            let media_key = match_media_key(&root.root_id, relative);
            if unresolved_discovery.remove(&media_key) {
                let discovery_permit =
                    acquire_observed_discovery_write(&media_key, 64 * 1024, &walk_observation)?;
                store.resolve_discovery_failure(&job_id, &media_key, &discovery_permit)?;
            }
            continue;
        }
        let media_key = match_media_key(&root.root_id, relative);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        let (metadata, metadata_observation, _metadata_resources) =
            admitted_match_discovery_worker(
                &store,
                &job,
                worker.as_mut().ok_or("isolated Match worker absent")?,
                &coordinator,
                &root_identity,
                &cancelled,
                crate::match_discovery::CURSOR_BYTES,
                |worker, fence| worker.discovery_metadata(entry.path(), fence),
            )?;
        let source_bytes = match metadata {
            Ok(size) => size,
            Err(error) => {
                let discovery_permit =
                    acquire_observed_discovery_write(&media_key, 64 * 1024, &metadata_observation)?;
                store.record_discovery_failure(
                    &job_id,
                    &media_key,
                    Some(entry.path()),
                    "io",
                    &format!("{relative_text}: read Match source metadata: {error}"),
                    &discovery_permit,
                )?;
                continue;
            }
        };
        if !video_source && source_bytes > MATCH_MAX_SOURCE_FILE_BYTES {
            let discovery_permit =
                acquire_observed_discovery_write(&media_key, 64 * 1024, &metadata_observation)?;
            store.record_discovery_failure(
                &job_id,
                &media_key,
                Some(entry.path()),
                "resource_pressure",
                &format!(
                    "{relative_text}: Match source size {source_bytes} exceeds the bounded {} byte snapshot limit",
                    MATCH_MAX_SOURCE_FILE_BYTES
                ),
                &discovery_permit,
            )?;
            continue;
        }
        let Some((io_permit, snapshot_observation)) =
            begin_match_discovery_io(&store, &job_id, &coordinator, &root_identity, &cancelled)?
        else {
            return Ok(());
        };
        let mut io_permit = Some(io_permit);
        let snapshot_result = {
            // The admitted observation remains valid, but the child fingerprint
            // units acquire their own Background permits. Never nest an I/O lease.
            if let Some(permit) = io_permit.take() {
                permit.finish(PermitOutcome::Success);
            }
            match_video_jobs::video_discovery_snapshot(
                &store,
                &job,
                worker.as_mut().ok_or("isolated Match worker absent")?,
                entry.path(),
                &root_path,
                &coordinator,
                &root_identity,
                &cancelled,
            )
        };
        let snapshot = match snapshot_result {
            Ok(snapshot) => {
                if let Some(permit) = io_permit.take() {
                    permit.finish(PermitOutcome::Success);
                }
                snapshot
            }
            Err(error) => {
                if let Some(permit) = io_permit.take() {
                    permit.finish(PermitOutcome::Error);
                }
                if match_admission_interrupted(&error)
                    || store.job(&job_id)?.lifecycle()? == JobLifecycle::Failed
                {
                    return Err(error);
                }
                let discovery_permit =
                    acquire_observed_discovery_write(&media_key, 64 * 1024, &snapshot_observation)?;
                store.record_discovery_failure(
                    &job_id,
                    &media_key,
                    Some(entry.path()),
                    match_failure_code(&error),
                    &format!("{relative_text}: {error}"),
                    &discovery_permit,
                )?;
                continue;
            }
        };
        let discovery_permit =
            acquire_observed_discovery_write(&media_key, 64 * 1024, &snapshot_observation)?;
        let _database_unit = discovery_permit.begin_database_unit()?;
        // Keep the literal fingerprint used to derive existing Face IDs. A
        // restored graph can contain observations even when no jobs survive.
        let observed_fingerprint = store
            .observed_media_fingerprint(&media_key, &snapshot.fingerprint)?
            .unwrap_or_else(|| snapshot.fingerprint.clone());
        store.enqueue_asset_from_isolated_source(
            &job_id,
            &media_key,
            &observed_fingerprint,
            &snapshot.final_path,
            &discovery_permit,
        )?;
    }

    for asset in store.job_assets(&job_id)? {
        if !match_worker_can_continue(&store, &job_id, &cancelled)? {
            return Ok(());
        }
        if asset.failure_code.is_none()
            && asset.skipped_code.is_none()
            && !asset
                .completed_stages
                .iter()
                .any(|stage| stage == crate::match_store::JobStage::Complete.as_str())
        {
            let Some(source_path) = asset.source_path.as_deref() else {
                continue;
            };
            if let Err(error) = run_match_asset(
                &store,
                &job,
                &asset,
                Path::new(source_path),
                &root_path,
                worker.as_mut().ok_or("isolated Match worker absent")?,
                &coordinator,
                &root_identity,
                &cancelled,
            ) {
                if match_admission_interrupted(&error) {
                    return Ok(());
                }
                if match_database_outcome_unknown(&error) {
                    // The checkpoint may already be durable. The owner must
                    // reconcile its exact operation receipt before any retry.
                    return Err(error);
                }
                let current_asset = store.job_asset(&asset.asset_id)?;
                let current_job = store.job(&job_id)?;
                if current_job.lifecycle()? == JobLifecycle::Pausing {
                    return Err(error);
                }
                if store.can_attempt_automatic(current_job.lifecycle()?)? {
                    let fence = match_revision_fence(&current_job, &current_asset);
                    let stage = current_asset.next_stage()?;
                    let permit = retry_match_resource_pressure(
                        &cancelled,
                        || match_worker_can_continue(&store, &job_id, &cancelled),
                        || {
                            store.acquire_background_stage(
                                &coordinator,
                                root_identity.clone(),
                                &fence,
                                stage,
                                match_stage_request(
                                    stage,
                                    1,
                                    if matches!(
                                        stage,
                                        crate::match_store::JobStage::Detect
                                            | crate::match_store::JobStage::Align
                                    ) {
                                        1
                                    } else {
                                        0
                                    },
                                ),
                            )
                        },
                    )?;
                    store.record_asset_failure(
                        &current_asset.asset_id,
                        match_failure_code(&error),
                        &error,
                        &fence,
                        &permit,
                    )?;
                }
            }
        }
    }

    let final_job = store.job(&job_id)?;
    if final_job.lifecycle()? == JobLifecycle::Running {
        let next = if final_job.failed > 0 {
            if final_job.completed > 0 || final_job.skipped > 0 {
                JobLifecycle::Partial
            } else {
                JobLifecycle::Failed
            }
        } else {
            JobLifecycle::Completed
        };
        store.set_job_lifecycle(&job_id, next)?;
    }
    Ok(())
}

#[derive(Clone)]
enum MatchVideoPreview {
    Correction(crate::match_store::VideoTrackCorrectionPreview),
    Split(crate::match_store::VideoTrackSplitPreview),
}

pub struct FacialService {
    config: AppConfig,
    match_cpu_benchmark_policy: Option<crate::match_worker::CpuExecutionPolicy>,
    debug: DebugBus,
    registry: ModelRegistry,
    plugins: PluginHost,
    worktrees: WorktreeManager,
    copy_location: Option<PathBuf>,
    run_results_index: Vec<PathBuf>,
    identity: Option<IdentityEngine>,
    identity_load_error: Option<crate::identity::IdentityError>,
    identity_refs: Option<Vec<crate::identity::IdentityVector>>,
    identity_negs: Option<Vec<crate::identity::IdentityVector>>,
    match_store: Arc<Mutex<MatchStoreRuntime>>,
    external_match_holds: Arc<crate::match_store::MatchExternalHolds>,
    retired_match_workers: Vec<std::thread::JoinHandle<()>>,
    match_video_previews: Mutex<BTreeMap<String, MatchVideoPreview>>,
    /// Lazy PIPNet 98-pt landmark engine (WP-021); loaded on first gate use.
    landmarks: Option<crate::landmarks::LandmarkEngine>,
    landmarks_load_attempted: bool,
}

pub(crate) struct MatchInspectionFrame {
    pub request: crate::api::MatchVideoRequest,
    pub sample: crate::match_video_decode::DecodedVideoSample,
    pub observation_id: String,
    pub media_fingerprint: String,
    pub playback_pin: Option<Arc<crate::match_video_decode::PlaybackSourcePin>>,
    pub source_path: String,
    pub _resources: crate::match_store::MatchResourceLease,
}

impl FacialService {
    pub(crate) fn inspection_store(&self) -> Result<crate::match_store::MatchStore, String> {
        self.match_store
            .lock()
            .map_err(|_| "Match runtime lock poisoned")?
            .store
            .clone()
            .ok_or_else(|| "Match store is not ready; load appearances first".into())
    }

    pub(crate) fn match_inspect_appearance(
        store: crate::match_store::MatchStore,
        request: &crate::api::MatchVideoRequest,
        coordinator: &crate::media_io::MediaIoCoordinator,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<MatchInspectionFrame, String> {
        use crate::match_store::ResourceRequest;
        use crate::match_worker::{IsolatedMatchWorker, WorkerFence};
        use crate::media_io::{PermitOutcome, RootIdentity, RootKind, WorkClass};
        crate::api::validate_match_video(request)?;
        if !matches!(
            request.action,
            crate::api::MatchVideoAction::InspectAppearance
                | crate::api::MatchVideoAction::SeekAppearance
        ) {
            return Err("explicit inspection action required".into());
        }
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err("appearance inspection cancelled".into());
        }
        let track_id = request.track_id.as_deref().ok_or("track required")?;
        let revision = request.track_revision.ok_or("track revision required")?;
        let time = request.timestamp.ok_or("exact time required")?;
        let row = store.video_inspection_observation(track_id, revision, time)?;
        if row.media_key != request.media_key {
            return Err("inspection media scope changed".into());
        }
        let observation = row.observation()?;
        let authority = store.manual_media_authority(&row.media_key, &row.media_fingerprint)?;
        let job = store.job(&authority.fence.job_id)?;
        let root = RootIdentity::new(
            authority.root_path.to_string_lossy().into_owned(),
            0,
            RootKind::Unknown,
        );
        let fence = WorkerFence {
            job_id: job.job_id,
            asset_id: authority.fence.asset_id.clone(),
            media_key: row.media_key.clone(),
            schema_generation: job.schema_generation,
            identity_revision: job.identity_revision,
            catalog_revision: job.catalog_revision,
            admission_epoch: store.external_admission_epoch(),
            model_generation: job.model_generation,
            media_fingerprint: row.media_fingerprint.clone(),
            track_id: Some(track_id.into()),
            timestamp_ms: Some(time.milliseconds()?),
        };
        let retained = store.governor().try_acquire(ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 8 * 1024 * 1024,
            decoded_bytes: 8 * 1024 * 1024,
            ..ResourceRequest::default()
        })?;
        let compute = store.governor().try_acquire(ResourceRequest {
            admitted_items: 1,
            queued_items: 1,
            queued_bytes: 4 * 1024 * 1024,
            decoded_bytes: crate::match_decoder_process::MEMORY_LIMIT as u64,
            cpu_inference: 1,
            worker_memory_bytes: crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
            ..ResourceRequest::default()
        })?;
        let mut worker =
            IsolatedMatchWorker::spawn_with_resources(compute, None).map_err(|e| e.code)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut run = |action: &mut dyn FnMut(&mut IsolatedMatchWorker) -> Result<(), String>| {
            let queued = coordinator.enqueue(root.clone(), WorkClass::Visible);
            let permit = loop {
                if cancelled.load(std::sync::atomic::Ordering::Acquire)
                    || std::time::Instant::now() >= deadline
                {
                    queued.cancel();
                    return Err("appearance inspection timed out".to_string());
                }
                if let Some(permit) = queued
                    .try_acquire()
                    .map_err(|_| "inspection I/O admission failed")?
                {
                    break permit;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            let result = action(&mut worker);
            permit.finish(if result.is_ok() {
                PermitOutcome::Success
            } else {
                PermitOutcome::Error
            });
            result
        };
        let mut playback_pin = None;
        run(&mut |worker| {
            if request.action == crate::api::MatchVideoAction::SeekAppearance {
                playback_pin = Some(
                    worker
                        .begin_playback_source(&authority.source_path, &authority.root_path, &fence)
                        .map_err(|e| e.code)?,
                );
                Ok(())
            } else {
                worker
                    .begin_source(&authority.source_path, &authority.root_path, &fence)
                    .map_err(|e| e.code)
            }
        })?;
        let mut previous = 0;
        let decoded_path = loop {
            let mut progress = None;
            run(&mut |worker| {
                progress = Some(worker.hash_source_step(&fence).map_err(|e| e.code)?);
                Ok(())
            })?;
            let progress = progress.ok_or("missing source progress")?;
            if playback_pin
                .as_ref()
                .is_some_and(|pin| pin.final_path != progress.final_path)
            {
                return Err("playback source pin differs from hashed source".into());
            }
            if progress.bytes_hashed < previous
                || progress.bytes_hashed - previous > 4 * 1024 * 1024
            {
                return Err("inspection hash progress invalid".into());
            }
            if let Some(hash) = progress.fingerprint {
                if progress.bytes_hashed != progress.size
                    || crate::match_store::canonical_media_sha256(&row.media_fingerprint)
                        != Some(hash.as_str())
                {
                    return Err("inspection source fingerprint changed".into());
                }
                break progress.final_path;
            }
            if progress.bytes_hashed == previous {
                return Err("inspection source hash stalled".into());
            }
            previous = progress.bytes_hashed;
        };
        let mut sample = None;
        run(&mut |worker| {
            sample = worker
                .decode_exact(&decoded_path, time, observation.stream_index, &fence)
                .map_err(|e| e.code)?;
            Ok(())
        })?;
        drop(run);
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err("appearance inspection cancelled".into());
        }
        let sample = sample.ok_or("exact appearance frame absent")?;
        if sample.frame_sha256 != observation.frame_sha256
            || sample.playback_origin != observation.playback_origin
        {
            return Err("inspection frame provenance changed".into());
        }
        let current = store.video_inspection_observation(track_id, revision, time)?;
        let current_authority =
            store.manual_media_authority(&row.media_key, &row.media_fingerprint)?;
        if current != row
            || current_authority.source_path != authority.source_path
            || current_authority.root_path != authority.root_path
            || current_authority.fence != authority.fence
        {
            return Err("inspection canonical source or observation changed".into());
        }
        if !worker.shutdown_and_confirm() {
            return Err("inspection worker exit unconfirmed".into());
        }
        drop(worker);
        Ok(MatchInspectionFrame {
            request: request.clone(),
            sample,
            observation_id: row.observation_id,
            media_fingerprint: row.media_fingerprint,
            playback_pin,
            source_path: authority.source_path.to_string_lossy().into_owned(),
            _resources: retained,
        })
    }

    pub fn new(config: AppConfig) -> Self {
        if !config.worktrees_root.exists() {
            let _ = fs::create_dir_all(&config.worktrees_root);
        }
        let mut debug = DebugBus::new(config.debug_log_path.clone(), config.max_debug_events);
        debug.emit("INFO", "Service", "initializing service", None);
        let registry = ModelRegistry::load(config.model_registry_path.clone(), &mut debug);
        let plugins = PluginHost::new(&config);
        let worktrees = WorktreeManager {
            root: config.worktrees_root.clone(),
        };
        let copy_location = config.copy_location.clone();
        let match_store = match_store_runtime(config.workspace_root.clone());
        let external_match_holds = Arc::clone(&match_store.lock().unwrap().external_holds);
        let (identity, identity_load_error) = match config.identity_manifest_path.as_ref() {
            Some(path) => match IdentityEngine::load_manifest(path) {
                Ok(engine) => {
                    debug.emit(
                        "INFO",
                        "Identity",
                        &format!(
                            "identity model loaded: {} sha256={}",
                            engine.model_path().display(),
                            engine.model_sha256()
                        ),
                        None,
                    );
                    (Some(engine), None)
                }
                Err(err) => {
                    debug.emit(
                        "WARN",
                        "Identity",
                        &format!("identity model load failed: {}", err.code),
                        None,
                    );
                    (None, Some(err))
                }
            },
            None if config.identity_model_path.is_some() => (
                None,
                Some(crate::identity::IdentityError {
                    code: "manifest_required".to_string(),
                    message: "legacy raw model paths require explicit reprovisioning".to_string(),
                }),
            ),
            None => (None, None),
        };
        let mut service = Self {
            config,
            match_cpu_benchmark_policy: None,
            debug,
            registry,
            plugins,
            worktrees,
            copy_location,
            run_results_index: Vec::new(),
            identity,
            identity_load_error,
            identity_refs: None,
            identity_negs: None,
            match_store,
            external_match_holds,
            retired_match_workers: Vec::new(),
            match_video_previews: Mutex::new(BTreeMap::new()),
            landmarks: None,
            landmarks_load_attempted: false,
        };
        service.sync_detector_registry();
        service
    }

    pub(crate) fn new_with_match_cpu_benchmark_policy(
        config: AppConfig,
        policy: crate::match_worker::CpuExecutionPolicy,
    ) -> Self {
        let mut service = Self::new(config);
        service.match_cpu_benchmark_policy = Some(policy);
        service
    }

    pub(crate) fn match_cpu_policy_diagnostics(&self) -> Result<serde_json::Value, String> {
        let runtime = self
            .match_store
            .lock()
            .map_err(|_| "Match runtime lock poisoned")?;
        let selected = self.match_cpu_benchmark_policy.unwrap_or_default();
        Ok(json!({
            "diagnostic_only": self.match_cpu_benchmark_policy.is_some(),
            "selected_policy": match selected {
                crate::match_worker::CpuExecutionPolicy::Baseline => "baseline",
                crate::match_worker::CpuExecutionPolicy::PrivateTwoThread => "private_two_thread",
            },
            "promoted": false,
            "acknowledgement_scope": "last_worker_acknowledgement_after_job_iteration_returned_not_live_thread_count",
            "last_executor_acknowledgement": runtime.last_cpu_executor_acknowledgement,
        }))
    }

    /// Load the landmark engine once on first use (47MB model; lazy so launch
    /// stays fast). Failure is logged and never retried within the process.
    fn ensure_landmarks(&mut self) {
        if self.landmarks.is_some() || self.landmarks_load_attempted {
            return;
        }
        self.landmarks_load_attempted = true;
        let Some(path) = self.config.landmark_model_path.clone() else {
            return;
        };
        match crate::landmarks::LandmarkEngine::load(&path) {
            Ok(engine) => {
                self.debug.emit(
                    "INFO",
                    "Landmarks",
                    &format!(
                        "landmark model loaded: {} sha256={}",
                        engine.model_path().display(),
                        engine.model_sha256()
                    ),
                    None,
                );
                self.landmarks = Some(engine);
            }
            Err(err) => {
                self.debug.emit(
                    "WARN",
                    "Landmarks",
                    &format!("landmark model load failed: {err}"),
                    None,
                );
            }
        }
    }

    /// Keep the model registry truthful about the active face detector
    /// (bundled vs override, WP-020): upsert a `yunet-detector` record whose
    /// description carries origin + sha256.
    fn sync_detector_registry(&mut self) {
        let Some(engine) = &self.identity else {
            return;
        };
        let origin = engine.detector_origin();
        if origin == "none" {
            return;
        }
        let description = format!(
            "YuNet face detector ({origin}) sha256={}",
            engine.detector_sha256().unwrap_or("?")
        );
        match self.registry.by_id("yunet-detector") {
            Some(existing) if existing.description == description => {}
            _ => {
                let record = ModelRecord {
                    id: "yunet-detector".to_string(),
                    name: format!("YuNet detector ({origin})"),
                    description,
                    source_path: String::new(),
                    status: "active".to_string(),
                    tags: vec!["detector".to_string()],
                };
                // add() refuses duplicates; replace by remove-then-add when stale.
                if !self.registry.add(record.clone(), &mut self.debug) {
                    self.registry.remove("yunet-detector", &mut self.debug);
                    let _ = self.registry.add(record, &mut self.debug);
                }
            }
        }
    }

    pub fn ingest_in_place_default(&self) -> bool {
        self.config.ingest_in_place_default
    }

    pub fn max_debug_events(&self) -> usize {
        self.config.max_debug_events
    }

    pub fn list_models(&mut self) -> Vec<ModelRecord> {
        self.registry.list()
    }

    pub fn add_model(
        &mut self,
        model_id: &str,
        name: &str,
        description: &str,
    ) -> Result<ModelRecord, String> {
        let record = ModelRecord {
            id: slugify(model_id),
            name: if name.trim().is_empty() {
                slugify(model_id)
            } else {
                name.to_string()
            },
            description: description.to_string(),
            source_path: "".to_string(),
            status: "active".to_string(),
            tags: Vec::new(),
        };
        if self.registry.add(record.clone(), &mut self.debug) {
            self.debug.emit(
                "INFO",
                "ModelRegistry",
                &format!("added model {}", record.id),
                None,
            );
            Ok(record)
        } else {
            Err(format!("model id already exists: {}", record.id))
        }
    }

    pub fn get_model(&mut self, model_id: &str) -> Option<ModelRecord> {
        self.registry.by_id(model_id)
    }

    pub fn create_project_worktree(&mut self, project_name: &str) -> Result<PathBuf, String> {
        if project_name.trim().is_empty() {
            return Err("project_name required".to_string());
        }
        self.worktrees
            .create(project_name)
            .map_err(|err| format!("cannot create worktree: {err}"))
    }

    pub fn list_worktrees(&mut self) -> BTreeMap<String, Vec<PathBuf>> {
        let mut out = BTreeMap::new();
        if !self.worktrees.root.exists() {
            return out;
        }
        for project in std::fs::read_dir(&self.worktrees.root)
            .into_iter()
            .flatten()
        {
            let Ok(project) = project else { continue };
            if !project.path().is_dir() {
                continue;
            }
            let project_name = project.file_name().to_string_lossy().to_string();
            let mut runs = Vec::new();
            if let Ok(entries) = std::fs::read_dir(project.path()) {
                for run in entries.flatten() {
                    if run.path().is_dir() {
                        runs.push(run.path());
                    }
                }
            }
            runs.sort();
            out.insert(project_name, runs);
        }
        out
    }

    pub fn list_plugins(&mut self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for manifest in self.plugins.list_plugins() {
            let payload = serde_json::to_value(&manifest).unwrap_or_else(|_| serde_json::json!({}));
            out.push(payload);
        }
        out
    }

    pub fn refresh_plugins(&mut self) -> Vec<serde_json::Value> {
        self.plugins.refresh();
        self.debug
            .emit("INFO", "Service", "plugins refreshed", None);
        self.list_plugins()
    }

    pub fn ingest_images(
        &mut self,
        project_name: &str,
        source_images: &[String],
        in_place: bool,
    ) -> Vec<IngestResult> {
        let sources = normalize_paths(source_images, Path::new(""));
        if sources.is_empty() {
            return vec![IngestResult {
                source: "".to_string(),
                destination: "".to_string(),
                mode: "error".to_string(),
                ok: false,
                message: "no images found".to_string(),
            }];
        }

        if in_place {
            return sources
                .into_iter()
                .map(|source| {
                    self.debug.emit(
                        "INFO",
                        "Ingest",
                        &format!("working in place source={source}"),
                        None,
                    );
                    IngestResult {
                        source: source.clone(),
                        destination: source,
                        mode: "in_place".to_string(),
                        ok: true,
                        message: "using source in place".to_string(),
                    }
                })
                .collect();
        }

        let target_root = match self.project_copy_images_root(project_name) {
            Ok(path) => path,
            Err(err) => {
                return vec![IngestResult {
                    source: "".to_string(),
                    destination: "".to_string(),
                    mode: "error".to_string(),
                    ok: false,
                    message: err,
                }];
            }
        };

        let mut output = Vec::new();
        for source in sources {
            output.push(self.ingest_single(Path::new(&source), &target_root, false));
        }
        output
    }

    pub fn run_pipeline(
        &mut self,
        project_name: &str,
        image_paths: &[String],
        feature_keys: &[String],
        worktree_path: Option<String>,
        in_place: bool,
    ) -> Result<RunSummary, String> {
        if self.copy_location.is_none() {
            self.debug
                .emit("ERROR", "Pipeline", "copy/output location not set", None);
            return Err("Set a copy/output location before starting any task".to_string());
        }
        if feature_keys.is_empty() {
            return Err("no features selected".to_string());
        }

        let fallback = if in_place {
            PathBuf::new()
        } else {
            self.project_copy_root(project_name)?
        };
        let mut normalized = normalize_paths(image_paths, &fallback);
        if normalized.is_empty() {
            self.debug
                .emit("ERROR", "Pipeline", "No images available", None);
            return Err("No images available".to_string());
        }

        if !in_place {
            let target_root = self.project_copy_images_root(project_name)?;
            let mut copied = Vec::new();
            for source in &normalized {
                if Path::new(source).starts_with(&target_root) {
                    copied.push(source.clone());
                } else {
                    let result = self.ingest_single(Path::new(source), &target_root, false);
                    if result.ok {
                        copied.push(result.destination);
                    } else {
                        return Err(result.message);
                    }
                }
            }
            normalized = copied;
        }

        let (worktree, run_root) =
            self.run_root_for(project_name, &normalized, worktree_path, in_place)?;
        fs::create_dir_all(&run_root).map_err(|err| format!("could not create run root: {err}"))?;
        let run_id = run_root
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("run")
            .to_string();

        self.debug.emit(
            "INFO",
            "Pipeline",
            &format!(
                "pipeline started: {run_id} features={} images={}",
                feature_keys.len(),
                normalized.len()
            ),
            Some(json!({
                "run_id": run_id,
                "in_place": in_place,
                "features": feature_keys.len(),
                "images": normalized.len(),
            })),
        );

        let mut plugin_results: Vec<PluginRunResult> = Vec::new();
        let mut totals = BTreeMap::new();
        totals.insert("ok".to_string(), 0);
        totals.insert("skipped".to_string(), 0);
        totals.insert("failed".to_string(), 0);

        for key in feature_keys {
            let split: Vec<_> = key.splitn(2, ':').collect();
            if split.len() != 2 {
                self.debug.emit(
                    "ERROR",
                    "Pipeline",
                    &format!("invalid feature key: {key} (expected plugin_id:feature_id)"),
                    Some(json!({"feature_key": key, "reason": "invalid feature key format"})),
                );
                totals.insert("failed".to_string(), totals["failed"] + 1);
                plugin_results.push(PluginRunResult {
                    plugin_id: "unknown".to_string(),
                    feature_id: key.to_string(),
                    status: "failed".to_string(),
                    message: format!("invalid feature key: {key}"),
                    payload: json!({"status":"failed"}),
                    artifacts: Vec::new(),
                });
                continue;
            }
            let plugin_id = split[0];
            let feature_id = split[1];
            let run_feature_root = run_root.join(plugin_id).join(feature_id);
            let _ = fs::create_dir_all(&run_feature_root);
            // (b) Real identity path: when an embedder is provisioned, deepface
            // represent/verify/find use real ArcFace embeddings instead of the proxy.
            if plugin_id == "deepface"
                && matches!(feature_id, "represent" | "verify" | "find")
                && self.identity.is_some()
            {
                let result = self.real_deepface_feature(feature_id, &normalized, &run_feature_root);
                if result.status == "ok" || result.status == "completed" {
                    totals.insert("ok".to_string(), totals["ok"] + 1);
                } else {
                    totals.insert("failed".to_string(), totals["failed"] + 1);
                }
                plugin_results.push(result);
                continue;
            }
            let result = self.plugins.run_feature(
                plugin_id,
                feature_id,
                &normalized,
                &run_feature_root,
                &run_id,
                &mut self.debug,
            );
            if result.status == "ok" || result.status == "completed" {
                totals.insert("ok".to_string(), totals["ok"] + 1);
            } else {
                totals.insert("failed".to_string(), totals["failed"] + 1);
            }
            plugin_results.push(result);
        }

        let status = if totals["failed"] == 0 {
            "completed".to_string()
        } else {
            "partial".to_string()
        };
        let summary = RunSummary {
            run_id: run_id.clone(),
            project_name: project_name.to_string(),
            worktree: worktree.to_string_lossy().to_string(),
            images: normalized,
            feature_keys: feature_keys.to_vec(),
            status: status.clone(),
            in_place,
            totals: totals.clone(),
            plugin_results: plugin_results.clone(),
            output_path: String::new(),
        };

        let summary_path = run_root.join("results.json");
        let payload = serde_json::to_value(&summary).unwrap_or_else(|_| serde_json::json!({}));
        let _ = fs::write(
            &summary_path,
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
        self.run_results_index.push(summary_path.clone());
        let mut final_summary = summary;
        final_summary.output_path = summary_path.to_string_lossy().to_string();

        self.debug.emit(
            "INFO",
            "Pipeline",
            &format!("pipeline finished: {status} run={run_id}"),
            None,
        );
        Ok(final_summary)
    }

    pub fn get_recent_events(&mut self, limit: usize) -> Vec<DebugEvent> {
        self.debug.combined_recent(limit)
    }

    /// Expose config so api.rs can build ApiPaths and AppStateSnapshot
    /// (repo_root/worktrees_root/api_root). config stays private otherwise.
    pub fn config(&self) -> &crate::config::AppConfig {
        &self.config
    }

    pub fn workspace_root(&self) -> &Path {
        &self.config.workspace_root
    }

    fn ready_match_store(&self) -> Result<crate::match_store::MatchStore, String> {
        start_match_store_initialization(&self.match_store)?;
        let worker = self
            .match_store
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?
            .worker
            .take();
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| "match_store_worker_panicked".to_string())?;
        }
        let state = self
            .match_store
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
        state.store.clone().ok_or_else(|| {
            state
                .error_code
                .clone()
                .unwrap_or_else(|| "match_store_unavailable".to_string())
        })
    }

    pub fn match_ui_snapshot(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        self.ready_match_store()?.ui_snapshot(offset, limit)
    }

    pub fn match_public_snapshot(&self) -> Result<serde_json::Value, String> {
        self.ready_match_store()?.public_snapshot()
    }

    /// Read only an already initialized GUI-owned store. Diagnostics must not
    /// start initialization or join its worker to observe an unavailable run.
    pub fn match_ready_public_snapshot(&self) -> Result<serde_json::Value, String> {
        self.match_ready_store_for_diagnostics()?.public_snapshot()
    }

    pub(crate) fn match_ready_store_for_diagnostics(
        &self,
    ) -> Result<crate::match_store::MatchStore, String> {
        {
            let state = self
                .match_store
                .lock()
                .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
            state.store.clone().ok_or_else(|| {
                state
                    .error_code
                    .clone()
                    .unwrap_or_else(|| "match_store_not_ready".to_string())
            })
        }
    }

    pub fn match_settings_snapshot(&self) -> Result<serde_json::Value, String> {
        self.ready_match_store()?.settings_snapshot()
    }

    pub fn match_settings_snapshot_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        self.ready_match_store()?
            .settings_snapshot_page(offset, limit)
    }

    pub fn match_create_person(
        &self,
        name: &str,
        aliases: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(self.ready_match_store()?.create_person(name, aliases)?)
            .map_err(|error| error.to_string())
    }

    pub fn match_update_person(
        &self,
        person_id: &str,
        expected_revision: u64,
        name: &str,
        aliases: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(self.ready_match_store()?.update_person(
            person_id,
            expected_revision,
            name,
            aliases,
        )?)
        .map_err(|error| error.to_string())
    }

    pub fn match_update_person_preferences(
        &self,
        person_id: &str,
        expected_revision: u64,
        cover_media_key: Option<String>,
        hidden: bool,
        favorite: bool,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(self.ready_match_store()?.update_person_preferences(
            person_id,
            expected_revision,
            cover_media_key,
            hidden,
            favorite,
        )?)
        .map_err(|error| error.to_string())
    }

    pub fn match_person_gallery(
        &self,
        person_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(
            self.ready_match_store()?
                .person_gallery(person_id, offset, limit)?,
        )
        .map_err(|error| error.to_string())
    }

    pub fn match_person_gallery_inventory(
        &self,
        person_id: &str,
    ) -> Result<crate::match_store::PersonGalleryInventory, String> {
        self.ready_match_store()?
            .person_gallery_inventory(person_id)
    }

    pub fn match_person_gallery_inventory_outcome(
        &self,
        person_id: &str,
    ) -> Result<crate::match_store::PersonGalleryInventoryOutcome, String> {
        self.ready_match_store()?
            .person_gallery_inventory_outcome(person_id)
    }

    pub fn match_person_membership_projection(
        &self,
        person_ids: &[String],
    ) -> Result<crate::match_store::PersonMembershipProjection, String> {
        self.ready_match_store()?
            .person_membership_projection(person_ids)
    }

    pub fn match_person_search_autocomplete(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::match_store::Person>, String> {
        self.ready_match_store()?
            .person_search_autocomplete(query, limit)
    }

    /// Execute the closed WP-085 maintenance protocol. Preview/apply pairs
    /// always recompute state in MatchStore; this service never treats a GUI
    /// process-local object as authority across separate intents or restarts.
    pub fn match_maintenance(
        &mut self,
        action_id: &str,
        request: &crate::api::MatchMaintenanceRequest,
    ) -> Result<serde_json::Value, String> {
        use crate::api::MatchMaintenanceAction as Action;
        use crate::match_store::IdentityImportMode;

        let quiesce = matches!(
            request.action,
            Action::IdentityImport
                | Action::IdentityImportRollback
                | Action::RebuildMatchAnalysis
                | Action::ClearAllMatchData
                | Action::RestoreRecoveryBundle
        );
        let store = self.ready_match_store()?;
        if quiesce {
            require_match_index_workers_quiescent(&self.match_store)?;
        }
        let path = || {
            request
                .path
                .as_deref()
                .map(Path::new)
                .ok_or_else(|| "Match maintenance action requires path".to_string())
        };
        let token = || {
            request
                .confirmation_token
                .as_deref()
                .ok_or_else(|| "Match maintenance action requires confirmation token".to_string())
        };
        let import_mode = || match request.conflict_policy.as_deref().unwrap_or("reject") {
            "reject" => Ok(IdentityImportMode::Merge),
            "replace" => Ok(IdentityImportMode::Replace),
            other => Err(format!(
                "unsupported identity import conflict policy: {other}"
            )),
        };
        match request.action {
            Action::IdentityExportPreview => {
                match_maintenance_value(store.preview_identity_bundle_export()?)
            }
            Action::IdentityExport => {
                let expected = request
                    .expected_digest
                    .as_deref()
                    .ok_or_else(|| "identity export requires expected digest".to_string())?;
                match_maintenance_value(store.export_identity_bundle_expected(path()?, expected)?)
            }
            Action::IdentityImportDryRun => match_maintenance_value(
                store
                    .preview_identity_bundle_import(path()?, &request.relocations, import_mode()?)?
                    .summary(),
            ),
            Action::IdentityImport => {
                let plan = store.preview_identity_bundle_import(
                    path()?,
                    &request.relocations,
                    import_mode()?,
                )?;
                if plan.plan_token != token()? {
                    return Err("stale or mismatched identity import plan token".to_string());
                }
                let recovery_root = self.config.api_root.join("match-recovery");
                fs::create_dir_all(&recovery_root)
                    .map_err(|error| format!("create Match recovery directory: {error}"))?;
                let recovery_path = recovery_root.join(format!(
                    "pre-import-{}.facial-identity.json",
                    Uuid::new_v4().simple()
                ));
                let recovery = store.export_identity_recovery_bundle(&recovery_path)?;
                let receipt = store.apply_identity_bundle_import(plan).map_err(|error| {
                    format!(
                        "{error}; pre-import recovery bundle retained at {}",
                        recovery_path.display()
                    )
                })?;
                Ok(json!({
                    "receipt": receipt.summary(),
                    "rollback_bundle": recovery,
                }))
            }
            Action::IdentityImportRollback => match_maintenance_value(
                store
                    .rollback_identity_bundle_import_file(path()?, &request.relocations, token()?)?
                    .summary(),
            ),
            Action::XmpExportPreview => match_maintenance_value(
                store.preview_mwg_xmp_sidecar(
                    request
                        .media_key
                        .as_deref()
                        .ok_or_else(|| "XMP export requires media_key".to_string())?,
                    path()?,
                )?,
            ),
            Action::XmpExport => match_maintenance_value(
                store.export_mwg_xmp_sidecar(
                    request
                        .media_key
                        .as_deref()
                        .ok_or_else(|| "XMP export requires media_key".to_string())?,
                    path()?,
                    token()?,
                )?,
            ),
            Action::XmpImportDryRun => match_maintenance_value(
                crate::match_store::MatchStore::preview_mwg_xmp_import(path()?)?,
            ),
            Action::XmpImport => {
                let receipt =
                    crate::match_store::MatchStore::import_mwg_xmp_sidecar(path()?, token()?)?;
                xmp_import_staging_contract(action_id, &self.config.api_root, receipt)
            }
            Action::RebuildMatchAnalysisPreview => {
                match_maintenance_value(store.preview_rebuild_match_analysis()?)
            }
            Action::RebuildMatchAnalysis => {
                let preview = store.preview_rebuild_match_analysis()?;
                if preview.preview_id != token()? {
                    return Err("stale or mismatched rebuild_match_analysis token".to_string());
                }
                match_maintenance_value(store.rebuild_match_analysis(&preview)?)
            }
            Action::ClearAllMatchDataPreview => {
                match_maintenance_value(store.preview_clear_all_match_data(path()?)?)
            }
            Action::ClearAllMatchData => {
                match_maintenance_value(store.clear_all_match_data_file(path()?, token()?)?)
            }
            Action::RestoreRecoveryBundle => match_maintenance_value(
                store
                    .restore_clear_recovery_bundle(path()?, &request.relocations, token()?)?
                    .summary(),
            ),
        }
    }

    /// Committed Viewer metadata retains the exact provenance join without opening source media.
    pub fn match_media_metadata(&self, media_key: &str) -> Result<serde_json::Value, String> {
        let store = self.ready_match_store()?;
        serde_json::to_value(store.media_faces(media_key)?).map_err(|error| error.to_string())
    }

    pub fn match_media_metadata_for_source(
        &self,
        source_path: &Path,
    ) -> Result<serde_json::Value, String> {
        let source_path = Self::match_selected_source_absolute(source_path)?;
        self.ready_match_store()?
            .media_metadata_for_source(&source_path)
    }

    pub fn match_media_faces_for_source(
        &self,
        source_path: &Path,
    ) -> Result<serde_json::Value, String> {
        let source_path = Self::match_selected_source_absolute(source_path)?;
        let store = self.ready_match_store()?;
        let snapshot = store.media_faces_for_source(&source_path)?.ok_or(
            "match_source_resolution_unindexed: selected source has no canonical Match asset",
        )?;
        let key = snapshot.media_key.clone();
        let value = serde_json::to_value(snapshot).map_err(|error| error.to_string())?;
        self.match_media_geometry(&store, &key, value, Some(&source_path))
    }

    fn match_selected_source_absolute(source_path: &Path) -> Result<PathBuf, String> {
        let resolved = if source_path.is_absolute() {
            source_path.to_path_buf()
        } else {
            let text = source_path
                .to_str()
                .ok_or("match_source_resolution_unsupported: non-UTF8 source")?;
            if text.is_empty()
                || source_path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::Prefix(_)
                            | std::path::Component::RootDir
                            | std::path::Component::ParentDir
                    )
                })
                || text.split(['/', '\\']).any(|part| {
                    part != "."
                        && (part.is_empty()
                            || part.ends_with(['.', ' '])
                            || part.chars().any(|ch| {
                                ch.is_control()
                                    || matches!(ch, ':' | '?' | '*' | '"' | '<' | '>' | '|')
                            }))
                })
            {
                return Err("match_source_resolution_unsupported: unsafe relative source".into());
            }
            std::path::absolute(source_path).map_err(|error| {
                format!("match_source_resolution_unsupported: resolve relative source: {error}")
            })?
        };
        crate::match_store::match_source_path_candidates(&resolved)?;
        Ok(resolved)
    }

    pub fn match_media_faces(&self, media_key: &str) -> Result<serde_json::Value, String> {
        let store = self.ready_match_store()?;
        let value = serde_json::to_value(store.media_faces(media_key)?)
            .map_err(|error| error.to_string())?;
        self.match_media_geometry(&store, media_key, value, None)
    }

    fn match_media_geometry(
        &self,
        store: &crate::match_store::MatchStore,
        media_key: &str,
        mut value: serde_json::Value,
        selected_source: Option<&Path>,
    ) -> Result<serde_json::Value, String> {
        let fingerprint = value
            .get("media_fingerprint")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(fingerprint) = fingerprint {
            crate::match_benchmark::note_match_geometry_preparation();
            match store.manual_media_authority(media_key, &fingerprint) {
                Ok(authority) => {
                    if let Some(source) = selected_source {
                        let candidates = crate::match_store::match_source_path_candidates(source)?;
                        if !authority.source_path.to_str().is_some_and(|path| {
                            candidates.iter().any(|candidate| candidate == path)
                        }) {
                            return Err("match_source_resolution_stale: explicit authority no longer matches selected source".into());
                        }
                    }
                    let geometry = authority
                        .root_path
                        .canonicalize()
                        .map_err(|error| format!("canonicalize Viewer Match root: {error}"))
                        .and_then(|root| match_file_snapshot(&authority.source_path, &root))
                        .and_then(|snapshot| {
                            if !match_source_fingerprint_matches(
                                &snapshot.fingerprint,
                                &fingerprint,
                            ) {
                                return Err("Viewer Match source fingerprint changed".to_string());
                            }
                            match_decoded_working_bytes(&snapshot.bytes, &snapshot.final_path)?;
                            let orientation =
                                crate::media_thumbs::exif_orientation(&snapshot.bytes);
                            let image =
                                image::load_from_memory(&snapshot.bytes).map_err(|error| {
                                    format!("decode Viewer Match source geometry: {error}")
                                })?;
                            let oriented =
                                crate::media_thumbs::apply_exif_orientation(image, orientation);
                            Ok(json!({
                                "source_width": oriented.width(),
                                "source_height": oriented.height(),
                                "exif_orientation": orientation,
                                "media_fingerprint": fingerprint,
                            }))
                        });
                    match geometry {
                        Ok(geometry) => value["source_geometry"] = geometry,
                        Err(error) => {
                            value["source_geometry_error"] = json!({
                                "code": "source_geometry_unavailable",
                                "message": error,
                            })
                        }
                    }
                }
                Err(error) => {
                    value["source_geometry_error"] = json!({
                        "code": manual_authority_error_code(&error),
                        "message": error,
                    });
                }
            }
        } else {
            value["source_geometry_error"] = json!({
                "code": "media_fingerprint_missing",
                "message": "Viewer media has no canonical Match fingerprint",
            });
        }
        Ok(value)
    }

    /// Exact track/timestamp metadata operations. Transport state stays in the
    /// Viewer; this method never seeks, draws overlays, or changes playback.
    pub fn match_video(
        &self,
        action_id: &str,
        request: &crate::api::MatchVideoRequest,
    ) -> Result<serde_json::Value, String> {
        use crate::api::MatchVideoAction as Action;
        use crate::match_store::TrackCorrectionAction;
        crate::api::validate_match_video(request)?;
        let store = self.ready_match_store()?;
        if request.action == Action::PersonAppearanceList {
            return serde_json::to_value(store.person_video_appearances_page(
                request.person_id.as_deref().ok_or("Person required")?,
                request.person_cursor.as_ref(),
                64,
            )?)
            .map_err(|error| error.to_string());
        }
        if request.action == Action::AppearanceList {
            let mut tracks = store.video_media_tracks_page(
                &request.media_key,
                request.after_track_id.as_deref().unwrap_or(""),
                65,
            )?;
            let has_more = tracks.len() > 64;
            tracks.truncate(64);
            let next_cursor = if has_more {
                tracks.last().map(|track| track.track_id.clone())
            } else {
                None
            };
            return Ok(json!({"tracks": tracks, "has_more": has_more, "next_cursor": next_cursor}));
        }
        let track =
            store.video_track_snapshot(request.track_id.as_deref().ok_or("track required")?)?;
        if track.media_key != request.media_key {
            return Err("video track is absent from the selected canonical media".into());
        }
        let timestamp = request.timestamp.ok_or("exact video timestamp required")?;
        if request.track_revision != Some(track.revision) || !track.timestamps.contains(&timestamp)
        {
            return Err(
                "video track revision or exact timestamp changed; refresh its metadata".into(),
            );
        }
        match request.action {
            Action::AppearanceList | Action::PersonAppearanceList => unreachable!(),
            Action::InspectAppearance => {
                Err("inspection pixels require the private Viewer route".into())
            }
            Action::SeekAppearance => Ok(json!({
                "media_key": track.media_key, "track_id": track.track_id,
                "track_revision": track.revision, "timestamp": timestamp,
                "playback_origin": track.playback_origin,
                "seek_ms": timestamp.playback_milliseconds(track.playback_origin)?,
            })),
            Action::CorrectionPreview | Action::CorrectionApply => {
                let action = match request.correction_action.as_deref() {
                    Some("assign") => TrackCorrectionAction::Assign,
                    Some("reassign") => TrackCorrectionAction::Reassign,
                    Some("remove") => TrackCorrectionAction::Remove,
                    Some("ignore") => TrackCorrectionAction::Ignore,
                    Some("not_a_person") => TrackCorrectionAction::NotAPerson,
                    _ => return Err("unsupported video correction action".into()),
                };
                let person_id = if request.correction_action.as_deref() == Some("remove") {
                    request.source_person_id.as_deref()
                } else {
                    request.target_person_id.as_deref()
                };
                let preview = if request.action == Action::CorrectionPreview {
                    store.preview_video_track_correction(&track.track_id, action, person_id)?
                } else {
                    match self
                        .match_video_previews
                        .lock()
                        .map_err(|_| "video preview lock poisoned")?
                        .get(
                            request
                                .preview_token
                                .as_deref()
                                .ok_or("video preview token required")?,
                        )
                        .cloned()
                    {
                        Some(MatchVideoPreview::Correction(preview)) => preview,
                        _ => {
                            return Err(
                                "video correction preview expired; request a fresh preview".into()
                            )
                        }
                    }
                };
                if preview.action != action || preview.person_id.as_deref() != person_id {
                    return Err("video correction action or target differs from the preview".into());
                }
                if matches!(
                    request.correction_action.as_deref(),
                    Some("reassign" | "remove")
                ) && preview
                    .batch
                    .as_ref()
                    .and_then(|batch| batch.source_person_id.as_deref())
                    != request.source_person_id.as_deref()
                {
                    return Err(
                        "video correction source Person does not cover the canonical track".into(),
                    );
                }
                if preview.track != track {
                    return Err("video track changed while preparing correction".into());
                }
                if request.action == Action::CorrectionPreview {
                    self.retain_match_video_preview(
                        preview.preview_id.clone(),
                        MatchVideoPreview::Correction(preview.clone()),
                    )?;
                    serde_json::to_value(preview).map_err(|error| error.to_string())
                } else {
                    if request.preview_token.as_deref() != Some(preview.preview_id.as_str()) {
                        return Err("video correction preview expired or changed".into());
                    }
                    let receipt = store.apply_video_track_correction(&preview)?;
                    Ok(json!({"action_id": action_id, "receipt": receipt}))
                }
            }
            Action::SplitPreview | Action::SplitApply => {
                let preview = if request.action == Action::SplitPreview {
                    store.preview_video_track_split(
                        &track.track_id,
                        &request.split_observation_ids,
                    )?
                } else {
                    match self
                        .match_video_previews
                        .lock()
                        .map_err(|_| "video preview lock poisoned")?
                        .get(
                            request
                                .preview_token
                                .as_deref()
                                .ok_or("video preview token required")?,
                        )
                        .cloned()
                    {
                        Some(MatchVideoPreview::Split(preview)) => preview,
                        _ => {
                            return Err(
                                "video split preview expired; request a fresh preview".into()
                            )
                        }
                    }
                };
                let selected: BTreeSet<_> = request.split_observation_ids.iter().collect();
                if selected
                    != preview
                        .selected_observation_ids
                        .iter()
                        .collect::<BTreeSet<_>>()
                {
                    return Err("video split scope differs from the preview".into());
                }
                if preview.track != track {
                    return Err("video track changed while preparing split".into());
                }
                if request.action == Action::SplitPreview {
                    self.retain_match_video_preview(
                        preview.preview_id.clone(),
                        MatchVideoPreview::Split(preview.clone()),
                    )?;
                    serde_json::to_value(preview).map_err(|error| error.to_string())
                } else {
                    if request.preview_token.as_deref() != Some(preview.preview_id.as_str()) {
                        return Err("video split preview expired or changed".into());
                    }
                    let receipt = store.apply_video_track_split(&preview)?;
                    Ok(json!({"action_id": action_id, "receipt": receipt}))
                }
            }
            Action::ContextReview => {
                let rows: Vec<_> = track
                    .observations
                    .iter()
                    .filter(|row| row.time == timestamp)
                    .collect();
                if rows.len() != 1 {
                    return Err(
                        "context review requires one exact canonical track observation".into(),
                    );
                }
                let ranked: Vec<_> = store.context_review(&rows[0].face_id, true)?.into_iter().map(|candidate| json!({
                    "person_id": candidate.person_id, "visual_score": candidate.visual_similarity,
                    "context_score": candidate.context_score(), "context": candidate.context,
                })).collect();
                Ok(json!({"ranked": ranked, "review_only": true}))
            }
        }
    }

    pub fn match_media_context_get(
        &self,
        request: &crate::api::MatchMediaContextGetRequest,
    ) -> Result<serde_json::Value, String> {
        crate::api::validate_match_media_context_get(request)?;
        let context = self
            .ready_match_store()?
            .media_context(&request.media_key)?;
        Ok(json!({"review_only": true, "context": context}))
    }

    pub fn match_media_context_replace(
        &self,
        request: &crate::api::MatchMediaContextReplaceRequest,
    ) -> Result<serde_json::Value, String> {
        request.validate()?;
        let context = self.ready_match_store()?.replace_media_context(request)?;
        Ok(json!({"review_only": true, "context": context}))
    }

    pub fn match_cluster_review(
        &self,
        request: &crate::api::MatchClusterReviewRequest,
    ) -> Result<serde_json::Value, String> {
        crate::api::validate_match_cluster_review(request)?;
        let review = self.ready_match_store()?.review_unnamed_clusters(request)?;
        serde_json::to_value(review).map_err(|error| error.to_string())
    }

    fn retain_match_video_preview(
        &self,
        token: String,
        preview: MatchVideoPreview,
    ) -> Result<(), String> {
        let mut pending = self
            .match_video_previews
            .lock()
            .map_err(|_| "video preview lock poisoned")?;
        if pending.len() >= 32 {
            pending.pop_first();
        }
        pending.insert(token, preview);
        Ok(())
    }

    pub fn match_person_faces(
        &self,
        person_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(
            self.ready_match_store()?
                .person_face_page(person_id, offset, limit)?,
        )
        .map_err(|error| error.to_string())
    }

    /// Return exact receipt-backed remove and optional merge preflights from
    /// the same store instance that will later apply the correction. Each
    /// preview digest binds its complete reversible-row inventory.
    pub fn match_person_edit_preflights(
        &self,
        source_person_id: &str,
        target_person_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let store = self.ready_match_store()?;
        let remove = store.preview_remove_person(source_person_id)?;
        let merge = target_person_id
            .map(|target_person_id| store.preview_merge_people(source_person_id, target_person_id))
            .transpose()?;
        Ok(json!({
            "remove": remove,
            "merge": merge,
        }))
    }

    pub fn match_split_to_person_preflight(
        &self,
        source_person_id: &str,
        target_person_id: &str,
        face_ids: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(self.ready_match_store()?.preview_split_to_person(
            source_person_id,
            target_person_id,
            face_ids,
        )?)
        .map_err(|error| error.to_string())
    }

    /// Receipt-facing exact split preflight. Unlike the in-app helper above,
    /// this model-operability route verifies every optimistic revision and
    /// Face/media fence before returning a preview token.
    pub fn match_split_person_preflight(
        &self,
        request: &crate::api::MatchSplitPersonPreflightRequest,
    ) -> Result<serde_json::Value, String> {
        let store = self.ready_match_store()?;
        let status = store.status()?;
        let current_schema = status
            .get("schema_generation")
            .and_then(serde_json::Value::as_str)
            .ok_or("Match status omitted schema_generation")?;
        let current_model = status
            .get("active_model_generation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("match-model-unconfigured");
        let current_catalog = status
            .pointer("/execution/catalog_revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or("Match status omitted catalog_revision")?;
        if current_schema != request.expected_revisions.schema_generation
            || current_model != request.expected_revisions.model_generation
            || current_catalog != request.expected_revisions.catalog_revision
        {
            return Err(
                "stale Match split preflight schema/model/catalog revision fence".to_string(),
            );
        }

        let referenced_people = vec![
            request.source_person_id.clone(),
            request.target_person_id.clone(),
        ];
        for face_id in &request.face_ids {
            let fence = store.correction_fence_for(face_id, referenced_people.clone())?;
            if request.expected_revisions.face_revisions.get(face_id) != Some(&fence.face_revision)
            {
                return Err("stale Match split preflight Face revision fence".to_string());
            }
            for (person_id, revision) in &request.expected_revisions.person_revisions {
                if fence.person_revisions.get(person_id) != Some(revision) {
                    return Err("stale Match split preflight Person revision fence".to_string());
                }
            }
            let media = request
                .face_media
                .get(face_id)
                .ok_or("split preflight missing Face media fence")?;
            if fence.media_key != media.media_key
                || fence.media_fingerprint != media.media_fingerprint
            {
                return Err("stale Match split preflight Face/media fence".to_string());
            }
            let media_snapshot = store.media_faces(&media.media_key)?;
            let canonical_face = media_snapshot
                .rows
                .iter()
                .find(|row| row.face.face_id.as_str() == face_id.as_str())
                .ok_or("split preflight Face does not belong to the requested media")?;
            if canonical_face.face.media_fingerprint.as_str() != media.media_fingerprint.as_str() {
                return Err("stale Match split preflight media fingerprint".to_string());
            }
        }

        let preview = store.preview_split_to_person(
            &request.source_person_id,
            &request.target_person_id,
            request.face_ids.clone(),
        )?;
        if preview.catalog_revision != request.expected_revisions.catalog_revision
            || preview.person_revisions != request.expected_revisions.person_revisions
            || preview.face_ids != request.face_ids
        {
            return Err("stale Match split preflight canonical preview fence".to_string());
        }
        serde_json::to_value(preview).map_err(|error| error.to_string())
    }

    pub fn match_autocomplete(
        &self,
        query: &str,
        expected_catalog_revision: u64,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(self.ready_match_store()?.correction_autocomplete(
            query,
            expected_catalog_revision,
            limit,
        )?)
        .map_err(|error| error.to_string())
    }

    /// Build the exact action-specific authorization for one multi-Face
    /// correction. This is read-only; the returned full typed preview is the
    /// only payload accepted by the apply route.
    pub fn match_batch_correction_preflight(
        &self,
        request: &crate::api::MatchCorrectionRequest,
    ) -> Result<serde_json::Value, String> {
        let action = match_batch_action(request.action)
            .ok_or("unsupported Match batch correction preflight action")?;
        if request.face_ids.len() < 2
            || request.operation_id.is_some()
            || request.batch_preview.is_some()
            || request.confirmed
        {
            return Err(
                "batch correction preflight requires >=2 faces and preview-only request fields"
                    .to_string(),
            );
        }
        let store = self.ready_match_store()?;
        let status = store.status()?;
        let current_schema = status
            .get("schema_generation")
            .and_then(serde_json::Value::as_str)
            .ok_or("Match status omitted schema_generation")?;
        let current_model = status
            .get("active_model_generation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("match-model-unconfigured");
        let current_catalog = status
            .pointer("/execution/catalog_revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or("Match status omitted catalog_revision")?;
        if current_schema != request.expected_revisions.schema_generation
            || current_model != request.expected_revisions.model_generation
            || current_catalog != request.expected_revisions.catalog_revision
        {
            return Err("stale Match batch preflight schema/model/catalog revision fence".into());
        }

        let referenced_people = request
            .person_id
            .iter()
            .chain(request.target_person_id.iter())
            .cloned()
            .collect::<Vec<_>>();
        let mut fences = Vec::with_capacity(request.face_ids.len());
        for face_id in &request.face_ids {
            let fence = store.correction_fence_for(face_id, referenced_people.clone())?;
            let media = request
                .face_media
                .get(face_id)
                .ok_or("batch preflight missing exact Face media fence")?;
            if request.expected_revisions.face_revisions.get(face_id) != Some(&fence.face_revision)
                || fence.person_revisions != request.expected_revisions.person_revisions
                || fence.media_key != media.media_key
                || fence.media_fingerprint != media.media_fingerprint
                || fence.schema_generation != request.expected_revisions.schema_generation
                || fence.model_generation != request.expected_revisions.model_generation
                || fence.catalog_revision != request.expected_revisions.catalog_revision
            {
                return Err("stale Match batch preflight exact correction fence".to_string());
            }
            let snapshot = store.media_faces(&media.media_key)?;
            let canonical_face = snapshot
                .rows
                .iter()
                .find(|row| row.face.face_id.as_str() == face_id.as_str())
                .ok_or("batch preflight Face does not belong to requested media")?;
            if canonical_face.face.media_fingerprint != media.media_fingerprint {
                return Err("stale Match batch preflight media fingerprint".to_string());
            }
            fences.push(fence);
        }
        serde_json::to_value(store.batch_correction_preflight(
            action,
            request.person_id.as_deref(),
            request.target_person_id.as_deref(),
            request.face_ids.clone(),
            fences,
        )?)
        .map_err(|error| error.to_string())
    }

    pub fn match_apply_correction(
        &mut self,
        request: &crate::api::MatchCorrectionRequest,
    ) -> Result<serde_json::Value, String> {
        use crate::api::MatchCorrectionAction as Action;
        use crate::match_store::{
            ExifOrientation, ManualEmbeddingInput, ManualFaceInput, NormalizedRegion,
        };

        let store = self.ready_match_store()?;
        let status = store.status()?;
        let current_schema = status
            .get("schema_generation")
            .and_then(serde_json::Value::as_str)
            .ok_or("Match status omitted schema_generation")?;
        let current_model = status
            .get("active_model_generation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("match-model-unconfigured");
        let current_catalog = status
            .pointer("/execution/catalog_revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or("Match status omitted catalog_revision")?;
        if current_schema != request.expected_revisions.schema_generation
            || current_model != request.expected_revisions.model_generation
            || current_catalog != request.expected_revisions.catalog_revision
        {
            return Err("stale Match correction schema/model/catalog revision fence".to_string());
        }

        let referenced_people = request
            .person_id
            .iter()
            .chain(request.target_person_id.iter())
            .cloned()
            .collect::<Vec<_>>();
        let fence_for = |face_id: &str| {
            let fence = store.correction_fence_for(face_id, referenced_people.clone())?;
            if request.expected_revisions.face_revisions.get(face_id) != Some(&fence.face_revision)
            {
                return Err("stale Match correction Face revision fence".to_string());
            }
            for (person_id, revision) in &request.expected_revisions.person_revisions {
                if fence.person_revisions.get(person_id) != Some(revision) {
                    return Err("stale Match correction Person revision fence".to_string());
                }
            }
            Ok(fence)
        };
        let verify_media_scope = |face_id: &str| -> Result<(), String> {
            let (media_key, fingerprint) = if let Some(media) = request.face_media.get(face_id) {
                (media.media_key.as_str(), media.media_fingerprint.as_str())
            } else {
                (
                    request.media_key.as_deref().ok_or("missing media_key")?,
                    request
                        .media_fingerprint
                        .as_deref()
                        .ok_or("missing media_fingerprint")?,
                )
            };
            let snapshot = store.media_faces(media_key)?;
            let face = snapshot
                .rows
                .iter()
                .find(|row| row.face.face_id == face_id)
                .ok_or("correction Face does not belong to the requested media")?;
            if face.face.media_fingerprint != fingerprint {
                return Err("stale Match correction media fingerprint".to_string());
            }
            Ok(())
        };
        let one_face = || -> Result<&str, String> {
            match request.face_ids.as_slice() {
                [face_id] => Ok(face_id.as_str()),
                [] => Err("correction requires one FaceId".to_string()),
                _ => Err("correction requires exactly one FaceId".to_string()),
            }
        };

        let value = match request.action {
            Action::ManualFace => {
                let bounds = request
                    .normalized_bounds
                    .as_ref()
                    .ok_or("manual_face requires normalized_bounds")?;
                let orientation = match request
                    .exif_orientation
                    .ok_or("missing EXIF orientation")?
                {
                    1 => ExifOrientation::Normal,
                    2 => ExifOrientation::MirrorHorizontal,
                    3 => ExifOrientation::Rotate180,
                    4 => ExifOrientation::MirrorVertical,
                    5 => ExifOrientation::Transpose,
                    6 => ExifOrientation::Rotate90Clockwise,
                    7 => ExifOrientation::Transverse,
                    8 => ExifOrientation::Rotate90CounterClockwise,
                    _ => return Err("manual_face EXIF orientation must be in 1..=8".to_string()),
                };
                let media_key = request
                    .media_key
                    .as_deref()
                    .ok_or("manual_face missing media_key")?;
                let media_fingerprint = request
                    .media_fingerprint
                    .as_deref()
                    .ok_or("manual_face missing media_fingerprint")?;
                let authority = store.manual_media_authority(media_key, media_fingerprint)?;
                let root_path = authority
                    .root_path
                    .canonicalize()
                    .map_err(|error| format!("canonicalize manual face root: {error}"))?;
                let snapshot = match_file_snapshot(&authority.source_path, &root_path)?;
                if !match_source_fingerprint_matches(
                    &snapshot.fingerprint,
                    &authority.media_fingerprint,
                ) || !match_source_fingerprint_matches(&snapshot.fingerprint, media_fingerprint)
                {
                    return Err("stale manual face source bytes fingerprint".to_string());
                }
                match_decoded_working_bytes(&snapshot.bytes, &snapshot.final_path)?;
                let actual_orientation = crate::media_thumbs::exif_orientation(&snapshot.bytes);
                if actual_orientation != u32::from(request.exif_orientation.unwrap_or(0)) {
                    return Err("stale manual face EXIF orientation".to_string());
                }
                let decoded = image::load_from_memory(&snapshot.bytes)
                    .map_err(|error| format!("decode canonical manual face source: {error}"))?;
                let raw_width = decoded.width();
                let raw_height = decoded.height();
                let oriented =
                    crate::media_thumbs::apply_exif_orientation(decoded, actual_orientation);
                if oriented.width() != bounds.source_width
                    || oriented.height() != bounds.source_height
                {
                    return Err("stale manual face source dimensions".to_string());
                }
                let display_region = NormalizedRegion {
                    x: bounds.left,
                    y: bounds.top,
                    width: bounds.width,
                    height: bounds.height,
                };
                let source_region = display_region.display_to_source(orientation)?;
                self.ensure_landmarks();
                let oriented_rgb = oriented.to_rgb8();
                let manual_bbox = [
                    display_region.x * bounds.source_width as f32,
                    display_region.y * bounds.source_height as f32,
                    display_region.width * bounds.source_width as f32,
                    display_region.height * bounds.source_height as f32,
                ];
                let manual_landmarks = match self.landmarks.as_ref() {
                    Some(engine) => match engine.analyze(&oriented_rgb, manual_bbox) {
                        Ok(analysis) => Some(analysis),
                        Err(error) => {
                            self.debug.emit(
                                "WARN",
                                "Match",
                                "manual face landmark analysis failed",
                                Some(json!({
                                    "media_key": media_key,
                                    "validation_code": "analysis_failed",
                                    "detail": error,
                                    "embedding_admitted": false,
                                })),
                            );
                            None
                        }
                    },
                    None => {
                        self.debug.emit(
                            "WARN",
                            "Match",
                            "manual face landmark engine is unavailable",
                            Some(json!({
                                "media_key": media_key,
                                "validation_code": "landmark_engine_unavailable",
                                "embedding_admitted": false,
                            })),
                        );
                        None
                    }
                };
                let landmark_validity = manual_landmarks.as_ref().map(|analysis| {
                    analysis.manual_alignment_validity(
                        bounds.source_width,
                        bounds.source_height,
                        manual_bbox,
                    )
                });
                if let Some(crate::landmarks::ManualLandmarkValidity::Invalid(reason)) =
                    landmark_validity
                {
                    self.debug.emit(
                        "WARN",
                        "Match",
                        &format!("manual face landmark alignment rejected: {}", reason.code()),
                        Some(json!({
                            "media_key": media_key,
                            "validation_code": reason.code(),
                            "embedding_admitted": false,
                        })),
                    );
                }
                let validated_manual_landmarks = manual_landmarks.as_ref().filter(|_| {
                    landmark_validity
                        .is_some_and(crate::landmarks::ManualLandmarkValidity::is_valid)
                });
                let mut display_landmarks = validated_manual_landmarks
                    .as_ref()
                    .map(|analysis| {
                        analysis
                            .points
                            .iter()
                            .map(|[x, y]| {
                                vec![
                                    (*x / bounds.source_width as f32).clamp(0.0, 1.0),
                                    (*y / bounds.source_height as f32).clamp(0.0, 1.0),
                                ]
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let mut alignment_valid = false;
                let mut quality = 0.0;
                let mut pose_bucket = "manual_unaligned".to_string();
                let mut embedding = None;
                if let Some(engine) = self.identity.as_ref().filter(|engine| {
                    engine.generation() == request.expected_revisions.model_generation
                }) {
                    if let Ok(batch) =
                        engine.embed_faces_bytes(&snapshot.bytes, &snapshot.final_path)
                    {
                        if batch.image_w != raw_width || batch.image_h != raw_height {
                            return Err(
                                "manual face detector decoded different dimensions".to_string()
                            );
                        }
                        if let Some((overlap, detected)) = batch
                            .faces
                            .into_iter()
                            .map(|face| {
                                (
                                    normalized_region_iou(source_region, face.bbox_normalized),
                                    face,
                                )
                            })
                            .max_by(|left, right| left.0.total_cmp(&right.0))
                            .filter(|(overlap, _)| *overlap >= 0.5)
                        {
                            let _ = overlap;
                            if manual_embedding_admitted(
                                landmark_validity,
                                detected.quality.alignment_valid,
                            ) {
                                alignment_valid = detected.quality.alignment_valid;
                                quality = detected.quality.detection_score;
                                pose_bucket = crate::identity::yaw_bucket(&detected.face.landmarks)
                                    .0
                                    .to_string();
                                embedding = Some(ManualEmbeddingInput {
                                    vector: detected.embedding.values().to_vec(),
                                    model_generation: detected.generation,
                                });
                            } else {
                                display_landmarks.clear();
                            }
                        }
                    }
                }
                let input = ManualFaceInput {
                    expected_schema_generation: request
                        .expected_revisions
                        .schema_generation
                        .clone(),
                    expected_model_generation: request.expected_revisions.model_generation.clone(),
                    expected_catalog_revision: request.expected_revisions.catalog_revision,
                    media_key: authority.media_key,
                    media_fingerprint: authority.media_fingerprint,
                    authority: authority.fence,
                    source_width: bounds.source_width,
                    source_height: bounds.source_height,
                    display_region,
                    exif_orientation: orientation,
                    display_landmarks,
                    alignment_valid,
                    quality,
                    pose_bucket,
                    embedding,
                };
                if let Some(person_id) = request.person_id.as_deref() {
                    let expected_person_revision = *request
                        .expected_revisions
                        .person_revisions
                        .get(person_id)
                        .ok_or("manual assignment missing Person revision")?;
                    serde_json::to_value(store.create_manual_face_and_assign(
                        input,
                        person_id,
                        expected_person_revision,
                    )?)
                } else {
                    serde_json::to_value(store.create_manual_face(input)?)
                }
            }
            Action::MergePeople => {
                let preview = store.preview_merge_people(
                    request
                        .person_id
                        .as_deref()
                        .ok_or("merge missing source Person")?,
                    request
                        .target_person_id
                        .as_deref()
                        .ok_or("merge missing target Person")?,
                )?;
                verify_person_preview_fence(request, &preview)?;
                serde_json::to_value(store.merge_people(&preview)?)
            }
            Action::SplitPerson => {
                // Split is preview-bound at the complete source-Person
                // inventory level, but its selected subset still carries
                // exact per-Face media and revision fences. Carry those
                // fences through to the store so it can revalidate them
                // atomically with preview comparison and correction commit.
                let mut fences = Vec::with_capacity(request.face_ids.len());
                for face_id in &request.face_ids {
                    verify_media_scope(face_id)?;
                    let fence = fence_for(face_id)?;
                    let (expected_media_key, expected_media_fingerprint) = request
                        .face_media
                        .get(face_id)
                        .map(|media| (media.media_key.as_str(), media.media_fingerprint.as_str()))
                        .or_else(|| {
                            Some((
                                request.media_key.as_deref()?,
                                request.media_fingerprint.as_deref()?,
                            ))
                        })
                        .ok_or("missing split media fence")?;
                    if fence.media_key != expected_media_key
                        || fence.media_fingerprint != expected_media_fingerprint
                    {
                        return Err("stale Match correction media fence".to_string());
                    }
                    fences.push(fence);
                }
                let preview = store.preview_split_to_person(
                    request
                        .person_id
                        .as_deref()
                        .ok_or("split missing source Person")?,
                    request
                        .target_person_id
                        .as_deref()
                        .ok_or("split missing target Person")?,
                    request.face_ids.clone(),
                )?;
                verify_person_preview_fence(request, &preview)?;
                serde_json::to_value(store.split_to_person(&preview, fences)?)
            }
            Action::RemovePerson => {
                let preview = store.preview_remove_person(
                    request
                        .person_id
                        .as_deref()
                        .ok_or("remove missing Person")?,
                )?;
                verify_person_preview_fence(request, &preview)?;
                serde_json::to_value(store.remove_person(&preview)?)
            }
            Action::Undo => serde_json::to_value(
                store.undo_correction(
                    request
                        .operation_id
                        .as_deref()
                        .ok_or("undo missing operation_id")?,
                )?,
            ),
            action => {
                if request.face_ids.len() > 1 {
                    let expected_action = match_batch_action(action)
                        .ok_or("batch Look placement is not supported; select one face")?;
                    let preview = request
                        .batch_preview
                        .as_ref()
                        .ok_or("batch correction requires its full exact preview")?;
                    if request.operation_id.as_deref() != Some(preview.preview_id.as_str()) {
                        return Err(
                            "batch correction operation_id differs from exact preview".to_string()
                        );
                    }
                    let preview_face_revisions = preview
                        .fences
                        .iter()
                        .map(|fence| (fence.face_id.clone(), fence.face_revision))
                        .collect::<BTreeMap<_, _>>();
                    let preview_face_media = preview
                        .fences
                        .iter()
                        .map(|fence| {
                            (
                                fence.face_id.clone(),
                                crate::api::MatchFaceMediaFence {
                                    media_key: fence.media_key.clone(),
                                    media_fingerprint: fence.media_fingerprint.clone(),
                                },
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    if preview.action != expected_action
                        || preview.source_person_id.as_deref() != request.person_id.as_deref()
                        || preview.target_person_id.as_deref()
                            != request.target_person_id.as_deref()
                        || preview.face_ids != request.face_ids
                        || preview.schema_generation != request.expected_revisions.schema_generation
                        || preview.model_generation != request.expected_revisions.model_generation
                        || preview.catalog_revision != request.expected_revisions.catalog_revision
                        || preview_face_revisions != request.expected_revisions.face_revisions
                        || preview_face_media != request.face_media
                        || preview.fences.iter().any(|fence| {
                            fence.person_revisions != request.expected_revisions.person_revisions
                        })
                    {
                        return Err(
                            "batch correction request differs from its exact preview".to_string()
                        );
                    }
                    let receipt = store.apply_batch_correction(preview)?;
                    return serde_json::to_value(receipt).map_err(|error| error.to_string());
                }
                let face_id = one_face()?;
                verify_media_scope(face_id)?;
                let fence = fence_for(face_id)?;
                let person_id = request.person_id.as_deref();
                let receipt = match action {
                    Action::Same => store.same_correction(
                        face_id,
                        person_id.ok_or("Same missing Person")?,
                        &fence,
                    )?,
                    Action::Different => store.different_correction(
                        face_id,
                        person_id.ok_or("Different missing Person")?,
                        &fence,
                    )?,
                    Action::NotSure => store.not_sure_correction(
                        face_id,
                        person_id.ok_or("Not sure missing Person")?,
                        &fence,
                    )?,
                    Action::ThisIsNot => store.this_is_not_correction(
                        face_id,
                        person_id.ok_or("This-is-not missing Person")?,
                        &fence,
                    )?,
                    Action::ChangePerson => store.change_person_correction(
                        face_id,
                        person_id.ok_or("Change person missing source Person")?,
                        request
                            .target_person_id
                            .as_deref()
                            .ok_or("Change person missing target Person")?,
                        &fence,
                    )?,
                    Action::RemoveAssignment => store.remove_assignment_correction(
                        face_id,
                        person_id.ok_or("Remove assignment missing source Person")?,
                        &fence,
                    )?,
                    Action::IgnoreFace => store.ignore_face(face_id, &fence)?,
                    Action::NotAFace => store.mark_not_a_face(face_id, &fence)?,
                    Action::DeleteFaceAnalysis => store.delete_face_analysis(face_id, &fence)?,
                    Action::MoveToLook => store.move_to_look_correction(
                        face_id,
                        request.look_id.as_deref().ok_or("move missing Look")?,
                        &fence,
                    )?,
                    Action::SamePersonNewLook => store.same_person_new_look_correction(
                        face_id,
                        person_id.ok_or("new Look missing Person")?,
                        request
                            .look_name
                            .as_deref()
                            .ok_or("new Look missing name")?,
                        &fence,
                    )?,
                    Action::ManualFace
                    | Action::MergePeople
                    | Action::SplitPerson
                    | Action::RemovePerson
                    | Action::Undo => unreachable!("handled above"),
                };
                serde_json::to_value(receipt)
            }
        }
        .map_err(|error| error.to_string())?;
        Ok(value)
    }

    pub fn match_configure_root(
        &self,
        path: &str,
        exclusions: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(
            self.ready_match_store()?
                .configure_index_root(Path::new(path), exclusions)?,
        )
        .map_err(|error| error.to_string())
    }

    pub fn match_remove_root(&self, root_id: &str) -> Result<(), String> {
        self.ready_match_store()?.remove_index_root(root_id)
    }

    pub fn match_start_job(&self, root_id: &str) -> Result<serde_json::Value, String> {
        self.match_start_job_with_io(
            root_id,
            Arc::new(crate::media_io::MediaIoCoordinator::new()),
        )
    }

    pub fn match_start_job_with_io(
        &self,
        root_id: &str,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<serde_json::Value, String> {
        let generation = self
            .identity
            .as_ref()
            .map(|engine| engine.generation().to_string())
            .ok_or_else(|| {
                self.identity_load_error
                    .as_ref()
                    .map(|error| error.code.clone())
                    .unwrap_or_else(|| "identity_model_unavailable".to_string())
            })?;
        let store = self.ready_match_store()?;
        store.register_model_generation(&generation, true)?;
        store.activate_model_generation(&generation)?;
        let mut job = store.start_index_job(root_id, &generation)?;
        if store.can_attempt_automatic(job.lifecycle()?)? {
            job =
                store.set_job_lifecycle(&job.job_id, crate::match_store::JobLifecycle::Running)?;
            self.spawn_match_job_worker(store, &job.job_id, coordinator)?;
        }
        serde_json::to_value(job).map_err(|error| error.to_string())
    }

    fn spawn_match_job_worker(
        &self,
        store: crate::match_store::MatchStore,
        job_id: &str,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<(), String> {
        let manifest_path = self
            .config
            .identity_manifest_path
            .clone()
            .ok_or("identity_manifest_required_for_match_indexing")?;
        let worker_runtime = Arc::clone(&self.match_store);
        let cancelled = {
            let mut runtime = worker_runtime
                .lock()
                .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
            if !runtime.active_index_jobs.insert(job_id.to_string()) {
                runtime.pending_index_jobs.insert(job_id.to_string());
                return Ok(());
            }
            runtime.cancelled.clone()
        };
        let worker_store = store.clone();
        let cpu_policy = self.match_cpu_benchmark_policy.unwrap_or_default();
        let worker_job_id = job_id.to_string();
        let worker_name = format!("facial-match-index-{}", &job_id[..job_id.len().min(12)]);
        let (start_tx, start_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let spawn_result = std::thread::Builder::new()
            .name(worker_name)
            .spawn(move || {
                if start_rx.recv().is_err() {
                    return;
                }
                let mut worker = None;
                loop {
                    let result = run_match_index_job(
                        worker_store.clone(),
                        worker_job_id.clone(),
                        &mut worker,
                        &manifest_path,
                        Arc::clone(&coordinator),
                        Arc::clone(&cancelled),
                        cpu_policy,
                    );
                    if let Err(error) = &result {
                        if match_database_requires_recovery(error) {
                            worker_store.record_pending_database_failure(
                                &worker_job_id, match_failure_code(error), error,
                            );
                            let _ = worker_store
                                .add_hold(crate::match_store::HoldReason::DatabaseOwnerQuarantined);
                        }
                        if !match_admission_interrupted(error)
                            && !match_database_outcome_unknown(error)
                        {
                            match worker_store.job(&worker_job_id) {
                                Ok(job) => {
                                if matches!(
                                    job.lifecycle.as_str(),
                                    "running" | "retrying" | "pausing"
                                ) {
                                    if let Err(persistence_error) = worker_store.record_job_failure(
                                        &worker_job_id,
                                        match_failure_code(error),
                                        error,
                                    ) {
                                        worker_store.record_pending_database_failure(
                                            &worker_job_id, match_failure_code(error), &persistence_error,
                                        );
                                        if match_database_requires_recovery(&persistence_error) {
                                            let _ = worker_store.add_hold(crate::match_store::HoldReason::DatabaseOwnerQuarantined);
                                        }
                                        eprintln!("Match job {} failure was not acknowledged: {}", worker_job_id, persistence_error);
                                    }
                                }
                                }
                                Err(lookup_error) => {
                                    worker_store.record_pending_database_failure(
                                        &worker_job_id, match_failure_code(error), &lookup_error,
                                    );
                                }
                            }
                            eprintln!("Match indexing worker {} failed: {}", worker_job_id, error);
                        }
                    }
                    let rerun = worker_runtime
                        .lock()
                        .map(|mut runtime| {
                            if let Some(acknowledgement) = worker
                                .as_ref()
                                .and_then(|child| child.production_cpu_acknowledgement())
                            {
                                runtime.last_cpu_executor_acknowledgement = Some(acknowledgement);
                            }
                            settle_match_worker_iteration(&mut runtime, &worker_job_id)
                        })
                        .unwrap_or(false);
                    if !rerun {
                        break;
                    }
                }
            });
        match spawn_result {
            Ok(worker) => {
                let mut runtime = self
                    .match_store
                    .lock()
                    .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
                runtime.index_workers.insert(job_id.to_string(), worker);
                drop(runtime);
                if start_tx.send(()).is_err() {
                    let message = "start Match indexing worker: worker exited early";
                    if let Err(error) = store.record_job_failure(job_id, "internal", message) {
                        store.record_pending_database_failure(job_id, "internal", &error);
                        let _ = store
                            .add_hold(crate::match_store::HoldReason::DatabaseOwnerQuarantined);
                    }
                    return Err(message.to_string());
                }
                Ok(())
            }
            Err(error) => {
                if let Ok(mut runtime) = self.match_store.lock() {
                    runtime.active_index_jobs.remove(job_id);
                }
                let message = format!("spawn Match indexing worker: {error}");
                if let Err(error) = store.record_job_failure(job_id, "internal", &message) {
                    store.record_pending_database_failure(job_id, "internal", &error);
                    let _ =
                        store.add_hold(crate::match_store::HoldReason::DatabaseOwnerQuarantined);
                }
                Err(message)
            }
        }
    }

    fn reconcile_match_job_workers(
        &self,
        store: crate::match_store::MatchStore,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<(), String> {
        for mut job in store.jobs()? {
            let lifecycle = job.lifecycle()?;
            if !store.can_attempt_automatic(lifecycle)? {
                continue;
            }
            if lifecycle == crate::match_store::JobLifecycle::Queued {
                job = store
                    .set_job_lifecycle(&job.job_id, crate::match_store::JobLifecycle::Running)?;
            }
            if matches!(
                job.lifecycle()?,
                crate::match_store::JobLifecycle::Running
                    | crate::match_store::JobLifecycle::Retrying
            ) {
                self.spawn_match_job_worker(store.clone(), &job.job_id, Arc::clone(&coordinator))?;
            }
        }
        Ok(())
    }

    pub fn match_control_job(
        &self,
        job_id: &str,
        action: &str,
    ) -> Result<serde_json::Value, String> {
        self.match_control_job_with_io(
            job_id,
            action,
            Arc::new(crate::media_io::MediaIoCoordinator::new()),
        )
    }

    pub fn match_control_job_with_io(
        &self,
        job_id: &str,
        action: &str,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<serde_json::Value, String> {
        let store = self.ready_match_store()?;
        let job = store.control_job(job_id, action)?;
        if matches!(action, "resume" | "retry") {
            self.spawn_match_job_worker(store, &job.job_id, coordinator)?;
        }
        serde_json::to_value(job).map_err(|error| error.to_string())
    }

    pub fn match_set_operator_paused(&self, paused: bool) -> Result<(), String> {
        self.match_set_operator_paused_with_io(
            paused,
            Arc::new(crate::media_io::MediaIoCoordinator::new()),
        )
    }

    pub fn match_set_operator_paused_with_io(
        &self,
        paused: bool,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<(), String> {
        let store = self.ready_match_store()?;
        if !paused {
            store.reconcile_database_operations()?;
            for job in store.jobs()? {
                store.job_assets(&job.job_id)?;
            }
            store.resolve_pending_database_failures(None)?;
            store.remove_hold(crate::match_store::HoldReason::DatabaseOwnerQuarantined)?;
        }
        store.set_desired_mode(if paused {
            crate::match_store::DesiredMode::OperatorPaused
        } else {
            crate::match_store::DesiredMode::Running
        })?;
        if !paused {
            self.reconcile_match_job_workers(store, coordinator)?;
        }
        Ok(())
    }

    pub fn match_status(&self) -> Result<serde_json::Value, String> {
        start_match_store_initialization(&self.match_store)?;
        let worker = self
            .match_store
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?
            .worker
            .take();
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| "match_store_worker_panicked".to_string())?;
        }
        let (store, error_code, initializing) = {
            let state = self
                .match_store
                .lock()
                .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
            (
                state.store.clone(),
                state.error_code.clone(),
                state.initializing,
            )
        };
        if let Some(store) = store {
            store.status()
        } else if let Some(error_code) = error_code {
            Err(error_code)
        } else if initializing {
            Ok(serde_json::json!({
                "availability": "initializing",
                "privacy": {
                    "embeddings_in_status": false,
                    "face_crops_in_status": false,
                    "database_handles_in_status": false
                }
            }))
        } else {
            Ok(serde_json::json!({
                "availability": "disabled",
                "privacy": {
                    "embeddings_in_status": false,
                    "face_crops_in_status": false,
                    "database_handles_in_status": false
                }
            }))
        }
    }

    pub fn match_calibration_verify(
        &self,
        contract: &str,
        evaluation_root: &str,
    ) -> Result<serde_json::Value, String> {
        let verification =
            verify_match_calibration(Path::new(contract), Path::new(evaluation_root));
        if verification.verified_claim().is_some() || verification.observed_trial_claim().is_some()
        {
            start_match_store_initialization(&self.match_store)?;
            let worker = self
                .match_store
                .lock()
                .map_err(|_| "Match store runtime lock is poisoned".to_string())?
                .worker
                .take();
            if let Some(worker) = worker {
                worker
                    .join()
                    .map_err(|_| "match_store_worker_panicked".to_string())?;
            }
            let store = {
                let state = self
                    .match_store
                    .lock()
                    .map_err(|_| "Match store runtime lock is poisoned".to_string())?;
                state.store.clone().ok_or_else(|| {
                    state.error_code.clone().unwrap_or_else(|| {
                        "match_store_unavailable_for_calibration_spending".to_string()
                    })
                })?
            };
            if verification.verified_claim().is_some() {
                // Registration consumes the observed trial before checking the
                // live activation prerequisites, so even a later mismatch is
                // permanently spent.
                store.register_calibration_verification(&verification)?;
            } else {
                store.consume_calibration_observation(&verification)?;
            }
        }
        serde_json::to_value(verification)
            .map_err(|_| "match_calibration_receipt_serialization_failed".to_string())
    }

    pub fn set_match_immersive_fullscreen(&self, active: bool) -> Result<(), String> {
        self.set_match_immersive_fullscreen_with_io(
            active,
            Arc::new(crate::media_io::MediaIoCoordinator::new()),
        )
    }

    pub fn set_match_immersive_fullscreen_with_io(
        &self,
        active: bool,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<(), String> {
        self.external_match_holds.set_fullscreen(active);
        self.reconcile_match_external_holds(coordinator)
    }

    pub fn match_external_holds(&self) -> Arc<crate::match_store::MatchExternalHolds> {
        Arc::clone(&self.external_match_holds)
    }

    /// Run off the render thread. Re-read current holds, never replay an old
    /// event's requested state, and never initialize a dormant Match store.
    pub fn reconcile_match_external_holds(
        &self,
        coordinator: Arc<crate::media_io::MediaIoCoordinator>,
    ) -> Result<(), String> {
        if self.external_match_holds.blocked() {
            return Ok(());
        }
        let store = self
            .match_store
            .lock()
            .map_err(|_| "Match store runtime lock is poisoned".to_string())?
            .store
            .clone();
        if let Some(store) = store {
            self.reconcile_match_job_workers(store, coordinator)?;
        }
        Ok(())
    }

    /// Current copy/output location, if set. Drives the run/sort gate.
    pub fn copy_location(&self) -> Option<&Path> {
        self.copy_location.as_deref()
    }

    pub fn artifact_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.config.worktrees_root.clone(),
            self.config.api_root.clone(),
        ];
        if let Some(copy_location) = self.copy_location.clone() {
            roots.push(copy_location);
        }
        for path in &self.run_results_index {
            if let Some(run_dir) = path.parent() {
                roots.push(run_dir.to_path_buf());
            }
        }
        roots
    }

    /// Set + persist the copy/output destination (creates it). Required before
    /// any run or sort can start.
    pub fn set_copy_location(&mut self, path: &str) -> Result<String, String> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err("copy location cannot be empty".to_string());
        }
        let pb = PathBuf::from(trimmed);
        fs::create_dir_all(&pb).map_err(|err| format!("could not create copy location: {err}"))?;
        self.copy_location = Some(pb.clone());
        let _ = crate::config::save_copy_location(&self.config, &self.copy_location);
        self.debug.emit(
            "INFO",
            "Service",
            &format!("copy location set: {}", pb.display()),
            None,
        );
        Ok(pb.to_string_lossy().to_string())
    }

    pub fn set_workspace_root(&mut self, path: &str) -> Result<String, String> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err("workspace root cannot be empty".to_string());
        }
        let root = PathBuf::from(trimmed);
        fs::create_dir_all(&root)
            .map_err(|err| format!("could not create workspace root: {err}"))?;
        let state_root = root.join(".facial");
        let data_root = state_root.join("data");
        let worktrees_root = state_root.join("worktrees");
        fs::create_dir_all(&data_root)
            .map_err(|err| format!("could not create workspace data root: {err}"))?;
        fs::create_dir_all(&worktrees_root)
            .map_err(|err| format!("could not create workspace worktrees root: {err}"))?;

        self.config.workspace_root = root.clone();
        self.config.worktrees_root = worktrees_root.clone();
        self.config.model_registry_path = data_root.join("model_registry.json");
        self.config.debug_log_path = data_root.join("events.jsonl");
        self.config.api_root = data_root.join("api");
        self.retired_match_workers
            .extend(cancel_match_store_runtime(&self.match_store));
        let mut active_workers = Vec::new();
        for worker in self.retired_match_workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                active_workers.push(worker);
            }
        }
        self.retired_match_workers = active_workers;
        self.match_store = match_store_runtime(root.clone());
        self.match_store.lock().unwrap().external_holds = Arc::clone(&self.external_match_holds);
        self.worktrees = WorktreeManager {
            root: worktrees_root,
        };
        self.debug = DebugBus::new(
            self.config.debug_log_path.clone(),
            self.config.max_debug_events,
        );
        let _ = crate::config::save_workspace_root(&self.config, &root);
        self.debug.emit(
            "INFO",
            "Service",
            &format!("workspace root set: {}", root.display()),
            None,
        );
        Ok(root.to_string_lossy().to_string())
    }

    pub fn list_lanes(&self) -> Result<Vec<LaneRecord>, String> {
        self.lane_store().list_lanes()
    }

    pub fn set_lane(
        &mut self,
        lane_id: &str,
        name: &str,
        mode: &str,
        folder: &str,
        recursive: bool,
        feature_keys: &[String],
    ) -> Result<LaneRecord, String> {
        self.set_lane_for_actor(
            lane_id,
            if name.is_empty() { None } else { Some(name) },
            Some(mode),
            if folder.is_empty() {
                None
            } else {
                Some(folder)
            },
            Some(recursive),
            Some(feature_keys),
            None,
            false,
        )
    }

    pub fn set_lane_for_actor(
        &mut self,
        lane_id: &str,
        name: Option<&str>,
        mode: Option<&str>,
        folder: Option<&str>,
        recursive: Option<bool>,
        feature_keys: Option<&[String]>,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        self.lane_store().set_lane_for_actor(
            LaneUpdate {
                lane_id: lane_id.to_string(),
                name: name.map(str::to_string),
                mode: mode.map(parse_lane_mode).transpose()?,
                folder: folder.map(str::to_string),
                recursive,
                feature_keys: feature_keys.map(|keys| keys.to_vec()),
            },
            actor,
            steal,
        )
    }

    pub fn scan_lane(&mut self, lane_id: &str) -> Result<LaneScanResult, String> {
        self.lane_store().scan_lane(lane_id)
    }

    pub fn scan_lane_for_actor(
        &mut self,
        lane_id: &str,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneScanResult, String> {
        self.lane_store().scan_lane_for_actor(lane_id, actor, steal)
    }

    pub fn scan_all_lanes(&mut self) -> Result<Vec<LaneScanResult>, String> {
        self.lane_store().scan_all_lanes()
    }

    pub fn scan_all_lanes_for_actor(
        &mut self,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<Vec<LaneScanResult>, String> {
        self.lane_store().scan_all_lanes_for_actor(actor, steal)
    }

    pub fn claim_lane(
        &mut self,
        lane_id: &str,
        actor: &str,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        self.lane_store().claim_lane(lane_id, actor, steal)
    }

    pub fn release_lane(
        &mut self,
        lane_id: &str,
        actor: &str,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        self.lane_store().release_lane(lane_id, actor, steal)
    }

    pub fn lane_status(&self, lane_id: Option<&str>) -> Result<Vec<LaneRecord>, String> {
        self.lane_store().lane_status(lane_id)
    }

    fn lane_store(&self) -> LaneStore {
        LaneStore::new(self.config.workspace_root.clone())
    }

    pub fn start_lane_batch(
        &mut self,
        lane_id: &str,
        project_name: &str,
        feature_keys: &[String],
        in_place: bool,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneBatchResult, String> {
        let action_id = Uuid::new_v4().to_string();
        self.start_lane_batch_with_action_id(
            lane_id,
            project_name,
            feature_keys,
            in_place,
            actor,
            steal,
            &action_id,
        )
    }

    pub fn start_lane_batch_with_action_id(
        &mut self,
        lane_id: &str,
        project_name: &str,
        feature_keys: &[String],
        in_place: bool,
        actor: Option<&str>,
        steal: bool,
        action_id: &str,
    ) -> Result<LaneBatchResult, String> {
        let lane = self.lane_store().lane_for_actor(lane_id, actor, steal)?;
        let store = self.lane_store();
        store.record_batch_started(&lane.lane_id, action_id)?;
        match self.run_lane_batch_record(&lane, action_id, project_name, feature_keys, in_place) {
            Ok(result) => {
                let _ = store.record_batch_result(&result);
                Ok(result)
            }
            Err(err) => {
                let result = lane_batch_error(&lane, action_id, feature_keys, err.clone());
                let _ = store.record_batch_result(&result);
                Err(err)
            }
        }
    }

    pub fn start_all_lane_batches(
        &mut self,
        project_name: &str,
        feature_keys: &[String],
        concurrency_limit: usize,
        in_place: bool,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneBatchAggregate, String> {
        let limit = concurrency_limit.max(1);
        let lanes: Vec<LaneRecord> = self
            .lane_store()
            .list_lanes()?
            .into_iter()
            .filter(|lane| lane.mode == LaneMode::Batch)
            .collect();
        let total_lanes = lanes.len();
        let mut results = Vec::new();

        for chunk in lanes.chunks(limit) {
            let mut handles = Vec::new();
            for lane in chunk {
                let lane = match self
                    .lane_store()
                    .lane_for_actor(&lane.lane_id, actor, steal)
                {
                    Ok(lane) => lane,
                    Err(err) => {
                        results.push(LaneBatchResult {
                            lane_id: lane.lane_id.clone(),
                            action_id: Uuid::new_v4().to_string(),
                            status: "error".to_string(),
                            item_count: lane.item_count,
                            feature_keys: effective_batch_features(lane, feature_keys),
                            run_id: None,
                            output_path: None,
                            error: Some(err),
                        });
                        continue;
                    }
                };
                let action_id = Uuid::new_v4().to_string();
                let _ = self
                    .lane_store()
                    .record_batch_started(&lane.lane_id, &action_id);
                let mut cfg = self.config.clone();
                cfg.copy_location = self.copy_location.clone();
                let project = project_name.to_string();
                let features = feature_keys.to_vec();
                handles.push(std::thread::spawn(move || {
                    let mut service = FacialService::new(cfg);
                    service
                        .run_lane_batch_record(&lane, &action_id, &project, &features, in_place)
                        .unwrap_or_else(|err| lane_batch_error(&lane, &action_id, &features, err))
                }));
            }
            for handle in handles {
                match handle.join() {
                    Ok(result) => {
                        let _ = self.lane_store().record_batch_result(&result);
                        results.push(result);
                    }
                    Err(_) => results.push(LaneBatchResult {
                        lane_id: "unknown".to_string(),
                        action_id: Uuid::new_v4().to_string(),
                        status: "error".to_string(),
                        item_count: 0,
                        feature_keys: Vec::new(),
                        run_id: None,
                        output_path: None,
                        error: Some("lane worker panicked".to_string()),
                    }),
                }
            }
        }

        let ok = results
            .iter()
            .filter(|result| result.run_id.is_some())
            .count();
        let failed = results.len().saturating_sub(ok);
        Ok(LaneBatchAggregate {
            concurrency_limit: limit,
            total_lanes,
            ok,
            failed,
            results,
        })
    }

    fn run_lane_batch_record(
        &mut self,
        lane: &LaneRecord,
        action_id: &str,
        project_name: &str,
        feature_keys: &[String],
        in_place: bool,
    ) -> Result<LaneBatchResult, String> {
        if lane.files.is_empty() {
            return Err(format!(
                "lane {} has no scanned inventory; run scan_lane first",
                lane.lane_id
            ));
        }
        let features = effective_batch_features(lane, feature_keys);
        if features.is_empty() {
            return Err(format!("lane {} has no feature keys", lane.lane_id));
        }
        let project = if project_name.trim().is_empty() {
            if lane.name.trim().is_empty() {
                lane.lane_id.as_str()
            } else {
                lane.name.as_str()
            }
        } else {
            project_name
        };
        let summary = self.run_pipeline(project, &lane.files, &features, None, in_place)?;
        let status = summary.status.clone();
        Ok(LaneBatchResult {
            lane_id: lane.lane_id.clone(),
            action_id: action_id.to_string(),
            status,
            item_count: lane.files.len(),
            feature_keys: features,
            run_id: Some(summary.run_id),
            output_path: Some(summary.output_path),
            error: None,
        })
    }

    fn project_copy_root(&self, project_name: &str) -> Result<PathBuf, String> {
        let _ = project_name;
        let base = self
            .copy_location
            .clone()
            .ok_or_else(|| "Set a copy/output location before starting any task".to_string())?;
        Ok(base)
    }

    fn project_copy_images_root(&self, project_name: &str) -> Result<PathBuf, String> {
        Ok(self.project_copy_root(project_name)?.join("images"))
    }

    fn run_root_for(
        &mut self,
        project_name: &str,
        normalized_images: &[String],
        worktree_path: Option<String>,
        in_place: bool,
    ) -> Result<(PathBuf, PathBuf), String> {
        let run_id = format!(
            "{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        if in_place {
            let parent = common_image_parent(normalized_images)
                .ok_or_else(|| "No image parent available for in-place run".to_string())?;
            let root = parent.join(".facial");
            return Ok((root.clone(), root.join("runs").join(run_id)));
        }
        let worktree = if let Some(raw) = worktree_path {
            if raw.trim().is_empty() || raw == "no worktree yet" {
                self.project_copy_root(project_name)?
            } else {
                PathBuf::from(raw)
            }
        } else {
            self.project_copy_root(project_name)?
        };
        Ok((worktree.clone(), worktree.join("runs").join(run_id)))
    }

    /// Explicitly import + hash-pin an identity engine. The imported artifacts
    /// and manifest live under the selected workspace's app-owned model root.
    /// A blank detector selects the pinned bundled YuNet bytes.
    pub fn set_identity_paths(
        &mut self,
        model_path: &str,
        detector_path: &str,
    ) -> Result<serde_json::Value, String> {
        let model = model_path.trim();
        if model.is_empty() {
            return Err("model_path cannot be empty".to_string());
        }
        let model_pb = std::path::PathBuf::from(model);
        if !model_pb.exists() {
            return Err(format!("model file not found: {model}"));
        }
        let det_pb = if detector_path.trim().is_empty() {
            None
        } else {
            let p = std::path::PathBuf::from(detector_path.trim());
            if !p.exists() {
                return Err(format!("detector file not found: {}", p.display()));
            }
            Some(p)
        };
        let manifest_path = self
            .config
            .workspace_root
            .join(".facial")
            .join("models")
            .join("match-inference-manifest-v1.json");
        let prior_manifest = match fs::read(&manifest_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(format!("read prior identity manifest failed: {err}")),
        };
        let engine = IdentityEngine::provision(&model_pb, det_pb.as_deref(), &manifest_path)
            .map_err(|e| format!("identity engine load failed: {e}"))?;
        let align = engine.align_method().to_string();
        let sha = engine.model_sha256().to_string();
        let generation = engine.generation().to_string();
        let embedding_dim = engine.embedding_dim();
        let detector_origin = engine.detector_origin().to_string();
        if let Err(save_error) = crate::config::save_identity_paths(
            &self.config,
            &Some(model_pb.clone()),
            &det_pb,
            &Some(manifest_path.clone()),
        ) {
            crate::identity::restore_manifest(&manifest_path, prior_manifest.as_deref()).map_err(
                |restore_error| {
                    format!(
                        "identity settings save failed: {save_error}; prior manifest restore failed: {restore_error}"
                    )
                },
            )?;
            return Err(format!("identity settings save failed: {save_error}"));
        }
        self.config.identity_model_path = Some(model_pb.clone());
        self.config.identity_detector_path = det_pb.clone();
        self.config.identity_manifest_path = Some(manifest_path.clone());
        self.identity = Some(engine);
        self.identity_load_error = None;
        self.identity_refs = None;
        self.identity_negs = None;
        self.sync_detector_registry();
        self.debug.emit(
            "INFO",
            "Identity",
            &format!("identity engine set: sha256={sha} align={align}"),
            None,
        );
        Ok(serde_json::json!({
            "model_path": model_pb.to_string_lossy(),
            "detector_path": det_pb.as_ref().map(|p| p.to_string_lossy().to_string()),
            "detector_origin": detector_origin,
            "align": align,
            "model_sha256": sha,
            "model_generation": generation,
            "embedding_dim": embedding_dim,
            "manifest_path": manifest_path.to_string_lossy(),
            "manifest": self.identity.as_ref().map(|value| value.manifest()),
        }))
    }

    /// Deterministically sort a completed run's images into keep/review/cull
    /// folders from its on-disk verdicts. Copy-only (non-destructive).
    pub fn sort_run(
        &mut self,
        run_id: &str,
        in_parent: bool,
        keep_dir: &str,
        cull_dir: &str,
        review_dir: &str,
    ) -> Result<serde_json::Value, String> {
        let (keep_d, review_d, cull_d) = if in_parent {
            if keep_dir.trim().is_empty()
                || cull_dir.trim().is_empty()
                || review_dir.trim().is_empty()
            {
                return Err(
                    "work-in-parent sort requires keep, review, and cull folder paths".to_string(),
                );
            }
            (
                PathBuf::from(keep_dir.trim()),
                PathBuf::from(review_dir.trim()),
                PathBuf::from(cull_dir.trim()),
            )
        } else {
            let base = self
                .copy_location
                .clone()
                .ok_or_else(|| "Set a copy/output location before starting any task".to_string())?;
            (base.join("keep"), base.join("review"), base.join("cull"))
        };

        let results_path = self
            .find_run_results(run_id)
            .ok_or_else(|| format!("run not found: {run_id}"))?;
        let run_dir = results_path
            .parent()
            .ok_or_else(|| "run dir has no parent".to_string())?
            .to_path_buf();

        let (universe, cull_set, review_set) = Self::classify_run(&run_dir);
        if universe.is_empty() {
            return Err(
                "no per-image verdicts found in this run (run quality/dedupe features first)"
                    .to_string(),
            );
        }
        for dir in [&keep_d, &review_d, &cull_d] {
            fs::create_dir_all(dir)
                .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        }

        let (mut keep, mut review, mut cull) = (0usize, 0usize, 0usize);
        let mut errors: Vec<String> = Vec::new();
        for path in &universe {
            let dest_dir = if cull_set.contains(path) {
                &cull_d
            } else if review_set.contains(path) {
                &review_d
            } else {
                &keep_d
            };
            let src = Path::new(path);
            let file_name = src
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("image");
            let mut dest = dest_dir.join(file_name);
            if dest.exists() {
                let stem = src.file_stem().and_then(|v| v.to_str()).unwrap_or("image");
                let ext = src
                    .extension()
                    .and_then(|v| v.to_str())
                    .map(|e| format!(".{e}"))
                    .unwrap_or_default();
                dest = dest_dir.join(format!("{stem}_{}{ext}", &Uuid::new_v4().to_string()[..8]));
            }
            match fs::copy(src, &dest) {
                Ok(_) => {
                    if cull_set.contains(path) {
                        cull += 1;
                    } else if review_set.contains(path) {
                        review += 1;
                    } else {
                        keep += 1;
                    }
                }
                Err(err) => errors.push(format!("{path}: {err}")),
            }
        }

        let mode = if in_parent { "in_parent" } else { "copy" };
        self.debug.emit(
            "INFO",
            "Sort",
            &format!("sort_run {run_id}: keep={keep} review={review} cull={cull} mode={mode}"),
            None,
        );
        Ok(serde_json::json!({
            "run_id": run_id,
            "mode": mode,
            "total": universe.len(),
            "keep": keep,
            "review": review,
            "cull": cull,
            "keep_dir": keep_d.to_string_lossy(),
            "review_dir": review_d.to_string_lossy(),
            "cull_dir": cull_d.to_string_lossy(),
            "errors": errors,
        }))
    }

    /// Deterministic classifier: walk every per-image verdict JSON under the run
    /// dir and split images into (universe, cull, review).
    fn classify_run(
        run_dir: &Path,
    ) -> (
        Vec<String>,
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    ) {
        let mut universe: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut cull: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut reject: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut weak: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut files: Vec<PathBuf> = Vec::new();
        Self::collect_json_files(run_dir, &mut files);
        for file in files {
            if let Ok(raw) = fs::read_to_string(&file) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    Self::walk_classify(&value, &mut universe, &mut cull, &mut reject, &mut weak);
                }
            }
        }

        let mut cull_set = cull;
        for path in reject {
            cull_set.insert(path);
        }
        let review_set: std::collections::HashSet<String> = weak
            .into_iter()
            .filter(|path| !cull_set.contains(path))
            .collect();
        (universe.into_iter().collect(), cull_set, review_set)
    }

    fn walk_classify(
        value: &serde_json::Value,
        universe: &mut std::collections::BTreeSet<String>,
        cull: &mut std::collections::HashSet<String>,
        reject: &mut std::collections::HashSet<String>,
        weak: &mut std::collections::HashSet<String>,
    ) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(path)) = map.get("path") {
                    if Self::is_image_path(path) {
                        universe.insert(path.clone());
                        if let Some(serde_json::Value::String(band)) = map.get("quality_band") {
                            match band.as_str() {
                                "reject" => {
                                    reject.insert(path.clone());
                                }
                                "weak" => {
                                    weak.insert(path.clone());
                                }
                                _ => {}
                            }
                        }
                        let flagged = matches!(map.get("action"), Some(serde_json::Value::String(a)) if a == "remove")
                            || matches!(map.get("keep"), Some(serde_json::Value::Bool(false)))
                            || matches!(map.get("blink"), Some(serde_json::Value::Bool(true)))
                            || matches!(
                                map.get("blink_frame"),
                                Some(serde_json::Value::Bool(true))
                            )
                            || matches!(map.get("is_blink"), Some(serde_json::Value::Bool(true)));
                        if flagged {
                            cull.insert(path.clone());
                        }
                    }
                }
                for child in map.values() {
                    Self::walk_classify(child, universe, cull, reject, weak);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    Self::walk_classify(child, universe, cull, reject, weak);
                }
            }
            _ => {}
        }
    }

    fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    Self::collect_json_files(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("json")
                    && path.file_name().and_then(|n| n.to_str()) != Some("results.json")
                {
                    out.push(path);
                }
            }
        }
    }

    fn is_image_path(path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        [
            ".jpg", ".jpeg", ".png", ".webp", ".bmp", ".tif", ".tiff", ".gif",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
    }

    /// Identity engine status (available/disabled + provenance) for the harness.
    pub fn identity_status(&self) -> serde_json::Value {
        match &self.identity {
            Some(engine) => serde_json::json!({
                "available": true,
                "state": "ready",
                "model_sha256": engine.model_sha256(),
                "model_generation": engine.generation(),
                "embedding_dim": engine.embedding_dim(),
                "runtime": engine.manifest().runtime,
                "runtime_name": engine.manifest().runtime_name,
                "manifest": engine.manifest(),
                "align_capability": engine.align_method(),
                "detector": engine.has_detector(),
                "detector_origin": engine.detector_origin(),
                "detector_sha256": engine.detector_sha256(),
                "reference_dir": self.config.identity_reference_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                "negative_dir": self.config.identity_negative_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                "threshold": self.config.identity_threshold,
                "required_margin": self.config.identity_margin,
                "privacy": {
                    "embeddings_logged": false,
                    "face_crops_logged": false,
                },
                "landmarks": match &self.landmarks {
                    Some(lm) => serde_json::json!({
                        "available": true,
                        "points": crate::landmarks::NUM_LMS,
                        "model_path": lm.model_path().to_string_lossy(),
                        "model_sha256": lm.model_sha256(),
                        "ear_method": crate::landmarks::EAR_METHOD,
                        "ear_open_min": crate::landmarks::EAR_OPEN_MIN,
                        "occlusion": "withheld (validation gate failed; see WP-021)",
                    }),
                    None => serde_json::json!({
                        "available": false,
                        "configured": self.config.landmark_model_path.as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        "load_attempted": self.landmarks_load_attempted,
                    }),
                },
            }),
            None => serde_json::json!({
                "available": false,
                "state": if self.identity_load_error.is_some() { "rejected" } else { "disabled" },
                "reason": if self.identity_load_error.is_some() {
                    "configured identity model was rejected"
                } else {
                    "no accepted identity manifest provisioned (use Set identity engine or FACIAL_IDENTITY_MANIFEST)"
                },
                "load_error": self.identity_load_error,
                "privacy": {
                    "embeddings_logged": false,
                    "face_crops_logged": false,
                },
            }),
        }
    }

    /// Redacted multi-face Match diagnostic. Embeddings and aligned crops are
    /// used internally but never serialized into the receipt or debug stream.
    pub fn match_faces(&mut self, image: &str) -> Result<serde_json::Value, String> {
        let engine = self
            .identity
            .as_ref()
            .ok_or_else(|| "identity_unavailable: no accepted model generation".to_string())?;
        let batch = engine
            .embed_faces(Path::new(image))
            .map_err(|error| error.to_string())?;
        let faces: Vec<serde_json::Value> = batch
            .faces
            .iter()
            .enumerate()
            .map(|(index, face)| {
                json!({
                    "index": index,
                    "bbox_normalized": face.bbox_normalized,
                    "landmarks_normalized": face.landmarks_normalized,
                    "quality": face.quality,
                    "embedding_dim": face.embedding_dim,
                    "model_generation": face.generation,
                })
            })
            .collect();
        let result = json!({
            "image_w": batch.image_w,
            "image_h": batch.image_h,
            "face_count": faces.len(),
            "faces": faces,
            "rejected_faces": batch.failures,
            "model_generation": engine.generation(),
            "embedding_dim": engine.embedding_dim(),
            "privacy": { "embeddings_in_receipt": false, "face_crops_in_receipt": false },
        });
        self.debug.emit(
            "INFO",
            "Match",
            &format!(
                "match_faces completed: faces={} rejected={} generation={}",
                result["face_count"],
                result["rejected_faces"].as_array().map_or(0, Vec::len),
                engine.generation()
            ),
            None,
        );
        Ok(result)
    }

    /// Lazily embed the reference + negative sets once, then cache them.
    fn ensure_identity_refs(&mut self) {
        if self.identity_refs.is_none() {
            let engine = self.identity.as_ref().unwrap();
            let refs =
                Self::load_dir_embeddings(engine, self.config.identity_reference_dir.as_deref());
            let negs =
                Self::load_dir_embeddings(engine, self.config.identity_negative_dir.as_deref());
            self.identity_refs = Some(refs);
            self.identity_negs = Some(negs);
        }
    }

    /// Gate one image: embed + detect in a single pass, compare against the
    /// reference/negative sets, and report face geometry (box/count/scale).
    /// Returns a full row object; never panics on a bad image (yields a row with
    /// `verdict: "error"`). `count_threshold` gates which faces count toward
    /// `face_count` (collage / no-face signal); the alignment floor is lower.
    #[allow(clippy::too_many_arguments)]
    fn identity_gate_failure_row(
        image: &str,
        error: crate::identity::IdentityError,
        generation: &str,
        embedding_dim: usize,
    ) -> serde_json::Value {
        if error.code == "missing_face" {
            return json!({
                "image": image,
                "verdict": "no_face",
                "source": "real",
                "error": serde_json::Value::Null,
                "face_count": 0,
                "face_box": serde_json::Value::Null,
                "face_frac": serde_json::Value::Null,
                "face_score": serde_json::Value::Null,
                "framing": "none",
                "model_generation": generation,
                "embedding_dim": embedding_dim,
            });
        }
        json!({
            "image": image,
            "verdict": "error",
            "source": "real",
            "error": error,
            "face_count": 0,
            "face_box": serde_json::Value::Null,
            "face_frac": serde_json::Value::Null,
            "face_score": serde_json::Value::Null,
            "framing": "none",
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn gate_row(
        engine: &IdentityEngine,
        landmarks: Option<&crate::landmarks::LandmarkEngine>,
        refs: &[crate::identity::IdentityVector],
        negs: &[crate::identity::IdentityVector],
        threshold: f32,
        margin: f32,
        count_threshold: f32,
        closeup_min: f32,
        threequarter_min: f32,
        image: &str,
    ) -> serde_json::Value {
        let detect = match engine.embed_with_detection(Path::new(image)) {
            Ok(d) => d,
            Err(e) => {
                return Self::identity_gate_failure_row(
                    image,
                    e,
                    engine.generation(),
                    engine.embedding_dim(),
                );
            }
        };
        let max_sim = |set: &[crate::identity::IdentityVector]| -> Result<f32, crate::identity::IdentityError> {
            set.iter()
                .map(|v| crate::identity::cosine_checked(&detect.embedding, v))
                .try_fold(f32::NEG_INFINITY, |best, value| value.map(|value| best.max(value)))
        };
        let ref_sim = if refs.is_empty() {
            0.0
        } else {
            match max_sim(refs) {
                Ok(value) => value,
                Err(error) => {
                    return json!({"image": image, "verdict": "error", "source": "real", "error": error})
                }
            }
        };
        let neg_sim = if negs.is_empty() {
            0.0
        } else {
            match max_sim(negs) {
                Ok(value) => value,
                Err(error) => {
                    return json!({"image": image, "verdict": "error", "source": "real", "error": error})
                }
            }
        };
        let id_verdict = if refs.is_empty() {
            "no_reference"
        } else if ref_sim >= threshold && (ref_sim - neg_sim) >= margin {
            "match"
        } else if ref_sim < threshold {
            "no_match"
        } else {
            "unsure"
        };
        // Face geometry. `face_count` = faces at/above the count threshold (the
        // collage signal). `face_box`/`face_frac`/`face_score` describe the
        // strongest detected face (>= alignment floor) for scale bucketing.
        let counted = detect
            .faces
            .iter()
            .filter(|f| f.score >= count_threshold)
            .count();
        let top = detect.faces.first();
        let no_face = engine.has_detector() && top.is_none();
        let verdict = if no_face { "no_face" } else { id_verdict };
        let img_area = detect.image_w as f32 * detect.image_h as f32;
        let frac_opt: Option<f32> = top.map(|f| {
            if img_area > 0.0 {
                (f.bbox[2] * f.bbox[3]) / img_area
            } else {
                0.0
            }
        });
        let (face_box, face_score) = match top {
            Some(f) => (
                json!({ "x": f.bbox[0], "y": f.bbox[1], "w": f.bbox[2], "h": f.bbox[3] }),
                json!(f.score),
            ),
            None => (serde_json::Value::Null, serde_json::Value::Null),
        };
        // Shot-scale bucket from face area ratio (thresholds calibrated on leeseo,
        // configurable). `none` when no face was detected.
        let framing = match frac_opt {
            Some(fr) if fr >= closeup_min => "close-up",
            Some(fr) if fr >= threequarter_min => "three-quarter",
            Some(_) => "full-body",
            None => "none",
        };
        let face_frac = match frac_opt {
            Some(fr) => json!(fr),
            None => serde_json::Value::Null,
        };
        // Curation metadata, wave 2 (WP-021): PIPNet 98-pt landmarks give
        // eyes-open EAR (source: real). The cls-confidence occlusion proxy
        // FAILED its validation gate (no clean-vs-occluded separation) and is
        // withheld per the spike contract; landmark_conf_min stays as a real
        // localization measurement. Null when no engine/face; engine errors
        // are isolated to null fields, never abort the row.
        let lm_fields = match (landmarks, top) {
            (Some(lm_engine), Some(face)) => match lm_engine.analyze(&detect.image, face.bbox) {
                Ok(lm) => Some((
                    json!(lm.eyes_open),
                    json!(lm.ear_left),
                    json!(lm.ear_right),
                    json!(lm.confidence_min),
                )),
                Err(_) => None,
            },
            _ => None,
        };
        let (eyes_open, ear_left, ear_right, lm_conf_min) = lm_fields.unwrap_or((
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ));
        // Curation metadata, wave 1 (WP-019). Sharpness + yaw derive from the
        // real detector geometry; the hair flag is an HSV strip heuristic and
        // is labeled proxy + carries its confidence.
        let (sharpness, yaw, yaw_ratio, hair, hair_conf) = match top {
            Some(face) => {
                let sharp = crate::identity::laplacian_variance(&detect.image, Some(face.bbox));
                let (yaw, ratio) = crate::identity::yaw_bucket(&face.landmarks);
                let (hair, conf) = crate::identity::hair_color_flag(&detect.image, face.bbox);
                (
                    json!(sharp),
                    json!(yaw),
                    json!(ratio),
                    json!(hair),
                    json!(conf),
                )
            }
            None => (
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
        };
        json!({
            "image": image,
            "verdict": verdict,
            "source": "real",
            "reference_similarity": ref_sim,
            "negative_similarity": neg_sim,
            "margin": ref_sim - neg_sim,
            "threshold": threshold,
            "required_margin": margin,
            "reference_count": refs.len(),
            "negative_count": negs.len(),
            "face_count": counted,
            "face_box": face_box,
            "face_frac": face_frac,
            "face_score": face_score,
            "framing": framing,
            "face_crop_sharpness": sharpness,
            "yaw_estimate": yaw,
            "yaw_ratio": yaw_ratio,
            "hair_color": hair,
            "hair_confidence": hair_conf,
            "hair_source": "proxy",
            "eyes_open": eyes_open,
            "ear_left": ear_left,
            "ear_right": ear_right,
            "ear_method": crate::landmarks::EAR_METHOD,
            "ear_open_min": crate::landmarks::EAR_OPEN_MIN,
            "landmark_conf_min": lm_conf_min,
            "image_w": detect.image_w,
            "image_h": detect.image_h,
            "align": "yunet_112",
            "model_sha256": engine.model_sha256(),
            "model_generation": detect.generation,
            "embedding_dim": detect.embedding_dim,
            "count_threshold": count_threshold,
            "error": serde_json::Value::Null,
        })
    }

    /// Deterministic identity gate: embed the image and compare against the
    /// configured reference and negative sets. Errors when no model is provisioned.
    pub fn identity_gate(&mut self, image: &str) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        self.ensure_identity_refs();
        self.ensure_landmarks();
        let engine = self.identity.as_ref().unwrap();
        let landmarks = self.landmarks.as_ref();
        let refs = self.identity_refs.as_ref().unwrap();
        let negs = self.identity_negs.as_ref().unwrap();
        let result = Self::gate_row(
            engine,
            landmarks,
            refs,
            negs,
            self.config.identity_threshold,
            self.config.identity_margin,
            self.config.identity_count_threshold,
            self.config.framing_closeup_min,
            self.config.framing_threequarter_min,
            image,
        );
        let verdict = result["verdict"].as_str().unwrap_or("error").to_string();
        self.debug.emit(
            "INFO",
            "Identity",
            &format!("identity_gate {image}: {verdict}"),
            None,
        );
        Ok(result)
    }

    // ------------------------------------------------------------------
    // Review queue (WP-016): thin service wrappers over crate::review so
    // every verb lands in the debug event stream like other actions.
    // ------------------------------------------------------------------

    pub fn review_init(
        &mut self,
        dir: &str,
        shards: usize,
        gate_manifest: Option<&str>,
        clusters: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let result =
            crate::review::init_session(&self.config, dir, shards, gate_manifest, clusters)?;
        self.debug.emit(
            "INFO",
            "Review",
            &format!(
                "review_init session={} images={} shards={} joined_metadata={} joined_clusters={}",
                result["session_id"],
                result["image_count"],
                result["shards"],
                result["joined_metadata"],
                result["joined_clusters"]
            ),
            None,
        );
        Ok(result)
    }

    pub fn review_montage(
        &mut self,
        session: &str,
        shard: Option<usize>,
        page: usize,
        face_crop: bool,
        filters: &[String],
    ) -> Result<serde_json::Value, String> {
        let result =
            crate::review::montage(&self.config, session, shard, page, face_crop, filters)?;
        self.debug.emit(
            "INFO",
            "Review",
            &format!(
                "review_montage session={session} page={page} tiles={} png={}",
                result["tiles"], result["png"]
            ),
            None,
        );
        Ok(result)
    }

    pub fn review_export(
        &mut self,
        session: &str,
        out: &str,
        repeats: usize,
        name: &str,
        allow_partial: bool,
    ) -> Result<serde_json::Value, String> {
        let result =
            crate::review::export_kohya(&self.config, session, out, repeats, name, allow_partial)?;
        self.debug.emit(
            "INFO",
            "Review",
            &format!(
                "review_export session={session} exported={} problems={} dataset={}",
                result["funnel"]["exported"],
                result["funnel"]["export_problems"],
                result["dataset_dir"]
            ),
            None,
        );
        Ok(result)
    }

    pub fn review_claim(
        &mut self,
        session: &str,
        shard: Option<usize>,
        actor: &str,
        steal: bool,
    ) -> Result<serde_json::Value, String> {
        let result = crate::review::claim_shard(&self.config, session, shard, actor, steal)?;
        self.debug.emit(
            "INFO",
            "Review",
            &format!(
                "review_claim session={session} shard={} actor={actor} steal={steal}",
                result["claim"]["shard"]
            ),
            None,
        );
        Ok(result)
    }

    pub fn review_decide(
        &mut self,
        session: &str,
        id: &str,
        decision: &str,
        reason: &str,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let result = crate::review::decide(&self.config, session, id, decision, reason, actor)?;
        self.debug.emit(
            "INFO",
            "Review",
            &format!("review_decide session={session} id={id} decision={decision} actor={actor}"),
            None,
        );
        Ok(result)
    }

    pub fn review_status(&mut self, session: &str) -> Result<serde_json::Value, String> {
        crate::review::status(&self.config, session)
    }

    /// Batch identity gate over a directory (top-level images). Reuses the
    /// cached reference/negative embeddings, isolates per-image errors, and
    /// writes `runs/<run_id>/identity_gate.csv` + `manifest.json` under the
    /// configured copy-root (else the gated dir's `.facial/`). Returns a small
    /// summary that points at the artifacts for the receipt.
    pub fn identity_gate_dir(&mut self, dir: &str) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            return Err(format!("not a directory: {dir}"));
        }
        self.ensure_identity_refs();

        // Top-level image files, sorted for deterministic output.
        let mut images: Vec<PathBuf> = fs::read_dir(dir_path)
            .map_err(|e| format!("read dir {dir}: {e}"))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_image_ext(p))
            .collect();
        images.sort();

        let run_id = format!(
            "{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        let run_root = match self.copy_location.clone() {
            Some(base) => base.join("runs").join(&run_id),
            None => dir_path.join(".facial").join("runs").join(&run_id),
        };
        fs::create_dir_all(&run_root).map_err(|e| format!("create run dir: {e}"))?;
        let csv_path = run_root.join("identity_gate.csv");
        let manifest_path = run_root.join("manifest.json");

        self.ensure_landmarks();
        let engine = self.identity.as_ref().unwrap();
        let landmarks = self.landmarks.as_ref();
        let refs = self.identity_refs.as_ref().unwrap();
        let negs = self.identity_negs.as_ref().unwrap();
        let threshold = self.config.identity_threshold;
        let margin = self.config.identity_margin;
        let count_threshold = self.config.identity_count_threshold;
        let closeup_min = self.config.framing_closeup_min;
        let threequarter_min = self.config.framing_threequarter_min;

        let mut csv = String::from(
            "path,verdict,source,reference_similarity,negative_similarity,margin,face_count,\
face_box_x,face_box_y,face_box_w,face_box_h,face_frac,face_score,framing,\
face_crop_sharpness,yaw_estimate,yaw_ratio,hair_color,hair_confidence,\
eyes_open,ear_left,ear_right,landmark_conf_min,\
image_w,image_h,align,error\n",
        );
        let mut rows: Vec<serde_json::Value> = Vec::with_capacity(images.len());
        let mut summary: BTreeMap<String, u64> = BTreeMap::new();
        for img in &images {
            let img_s = img.to_string_lossy().to_string();
            let row = Self::gate_row(
                engine,
                landmarks,
                refs,
                negs,
                threshold,
                margin,
                count_threshold,
                closeup_min,
                threequarter_min,
                &img_s,
            );
            let verdict = row["verdict"].as_str().unwrap_or("error").to_string();
            *summary.entry(verdict).or_insert(0) += 1;
            csv.push_str(&gate_csv_line(&row));
            rows.push(row);
        }
        fs::write(&csv_path, &csv).map_err(|e| format!("write csv: {e}"))?;

        let summary_json = json!(summary);
        let manifest = json!({
            "schema_version": 2,
            "run_id": run_id,
            "dir": dir,
            "total": images.len(),
            "summary": summary_json,
            "threshold": threshold,
            "required_margin": margin,
            "count_threshold": count_threshold,
            "nms_threshold": 0.3,
            "framing_closeup_min": closeup_min,
            "framing_threequarter_min": threequarter_min,
            "model_sha256": engine.model_sha256(),
            "align_capability": engine.align_method(),
            "rows": rows,
        });
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )
        .map_err(|e| format!("write manifest: {e}"))?;

        self.debug.emit(
            "INFO",
            "Identity",
            &format!(
                "identity_gate_dir {dir}: {} images -> {}",
                images.len(),
                run_root.display()
            ),
            None,
        );

        Ok(json!({
            "run_id": run_id,
            "run_dir": run_root.to_string_lossy(),
            "csv_path": csv_path.to_string_lossy(),
            "manifest_path": manifest_path.to_string_lossy(),
            "total": images.len(),
            "summary": summary_json,
            "threshold": threshold,
            "required_margin": margin,
            "count_threshold": count_threshold,
            "nms_threshold": 0.3,
            "model_sha256": engine.model_sha256(),
        }))
    }

    /// Embedding-based near-duplicate grouping (WP-018): embed every top-level
    /// image in `dir` with the real ArcFace engine, cluster by greedy cosine
    /// threshold, and write `identity_dedup.json` (groups with members +
    /// recommended keeper). Presentation-only: nothing is deleted; the review
    /// queue joins these cluster ids via `review_init --clusters`.
    pub fn identity_dedup(
        &mut self,
        dir: &str,
        threshold: f32,
    ) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            return Err(format!("not a directory: {dir}"));
        }
        let threshold = threshold.clamp(0.5, 0.9999);

        let mut images: Vec<PathBuf> = fs::read_dir(dir_path)
            .map_err(|e| format!("read dir {dir}: {e}"))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_image_ext(p))
            .collect();
        images.sort();
        if images.is_empty() {
            return Err(format!("no images in {dir}"));
        }
        if images.len() > 20_000 {
            return Err(format!(
                "{} images exceeds the 20k dedup cap (O(n*k) pairwise cosine); split the folder",
                images.len()
            ));
        }

        let engine = self.identity.as_ref().unwrap();
        struct Item {
            path: String,
            embedding: crate::identity::IdentityVector,
            face_score: f32,
            sharpness: f32,
        }
        let mut items: Vec<Item> = Vec::with_capacity(images.len());
        let mut errors: Vec<serde_json::Value> = Vec::new();
        for img in &images {
            let path_s = img.to_string_lossy().to_string();
            match engine.embed_with_detection(img) {
                Ok(detect) => {
                    let (face_score, sharpness) = match detect.faces.first() {
                        Some(face) => (
                            face.score,
                            crate::identity::laplacian_variance(&detect.image, Some(face.bbox)),
                        ),
                        None => (
                            0.0,
                            crate::identity::laplacian_variance(&detect.image, None),
                        ),
                    };
                    items.push(Item {
                        path: path_s,
                        embedding: detect.embedding,
                        face_score,
                        sharpness,
                    });
                }
                Err(err) => errors.push(json!({ "path": path_s, "error": err })),
            }
        }
        if items.is_empty() {
            return Err("no image could be embedded".to_string());
        }

        let embeddings: Vec<crate::identity::IdentityVector> =
            items.iter().map(|i| i.embedding.clone()).collect();
        let assignment = crate::identity::cluster_embeddings(&embeddings, threshold)
            .map_err(|error| error.to_string())?;

        // Build groups for clusters with >= 2 members; singletons stay ungrouped.
        let mut by_cluster: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (item_idx, cluster_idx) in assignment.iter().enumerate() {
            by_cluster.entry(*cluster_idx).or_default().push(item_idx);
        }
        let mut groups: Vec<serde_json::Value> = Vec::new();
        let mut grouped_images = 0usize;
        for (cluster_idx, member_idxs) in &by_cluster {
            if member_idxs.len() < 2 {
                continue;
            }
            grouped_images += member_idxs.len();
            let rep_idx = member_idxs[0];
            // Recommended keeper: best face score, sharpness as tiebreaker.
            let keeper_idx = *member_idxs
                .iter()
                .max_by(|a, b| {
                    let ia = &items[**a];
                    let ib = &items[**b];
                    ia.face_score
                        .partial_cmp(&ib.face_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(
                            ia.sharpness
                                .partial_cmp(&ib.sharpness)
                                .unwrap_or(std::cmp::Ordering::Equal),
                        )
                })
                .unwrap();
            let members: Vec<serde_json::Value> = member_idxs
                .iter()
                .map(|&idx| {
                    json!({
                        "path": items[idx].path,
                        "similarity_to_rep": crate::identity::cosine_checked(
                            &items[idx].embedding,
                            &items[rep_idx].embedding
                        ).unwrap_or(f32::NAN),
                        "face_score": items[idx].face_score,
                        "face_crop_sharpness": items[idx].sharpness,
                    })
                })
                .collect();
            let sims: Vec<f32> = member_idxs
                .iter()
                .map(|&idx| {
                    crate::identity::cosine_checked(
                        &items[idx].embedding,
                        &items[rep_idx].embedding,
                    )
                    .unwrap_or(f32::NAN)
                })
                .collect();
            groups.push(json!({
                "cluster_id": format!("c{cluster_idx:04}"),
                "member_count": member_idxs.len(),
                "min_similarity_to_rep": sims.iter().cloned().fold(f32::INFINITY, f32::min),
                "max_similarity_to_rep": sims.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
                "recommended_keep": items[keeper_idx].path,
                "members": members,
            }));
        }

        let run_id = format!(
            "{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        let run_root = match self.copy_location.clone() {
            Some(base) => base.join("runs").join(&run_id),
            None => dir_path.join(".facial").join("runs").join(&run_id),
        };
        fs::create_dir_all(&run_root).map_err(|e| format!("create run dir: {e}"))?;
        let artifact_path = run_root.join("identity_dedup.json");
        let artifact = json!({
            "schema_version": 1,
            "run_id": run_id,
            "dir": dir,
            "threshold": threshold,
            "engine": "arcface_cosine",
            "model_sha256": engine.model_sha256(),
            "total_images": images.len(),
            "embedded": items.len(),
            "groups": groups,
            "grouped_images": grouped_images,
            "singletons": items.len() - grouped_images,
            "errors": errors,
        });
        fs::write(
            &artifact_path,
            serde_json::to_string_pretty(&artifact).unwrap_or_default(),
        )
        .map_err(|e| format!("write identity_dedup.json: {e}"))?;

        self.debug.emit(
            "INFO",
            "Identity",
            &format!(
                "identity_dedup {dir}: {} images -> {} groups ({} grouped)",
                images.len(),
                groups.len(),
                grouped_images
            ),
            None,
        );

        Ok(json!({
            "run_id": run_id,
            "artifact": artifact_path.to_string_lossy(),
            "threshold": threshold,
            "total_images": images.len(),
            "embedded": items.len(),
            "groups": groups.len(),
            "grouped_images": grouped_images,
            "singletons": items.len() - grouped_images,
            "errors": errors.len(),
        }))
    }

    /// Batch render-eval (WP-017 / STUB-J): score every image under `dir`
    /// (recursive) against the configured anchor set, grouped by config key
    /// (immediate subfolder name, else filename stem with a trailing index
    /// stripped). `no_face`/`error` rows are counted but NEVER enter the
    /// similarity statistics.
    pub fn render_eval(&mut self, dir: &str) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            return Err(format!("not a directory: {dir}"));
        }
        self.ensure_identity_refs();
        if self.identity_refs.as_ref().is_none_or(|r| r.is_empty()) {
            return Err(
                "render_eval needs a reference set (identity_reference_dir / FACIAL_IDENTITY_REF_DIR)"
                    .to_string(),
            );
        }

        // Recursive walk, sorted for determinism.
        let mut images: Vec<PathBuf> = Vec::new();
        let mut queue = vec![dir_path.to_path_buf()];
        while let Some(current) = queue.pop() {
            if let Ok(entries) = fs::read_dir(&current) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        queue.push(p);
                    } else if is_image_ext(&p) {
                        images.push(p);
                    }
                }
            }
        }
        images.sort();
        if images.is_empty() {
            return Err(format!("no images under {dir}"));
        }

        self.ensure_landmarks();
        let engine = self.identity.as_ref().unwrap();
        let landmarks = self.landmarks.as_ref();
        let refs = self.identity_refs.as_ref().unwrap();
        let negs = self.identity_negs.as_ref().unwrap();
        let threshold = self.config.identity_threshold;
        let margin = self.config.identity_margin;
        let count_threshold = self.config.identity_count_threshold;
        let closeup_min = self.config.framing_closeup_min;
        let threequarter_min = self.config.framing_threequarter_min;

        let config_key = |p: &Path| -> String {
            let parent = p.parent().unwrap_or(dir_path);
            if parent != dir_path {
                return parent
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("_ungrouped")
                    .to_string();
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let trimmed = stem
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .trim_end_matches(['-', '_']);
            if trimmed.is_empty() {
                "_ungrouped".to_string()
            } else {
                trimmed.to_string()
            }
        };

        #[derive(Default)]
        struct GroupStats {
            sims: Vec<f32>,
            verdicts: BTreeMap<String, u64>,
        }
        let mut by_group: BTreeMap<String, GroupStats> = BTreeMap::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for img in &images {
            let img_s = img.to_string_lossy().to_string();
            let mut row = Self::gate_row(
                engine,
                landmarks,
                refs,
                negs,
                threshold,
                margin,
                count_threshold,
                closeup_min,
                threequarter_min,
                &img_s,
            );
            let key = config_key(img);
            row["config_key"] = json!(key);
            let verdict = row["verdict"].as_str().unwrap_or("error").to_string();
            let stats = by_group.entry(key).or_default();
            *stats.verdicts.entry(verdict.clone()).or_insert(0) += 1;
            // Similarity statistics ONLY for rows where a face was scored:
            // no_face / error must never read as passes or pull the stats.
            if matches!(verdict.as_str(), "match" | "no_match" | "unsure") {
                if let Some(sim) = row["reference_similarity"].as_f64() {
                    stats.sims.push(sim as f32);
                }
            }
            rows.push(row);
        }

        let mut group_rows: Vec<serde_json::Value> = Vec::new();
        for (key, stats) in &by_group {
            let scored = stats.sims.len();
            let (mean, min, max) = if scored > 0 {
                let sum: f32 = stats.sims.iter().sum();
                (
                    json!(sum / scored as f32),
                    json!(stats.sims.iter().cloned().fold(f32::INFINITY, f32::min)),
                    json!(stats.sims.iter().cloned().fold(f32::NEG_INFINITY, f32::max)),
                )
            } else {
                (
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                )
            };
            group_rows.push(json!({
                "config_key": key,
                "images": stats.verdicts.values().sum::<u64>(),
                "scored": scored,
                "excluded_no_face": stats.verdicts.get("no_face").copied().unwrap_or(0),
                "excluded_error": stats.verdicts.get("error").copied().unwrap_or(0),
                "verdicts": stats.verdicts,
                "mean_similarity": mean,
                "min_similarity": min,
                "max_similarity": max,
            }));
        }

        let run_id = format!(
            "{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        let run_root = match self.copy_location.clone() {
            Some(base) => base.join("runs").join(&run_id),
            None => dir_path.join(".facial").join("runs").join(&run_id),
        };
        fs::create_dir_all(&run_root).map_err(|e| format!("create run dir: {e}"))?;
        let artifact_path = run_root.join("render_eval.json");
        let artifact = json!({
            "schema_version": 1,
            "run_id": run_id,
            "dir": dir,
            "threshold": threshold,
            "model_sha256": engine.model_sha256(),
            "groups": group_rows,
            "rows": rows,
        });
        fs::write(
            &artifact_path,
            serde_json::to_string_pretty(&artifact).unwrap_or_default(),
        )
        .map_err(|e| format!("write render_eval.json: {e}"))?;

        self.debug.emit(
            "INFO",
            "Identity",
            &format!(
                "render_eval {dir}: {} images in {} groups",
                images.len(),
                group_rows.len()
            ),
            None,
        );

        Ok(json!({
            "run_id": run_id,
            "artifact": artifact_path.to_string_lossy(),
            "total_images": images.len(),
            "groups": group_rows,
        }))
    }

    /// Threshold calibration (WP-017 / STUB-I): anchor pairwise self-consistency
    /// + negative-set distribution -> a RECOMMENDED gate threshold with its
    /// reasoning. Report-only; nothing is applied.
    pub fn calibrate_threshold(&mut self) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        self.ensure_identity_refs();
        let refs = self.identity_refs.as_ref().unwrap();
        let negs = self.identity_negs.as_ref().unwrap();
        if refs.len() < 2 {
            return Err(format!(
                "calibration needs >= 2 reference images (have {})",
                refs.len()
            ));
        }

        let mut pairwise: Vec<f32> = Vec::new();
        for i in 0..refs.len() {
            for j in (i + 1)..refs.len() {
                pairwise.push(
                    crate::identity::cosine_checked(&refs[i], &refs[j])
                        .map_err(|error| error.to_string())?,
                );
            }
        }
        pairwise.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p10 = pairwise[(pairwise.len() as f32 * 0.1) as usize];
        let anchor_min = pairwise[0];
        let anchor_max = pairwise[pairwise.len() - 1];
        let anchor_mean: f32 = pairwise.iter().sum::<f32>() / pairwise.len() as f32;

        // Each negative scored the way the gate scores: max sim to any anchor.
        let neg_sims: Vec<f32> = negs
            .iter()
            .map(|n| -> Result<f32, String> {
                refs.iter()
                    .map(|r| crate::identity::cosine_checked(n, r))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| values.into_iter().fold(f32::NEG_INFINITY, f32::max))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let neg_max = neg_sims.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let neg_mean = if neg_sims.is_empty() {
            None
        } else {
            Some(neg_sims.iter().sum::<f32>() / neg_sims.len() as f32)
        };

        // Recommendation: midpoint between the anchors' own spread floor (p10)
        // and the hardest negative; without negatives, back off from the
        // anchors' weakest self-similarity. Refused below 4 anchors (the
        // distribution is too thin to trust).
        let (recommended, method) = if refs.len() < 4 {
            (None, "refused: fewer than 4 reference images")
        } else if neg_sims.is_empty() {
            (
                Some((anchor_min - 0.05).clamp(0.3, 0.9)),
                "anchor_min - 0.05 (no negative set)",
            )
        } else {
            (
                Some(((p10 + neg_max) / 2.0).clamp(0.3, 0.9)),
                "midpoint(anchor_p10, negative_max)",
            )
        };

        let result = json!({
            "schema_version": 1,
            "reference_count": refs.len(),
            "negative_count": negs.len(),
            "anchor_pairwise": {
                "pairs": pairwise.len(),
                "min": anchor_min,
                "p10": p10,
                "mean": anchor_mean,
                "max": anchor_max,
            },
            "negative_vs_anchors": {
                "count": neg_sims.len(),
                "max": if neg_sims.is_empty() { serde_json::Value::Null } else { json!(neg_max) },
                "mean": neg_mean,
            },
            "current_threshold": self.config.identity_threshold,
            "recommended_threshold": recommended,
            "method": method,
            "applied": false,
        });
        self.debug.emit(
            "INFO",
            "Identity",
            &format!(
                "calibrate_threshold: anchors={} pairs={} recommended={:?}",
                refs.len(),
                pairwise.len(),
                recommended
            ),
            None,
        );
        Ok(result)
    }

    /// Anchor-paired montage (WP-017): one grid PNG with the candidate as
    /// tile 0 and every anchor after it, plus a tile map carrying per-anchor
    /// cosine similarity to the candidate. Visual artifact for identity calls.
    pub fn anchor_montage(&mut self, image: &str) -> Result<serde_json::Value, String> {
        if self.identity.is_none() {
            return Err("identity unavailable: no model provisioned".to_string());
        }
        let ref_dir = self
            .config
            .identity_reference_dir
            .clone()
            .ok_or_else(|| "no identity_reference_dir configured".to_string())?;
        let candidate = Path::new(image);
        if !candidate.is_file() {
            return Err(format!("not a file: {image}"));
        }
        let mut anchors: Vec<PathBuf> = fs::read_dir(&ref_dir)
            .map_err(|e| format!("read reference dir: {e}"))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_image_ext(p))
            .collect();
        anchors.sort();
        if anchors.is_empty() {
            return Err(format!("no anchor images in {}", ref_dir.display()));
        }

        let engine = self.identity.as_ref().unwrap();
        let candidate_emb = engine
            .embed_file(candidate)
            .map_err(|e| format!("embed candidate: {e}"))?;

        const TILE: u32 = 256;
        const GAP: u32 = 6;
        const MARGIN: u32 = 8;
        let count = anchors.len() + 1;
        let cols = count.min(5) as u32;
        let rows_n = count.div_ceil(5) as u32;
        let canvas_w = MARGIN * 2 + cols * (TILE + GAP) - GAP;
        let canvas_h = MARGIN * 2 + rows_n * (TILE + GAP) - GAP;
        let mut canvas =
            image::RgbaImage::from_pixel(canvas_w, canvas_h, image::Rgba([235, 232, 222, 255]));

        let mut tiles_json: Vec<serde_json::Value> = Vec::new();
        let mut place = |idx: usize,
                         path: &Path,
                         role: &str,
                         similarity: Option<f32>,
                         canvas: &mut image::RgbaImage|
         -> Result<(), String> {
            let col = (idx % 5) as u32;
            let grid_row = (idx / 5) as u32;
            let cell_x = MARGIN + col * (TILE + GAP);
            let cell_y = MARGIN + grid_row * (TILE + GAP);
            let mut error = None;
            match image::open(path) {
                Ok(img) => {
                    let thumb = img.thumbnail(TILE, TILE).to_rgba8();
                    let off_x = cell_x + (TILE - thumb.width()) / 2;
                    let off_y = cell_y + (TILE - thumb.height()) / 2;
                    image::imageops::overlay(canvas, &thumb, off_x as i64, off_y as i64);
                }
                Err(err) => {
                    for y in cell_y..cell_y + TILE {
                        for x in cell_x..cell_x + TILE {
                            canvas.put_pixel(x, y, image::Rgba([140, 58, 58, 255]));
                        }
                    }
                    error = Some(format!("{err}"));
                }
            }
            tiles_json.push(json!({
                "tile": idx,
                "row": grid_row, "col": col,
                "x": cell_x, "y": cell_y, "w": TILE, "h": TILE,
                "path": path.to_string_lossy(),
                "role": role,
                "similarity_to_candidate": similarity,
                "error": error,
            }));
            Ok(())
        };

        place(0, candidate, "candidate", None, &mut canvas)?;
        for (i, anchor) in anchors.iter().enumerate() {
            let sim = engine.embed_file(anchor).ok().map(|emb| {
                crate::identity::cosine_checked(&candidate_emb, &emb).unwrap_or(f32::NAN)
            });
            place(i + 1, anchor, "anchor", sim, &mut canvas)?;
        }

        let run_id = format!(
            "{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        );
        let run_root = match self.copy_location.clone() {
            Some(base) => base.join("runs").join(&run_id),
            None => candidate
                .parent()
                .unwrap_or(Path::new("."))
                .join(".facial")
                .join("runs")
                .join(&run_id),
        };
        fs::create_dir_all(&run_root).map_err(|e| format!("create run dir: {e}"))?;
        let png_path = run_root.join("anchor_montage.png");
        let map_path = run_root.join("anchor_montage.map.json");
        canvas
            .save(&png_path)
            .map_err(|e| format!("save montage: {e}"))?;
        let map = json!({
            "schema_version": 1,
            "candidate": image,
            "anchor_dir": ref_dir.to_string_lossy(),
            "grid": { "cols": 5, "tile": TILE },
            "tiles": tiles_json,
        });
        fs::write(
            &map_path,
            serde_json::to_string_pretty(&map).unwrap_or_default(),
        )
        .map_err(|e| format!("write montage map: {e}"))?;

        self.debug.emit(
            "INFO",
            "Identity",
            &format!(
                "anchor_montage {image}: {} anchors -> {}",
                anchors.len(),
                png_path.display()
            ),
            None,
        );

        Ok(json!({
            "run_id": run_id,
            "png": png_path.to_string_lossy(),
            "map": map_path.to_string_lossy(),
            "anchors": anchors.len(),
        }))
    }

    /// Real ArcFace embeddings for deepface represent/verify/find (used only
    /// when an identity model is provisioned). Writes the feature artifact and
    /// returns the PluginRunResult.
    fn real_deepface_feature(
        &self,
        feature_id: &str,
        images: &[String],
        run_feature_root: &Path,
    ) -> PluginRunResult {
        let engine = self
            .identity
            .as_ref()
            .expect("real_deepface_feature called without an engine");
        let mut embs: Vec<(String, crate::identity::IdentityVector)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for img in images {
            match engine.embed_file(Path::new(img)) {
                Ok(e) => embs.push((img.clone(), e)),
                Err(err) => errors.push(format!("{img}: {err}")),
            }
        }
        let model_sha = engine.model_sha256().to_string();
        let threshold = self.config.identity_threshold;

        let payload = match feature_id {
            "represent" => {
                let rows: Vec<serde_json::Value> = embs
                    .iter()
                    .map(|(path, e)| {
                        let max_c = e
                            .values()
                            .iter()
                            .cloned()
                            .fold(f32::NEG_INFINITY, f32::max);
                        let min_c = e
                            .values()
                            .iter()
                            .cloned()
                            .fold(f32::INFINITY, f32::min);
                        let head: Vec<f32> = e.values().iter().cloned().take(12).collect();
                        serde_json::json!({
                            "path": path,
                            "source": "real",
                            "embedding_dim": e.dimension(),
                            "model_generation": e.generation(),
                            "embedding_norm": 1.0,
                            "embedding": e.values(),
                            "embedding_unit": {"head": head, "max_component": max_c, "min_component": min_c},
                        })
                    })
                    .collect();
                serde_json::json!({
                    "feature": "represent", "engine": "arcface_onnx",
                    "model_sha256": model_sha, "align": engine.align_method(),
                    "count": rows.len(), "errors": errors, "items": rows,
                })
            }
            "verify" => {
                let mut pairs = Vec::new();
                for i in 0..embs.len() {
                    for j in (i + 1)..embs.len() {
                        let sim = crate::identity::cosine_checked(&embs[i].1, &embs[j].1)
                            .unwrap_or(f32::NAN);
                        pairs.push(serde_json::json!({
                            "a": embs[i].0, "b": embs[j].0,
                            "similarity": sim, "verified": sim >= threshold,
                        }));
                    }
                }
                serde_json::json!({
                    "feature": "verify", "engine": "arcface_onnx",
                    "model_sha256": model_sha, "threshold": threshold,
                    "count": pairs.len(), "errors": errors, "pairs": pairs,
                })
            }
            "find" => {
                let top_k = 5usize;
                let mut queries = Vec::new();
                for (qi, (qpath, qe)) in embs.iter().enumerate() {
                    let mut cands: Vec<(String, f32)> = embs
                        .iter()
                        .enumerate()
                        .filter(|(ci, _)| *ci != qi)
                        .map(|(_, (p, e))| {
                            (
                                p.clone(),
                                crate::identity::cosine_checked(qe, e).unwrap_or(f32::NAN),
                            )
                        })
                        .collect();
                    cands
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    cands.truncate(top_k);
                    let best = cands.first().map(|c| c.1).unwrap_or(0.0);
                    let crows: Vec<serde_json::Value> = cands
                        .iter()
                        .map(|(p, s)| serde_json::json!({"path": p, "similarity": s}))
                        .collect();
                    queries.push(serde_json::json!({
                        "query": qpath, "best_similarity": best, "candidates": crows,
                    }));
                }
                serde_json::json!({
                    "feature": "find", "engine": "arcface_onnx",
                    "model_sha256": model_sha, "top_k": top_k,
                    "count": queries.len(), "errors": errors, "queries": queries,
                })
            }
            other => serde_json::json!({"feature": other, "error": "unsupported"}),
        };

        let artifact_path = run_feature_root.join(format!("{feature_id}.json"));
        let _ = fs::write(
            &artifact_path,
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
        PluginRunResult {
            plugin_id: "deepface".to_string(),
            feature_id: feature_id.to_string(),
            status: "ok".to_string(),
            message: format!(
                "{feature_id} completed (real arcface_onnx, {} images)",
                embs.len()
            ),
            payload,
            artifacts: vec![artifact_path.to_string_lossy().to_string()],
        }
    }

    fn load_dir_embeddings(
        engine: &IdentityEngine,
        dir: Option<&Path>,
    ) -> Vec<crate::identity::IdentityVector> {
        let mut out = Vec::new();
        let Some(dir) = dir else {
            return out;
        };
        if let Ok(entries) = fs::read_dir(dir) {
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            paths.sort();
            for path in paths {
                if let Ok(emb) = engine.embed_file(&path) {
                    out.push(emb);
                }
            }
        }
        out
    }

    /// Passthrough to DebugBus::record_applied_action (so ui.rs records without
    /// touching the private DebugBus). Returns the emitted event.
    pub fn record_applied_action(
        &mut self,
        command_id: &str,
        intent: &str,
        applied: bool,
        message: &str,
        snapshot: serde_json::Value,
    ) -> DebugEvent {
        self.debug
            .record_applied_action(command_id, intent, applied, message, snapshot)
    }

    /// Disk-scan helper for GetRunStatus/GetRunSummary/ListArtifacts: scan
    /// list_worktrees() for a run dir whose file_name == run_id, return its
    /// results.json path if present.
    pub fn find_run_results(&mut self, run_id: &str) -> Option<PathBuf> {
        for path in &self.run_results_index {
            if path
                .parent()
                .and_then(|value| value.file_name())
                .and_then(|value| value.to_str())
                .map(|name| name == run_id)
                .unwrap_or(false)
                && path.is_file()
            {
                return Some(path.clone());
            }
        }
        if let Some(copy_root) = self.copy_location.clone() {
            for entry in WalkDir::new(copy_root).into_iter().filter_map(Result::ok) {
                if entry.file_name() == "results.json"
                    && entry
                        .path()
                        .parent()
                        .and_then(|value| value.file_name())
                        .and_then(|value| value.to_str())
                        .map(|name| name == run_id)
                        .unwrap_or(false)
                {
                    return Some(entry.path().to_path_buf());
                }
            }
        }
        for runs in self.list_worktrees().values() {
            for run_dir in runs {
                // Direct child of the project dir.
                if run_dir
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|name| name == run_id)
                    .unwrap_or(false)
                {
                    let candidate = run_dir.join("results.json");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
                // Nested run dirs created by run_pipeline at <worktree>/runs/<run_id>.
                let nested = run_dir.join("runs").join(run_id).join("results.json");
                if nested.is_file() {
                    return Some(nested);
                }
            }
        }
        None
    }

    fn ingest_single(&mut self, source: &Path, target_dir: &Path, in_place: bool) -> IngestResult {
        if !source.exists() {
            self.debug.emit(
                "ERROR",
                "Ingest",
                &format!("missing source: {source:?}"),
                None,
            );
            return IngestResult {
                source: source.to_string_lossy().to_string(),
                destination: "".to_string(),
                mode: if in_place {
                    "in_place".to_string()
                } else {
                    "copy".to_string()
                },
                ok: false,
                message: "source missing".to_string(),
            };
        }

        let _ = fs::create_dir_all(target_dir);
        let file_name = source
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let mut destination = target_dir.join(file_name);
        if destination.exists() {
            let stem = destination
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("image");
            let ext = destination
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("jpg");
            destination = target_dir.join(format!(
                "{stem}_{}.{}",
                Uuid::new_v4().to_string().replace('-', "_"),
                ext
            ));
        }

        let mode = if in_place {
            #[cfg(windows)]
            {
                if std::os::windows::fs::symlink_file(source, &destination).is_ok() {
                    "symlink".to_string()
                } else if std::fs::hard_link(source, &destination).is_ok() {
                    "hardlink".to_string()
                } else {
                    if fs::copy(source, &destination).is_ok() {
                        "copy_fallback".to_string()
                    } else {
                        "error".to_string()
                    }
                }
            }
            #[cfg(not(windows))]
            {
                if std::os::unix::fs::symlink(source, &destination).is_ok() {
                    "symlink".to_string()
                } else if std::fs::hard_link(source, &destination).is_ok() {
                    "hardlink".to_string()
                } else {
                    if fs::copy(source, &destination).is_ok() {
                        "copy_fallback".to_string()
                    } else {
                        "error".to_string()
                    }
                }
            }
        } else if fs::copy(source, &destination).is_ok() {
            "copy".to_string()
        } else {
            "error".to_string()
        };

        let ok = mode != "error";
        let message = if mode == "copy_fallback" {
            "in-place fallback copy".to_string()
        } else if mode == "error" {
            "copy failed".to_string()
        } else {
            format!("ingested as {mode}")
        };
        self.debug.emit(
            "INFO",
            "Ingest",
            &format!(
                "{message} source={} destination={}",
                source.to_string_lossy(),
                destination.to_string_lossy()
            ),
            None,
        );
        IngestResult {
            source: source.to_string_lossy().to_string(),
            destination: destination.to_string_lossy().to_string(),
            mode: if in_place && mode == "error" {
                "copy_fallback".to_string()
            } else {
                mode
            },
            ok,
            message,
        }
    }
}

impl Drop for FacialService {
    fn drop(&mut self) {
        self.retired_match_workers
            .extend(cancel_match_store_runtime(&self.match_store));
        for worker in self.retired_match_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "loads provisioned identity model for XMP staging-to-manual-region restart proof"]
    fn wp085_xmp_staging_maps_fresh_media_to_manual_face_and_survives_restart() {
        use crate::api::{MatchCorrectionRequest, MatchMaintenanceRequest};
        use crate::match_store::{Assignment, FaceObservation, MatchStore};
        use sha2::{Digest, Sha256};

        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("w600k_r50.onnx");
        assert!(
            model.is_file(),
            "explicit XMP integration proof requires provisioned model"
        );
        let root = test_root("wp085-xmp-manual-restart");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let source = media_root.join("MixedCase.PNG");
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 255, 255, 255]))
            .save(&source)
            .unwrap();
        let original_hash = Sha256::digest(std::fs::read(&source).unwrap());
        let manifest = root.join(".facial/models/match-inference-manifest-v1.json");
        drop(IdentityEngine::provision(&model, None, &manifest).unwrap());
        let mut config = test_config(&root, None);
        config.identity_manifest_path = Some(manifest);
        let mut service = FacialService::new(config.clone());
        let configured = service
            .match_configure_root(&media_root.to_string_lossy(), Vec::new())
            .unwrap();
        let store = service.ready_match_store().unwrap();
        let job = service
            .match_start_job(configured["root_id"].as_str().unwrap())
            .unwrap();
        let job_id = job["job_id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            let current = store.job(job_id).unwrap();
            if current.lifecycle == "completed" {
                assert_eq!(current.skipped, 1);
                break;
            }
            assert!(
                !matches!(
                    current.lifecycle.as_str(),
                    "failed" | "partial" | "cancelled"
                ),
                "worker failed: {current:?}"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "worker timed out: {current:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let assets = store.job_assets(job_id).unwrap();
        assert_eq!(assets.len(), 1);
        let media_key = assets[0].media_key.clone();
        let selected_person = store
            .create_person("Explicitly selected Person", Vec::new())
            .unwrap();
        let sidecar = root.join("imported-region.xmp");
        let xml = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/" xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Bag><rdf:li rdf:parseType="Resource">
<mwg-rs:Name>Untrusted sidecar name</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type>
<mwg-rs:Area stArea:x="0.25" stArea:y="0.4" stArea:w="0.3" stArea:h="0.4" stArea:unit="normalized"/>
</rdf:li></rdf:Bag></mwg-rs:RegionList></mwg-rs:Regions></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        std::fs::write(&sidecar, xml).unwrap();
        let preview_request: MatchMaintenanceRequest = serde_json::from_value(json!({
            "action": "xmp_import_dry_run", "path": sidecar,
        }))
        .unwrap();
        let preview = service
            .match_maintenance("xmp-preview", &preview_request)
            .unwrap();
        let import_request: MatchMaintenanceRequest = serde_json::from_value(json!({
            "action": "xmp_import", "path": sidecar,
            "confirmation_token": preview["preview_token"], "confirmed": true,
        }))
        .unwrap();
        let staged = service
            .match_maintenance("xmp-stage", &import_request)
            .unwrap();
        assert_eq!(staged["staged_region_count"], 1);
        assert_eq!(staged["applied_match_rows"], 0);
        assert_eq!(staged["identity_truth_applied"], false);
        assert_eq!(
            store
                .list::<crate::match_store::Person>("match_person")
                .unwrap(),
            vec![selected_person.clone()]
        );
        assert!(store
            .list::<FaceObservation>("match_face_observation")
            .unwrap()
            .is_empty());
        assert!(store
            .list::<Assignment>("match_assignment")
            .unwrap()
            .is_empty());
        let fresh = service.match_media_faces(&media_key).unwrap();
        assert!(fresh.get("source_geometry_error").is_none(), "{fresh}");
        let geometry = &fresh["source_geometry"];
        assert_eq!(geometry["source_width"], 64);
        assert_eq!(geometry["source_height"], 64);
        let staged_bounds = &staged["staged_regions"][0]["bounds_normalized"];
        let correction: MatchCorrectionRequest = serde_json::from_value(json!({
            "action": "manual_face", "face_ids": [], "person_id": selected_person.person_id,
            "media_key": media_key, "media_fingerprint": fresh["media_fingerprint"],
            "normalized_bounds": {"left": staged_bounds[0], "top": staged_bounds[1],
                "width": staged_bounds[2], "height": staged_bounds[3],
                "source_width": geometry["source_width"], "source_height": geometry["source_height"]},
            "exif_orientation": geometry["exif_orientation"],
            "expected_revisions": {"schema_generation": fresh["schema_generation"],
                "model_generation": fresh["model_generation"], "catalog_revision": fresh["catalog_revision"],
                "person_revisions": {selected_person.person_id.clone(): selected_person.revision}, "face_revisions": {}},
            "confirmed": true,
        })).unwrap();
        service.match_apply_correction(&correction).unwrap();
        let media_db = crate::media_db::MediaDb::open(&root);
        let ui_key = media_db.key_for(source.to_str().unwrap());
        assert_ne!(ui_key, media_key);
        drop(media_db);
        let before = crate::match_benchmark::runtime_admission_snapshot();
        let metadata = service.match_media_metadata_for_source(&source).unwrap();
        let after = crate::match_benchmark::runtime_admission_snapshot();
        assert_eq!(metadata["media_key"], media_key);
        assert_eq!(metadata["source_resolution"], "resolved");
        assert_eq!(metadata["rows"].as_array().unwrap().len(), 1);
        assert_eq!(
            metadata["rows"][0]["assignment"]["person_id"],
            selected_person.person_id
        );
        assert_eq!(
            metadata["rows"][0]["assignment"]["state"],
            "operator_confirmed"
        );
        assert!(metadata.get("source_geometry").is_none());
        for counter in [
            "match_workers",
            "model_loads",
            "match_index_queries",
            "match_geometry_preparations",
        ] {
            assert_eq!(before[counter], after[counter], "{counter}");
        }
        let explicit = service.match_media_faces_for_source(&source).unwrap();
        assert_eq!(explicit["media_key"], media_key);
        assert_eq!(explicit["source_geometry"]["source_width"], 64);
        assert_eq!(explicit["media_fingerprint"], metadata["media_fingerprint"]);
        assert_eq!(explicit["catalog_revision"], metadata["catalog_revision"]);
        let faces = store
            .list::<FaceObservation>("match_face_observation")
            .unwrap();
        let assignments = store.list::<Assignment>("match_assignment").unwrap();
        assert_eq!(faces.len(), 1);
        assert!(faces[0].operator_owned);
        assert_eq!(faces[0].media_key, media_key);
        let actual_bounds = &faces[0].bounds_normalized;
        assert_eq!(actual_bounds.len(), 4);
        assert!(actual_bounds.iter().all(|value| value.is_finite()));
        assert!(actual_bounds[0] >= 0.0 && actual_bounds[1] >= 0.0);
        assert!(actual_bounds[2] > 0.0 && actual_bounds[3] > 0.0);
        assert!(actual_bounds[0] + actual_bounds[2] <= 1.0);
        assert!(actual_bounds[1] + actual_bounds[3] <= 1.0);
        // Normalized source/display geometry performs f32 additions and
        // subtractions; allow their rounding, far below one source pixel.
        for (index, actual) in actual_bounds.iter().enumerate() {
            let expected = staged_bounds[index].as_f64().unwrap() as f32;
            assert!(
                (*actual - expected).abs() <= f32::EPSILON,
                "staged region coordinate {index} changed: {expected} -> {actual}"
            );
        }
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].person_id, selected_person.person_id);
        assert_eq!(assignments[0].state, "operator_confirmed");
        assert_eq!(assignments[0].face_id, faces[0].face_id);
        drop(service);
        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        let service = FacialService::new(config);
        let store = MatchStore::open(&root).unwrap();
        assert_eq!(
            store
                .list::<FaceObservation>("match_face_observation")
                .unwrap(),
            faces
        );
        assert_eq!(
            store.list::<Assignment>("match_assignment").unwrap(),
            assignments
        );
        let restored = service.match_media_faces(&media_key).unwrap();
        assert_eq!(restored["total_faces"], 1);
        assert_eq!(
            Sha256::digest(std::fs::read(&source).unwrap()),
            original_hash
        );
        assert_eq!(std::fs::read(&sidecar).unwrap(), xml);
        drop(service);
        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn xmp_import_contract_stages_only_and_maps_every_region_to_manual_face() {
        let receipt = crate::match_store::MwgXmpImportReceipt {
            preview_token: "preview-token".to_string(),
            sidecar_path: PathBuf::from("portable/media-a.xmp"),
            content_sha256: "a".repeat(64),
            staged_regions: vec![crate::match_store::MwgXmpRegion {
                face_id: "xmp-face-hint".to_string(),
                person_id: "xmp-person-hint".to_string(),
                name: "Sidecar name hint".to_string(),
                bounds_normalized: [0.1, 0.2, 0.3, 0.4],
            }],
            applied_match_rows: 0,
            original_media_rows_mutated: 0,
        };
        let value =
            xmp_import_staging_contract("xmp-stage-1", Path::new("portable-api-root"), receipt)
                .unwrap();
        assert_eq!(value["stage_status"], "staged_only");
        assert_eq!(value["match_truth_applied"], false);
        assert_eq!(value["identity_truth_applied"], false);
        assert_eq!(value["applied_match_rows"], 0);
        assert_eq!(value["original_media_rows_mutated"], 0);
        assert_eq!(value["staged_region_count"], 1);
        assert_eq!(
            value["staging_artifact"]["receipt_relative_path"],
            "receipts/xmp-stage-1.json"
        );
        assert_eq!(
            value["next_action_contract"]["regions"][0]["manual_face_correction"]["action"],
            "manual_face"
        );
        assert_eq!(
            value["next_action_contract"]["regions"][0]["manual_face_correction"]
                ["field_values_from_staging"]["normalized_bounds"]["left"],
            serde_json::json!(0.1_f32)
        );
        assert_eq!(
            value["next_action_contract"]["regions"][0]["xmp_identity_hints_only"]["person_id"],
            "xmp-person-hint"
        );

        let invalid = crate::match_store::MwgXmpImportReceipt {
            preview_token: "preview-token".to_string(),
            sidecar_path: PathBuf::from("portable/media-a.xmp"),
            content_sha256: "a".repeat(64),
            staged_regions: Vec::new(),
            applied_match_rows: 1,
            original_media_rows_mutated: 0,
        };
        assert!(xmp_import_staging_contract(
            "xmp-stage-invalid",
            Path::new("portable-api-root"),
            invalid,
        )
        .unwrap_err()
        .contains("zero-Match-truth"));
    }

    #[test]
    fn wp087_selected_source_relative_paths_preserve_strict_boundaries() {
        let relative = Path::new("missing-wp087-relative-source/MixedCase.PNG");
        assert_eq!(
            FacialService::match_selected_source_absolute(relative).unwrap(),
            std::path::absolute(relative).unwrap()
        );
        for invalid in [
            r"D:relative.PNG",
            r"\Media\image.PNG",
            r"\\.\C:\image.PNG",
            r"D:\Media\..\image.PNG",
            r"missing\..\image.PNG",
            "missing/image.PNG ",
            "missing/image.PNG:stream",
        ] {
            assert!(
                FacialService::match_selected_source_absolute(Path::new(invalid)).is_err(),
                "{invalid}"
            );
        }
        let root = test_root("wp087-relative-source");
        let service = FacialService::new(test_config(&root, None));
        let before = crate::match_benchmark::runtime_admission_snapshot();
        let metadata = service.match_media_metadata_for_source(relative).unwrap();
        let after = crate::match_benchmark::runtime_admission_snapshot();
        assert_eq!(metadata["media_key"], serde_json::Value::Null);
        assert_eq!(metadata["source_resolution"], "unindexed");
        assert_eq!(metadata["configured"], false);
        assert!(metadata.get("error").is_none());
        for counter in [
            "match_workers",
            "model_loads",
            "match_index_queries",
            "match_geometry_preparations",
        ] {
            assert_eq!(before[counter], after[counter], "{counter}");
        }
        assert!(service
            .match_media_faces_for_source(relative)
            .unwrap_err()
            .contains("match_source_resolution_unindexed"));
        drop(service);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wp087_committed_media_metadata_does_not_prepare_missing_source_geometry() {
        use crate::match_store::FaceObservation;
        let root = test_root("wp087-metadata-only");
        let service = FacialService::new(test_config(&root, None));
        let store = service.ready_match_store().unwrap();
        let media_key = "missing/source.jpg";
        store
            .create_face(FaceObservation {
                face_id: "metadata-only-face".into(),
                media_key: media_key.into(),
                media_fingerprint: "a".repeat(64),
                source_index: 0,
                source_width: None,
                source_height: None,
                exif_orientation: None,
                bounds_normalized: vec![0.1, 0.2, 0.3, 0.4],
                landmarks_normalized: vec![vec![0.2, 0.3], vec![0.4, 0.3]],
                alignment_valid: true,
                quality: 0.9,
                pose_bucket: "frontal".into(),
                operator_owned: true,
                schema_generation: store.status().unwrap()["schema_generation"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                face_revision: 1,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();
        let before = crate::match_benchmark::runtime_admission_snapshot();
        let metadata = service.match_media_metadata(media_key).unwrap();
        let after = crate::match_benchmark::runtime_admission_snapshot();
        assert_eq!(metadata["rows"][0]["face"]["face_id"], "metadata-only-face");
        assert!(metadata.get("source_geometry").is_none());
        assert!(metadata.get("source_geometry_error").is_none());
        for field in [
            "match_geometry_preparations",
            "match_workers",
            "model_loads",
            "match_index_queries",
        ] {
            assert_eq!(before[field], after[field], "metadata started {field}");
        }
        let explicit = service.match_media_faces(media_key).unwrap();
        let final_counts = crate::match_benchmark::runtime_admission_snapshot();
        assert!(
            explicit["source_geometry_error"].is_object(),
            "explicit source validation remains active"
        );
        assert_eq!(
            final_counts["match_geometry_preparations"]
                .as_u64()
                .unwrap(),
            after["match_geometry_preparations"].as_u64().unwrap() + 1
        );
        drop(store);
        drop(service);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_face_geometry_matches_source_orientation_and_overlap() {
        use crate::match_store::{ExifOrientation, NormalizedRegion};
        let display = NormalizedRegion {
            x: 0.2,
            y: 0.25,
            width: 0.3,
            height: 0.4,
        };
        let source = display
            .display_to_source(ExifOrientation::Rotate90Clockwise)
            .unwrap();
        assert!((source.x - 0.25).abs() < 1.0e-6);
        assert!((source.y - 0.5).abs() < 1.0e-6);
        assert!((source.width - 0.4).abs() < 1.0e-6);
        assert!((source.height - 0.3).abs() < 1.0e-6);
        let projected = source_region_to_display(source, ExifOrientation::Rotate90Clockwise);
        assert!((projected.x - display.x).abs() < 1.0e-6);
        assert!((projected.y - display.y).abs() < 1.0e-6);
        assert!((projected.width - display.width).abs() < 1.0e-6);
        assert!((projected.height - display.height).abs() < 1.0e-6);
        let (display_x, display_y) =
            source_point_to_display(0.25, 0.8, ExifOrientation::Rotate90Clockwise);
        assert!((display_x - 0.2).abs() < 1.0e-6);
        assert!((display_y - 0.25).abs() < 1.0e-6);
        assert_eq!(
            normalized_region_iou(source, [source.x, source.y, source.width, source.height]),
            1.0
        );
    }

    #[test]
    fn manual_embedding_admission_requires_both_landmark_and_detector_alignment() {
        use crate::landmarks::{ManualLandmarkInvalidReason, ManualLandmarkValidity};

        assert!(manual_embedding_admitted(
            Some(ManualLandmarkValidity::Valid),
            true
        ));
        assert!(!manual_embedding_admitted(
            Some(ManualLandmarkValidity::Invalid(
                ManualLandmarkInvalidReason::ConfidenceBelowFloor
            )),
            true
        ));
        assert!(!manual_embedding_admitted(
            Some(ManualLandmarkValidity::Invalid(
                ManualLandmarkInvalidReason::EyeOrdering
            )),
            true
        ));
        assert!(!manual_embedding_admitted(None, true));
        assert!(!manual_embedding_admitted(
            Some(ManualLandmarkValidity::Valid),
            false
        ));
    }

    #[test]
    fn match_store_is_dormant_until_first_explicit_match_use() {
        let root = test_root("match_store_lazy_init");
        let service = FacialService::new(test_config(&root, None));
        let state = service.match_store.lock().unwrap();
        assert_eq!(state.workspace_root, root);
        assert!(state.store.is_none());
        assert!(state.error_code.is_none());
        assert!(!state.initializing);
        assert!(state.worker.is_none());
        assert!(state.index_workers.is_empty());
        assert!(!state.cancelled.load(Ordering::Acquire));
        drop(state);
        drop(service);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wp087_ready_diagnostics_do_not_initialize_and_observe_existing_live_leases() {
        let root = test_root("wp087-ready-diagnostics");
        let service = FacialService::new(test_config(&root, None));
        assert_eq!(
            service.match_ready_public_snapshot().unwrap_err(),
            "match_store_not_ready"
        );
        {
            let state = service.match_store.lock().unwrap();
            assert!(state.store.is_none());
            assert!(!state.initializing);
            assert!(state.worker.is_none());
            assert!(state.index_workers.is_empty());
        }
        let store = service.ready_match_store().unwrap();
        let lease = store
            .governor()
            .try_acquire(crate::match_store::ResourceRequest {
                cpu_inference: 1,
                ..crate::match_store::ResourceRequest::default()
            })
            .unwrap();
        let snapshot = service.match_ready_public_snapshot().unwrap();
        assert_eq!(
            snapshot["execution"]["resource_telemetry"]["current_usage"]["cpu_inference"],
            1
        );
        assert_eq!(
            snapshot["execution"]["resource_telemetry"]["lifetime_id"],
            store.governor().telemetry().unwrap().lifetime_id
        );
        drop(lease);
        assert_eq!(
            service.match_ready_public_snapshot().unwrap()["execution"]["resource_telemetry"]
                ["releases"],
            1
        );
        let db_path = crate::media_db::MediaDb::db_path(&root);
        drop(store);
        drop(service);
        crate::surreal_store::wait_until_closed(&db_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dormant_immersive_hold_does_not_start_match_initialization() {
        let root = test_root("match_store_dormant_fullscreen");
        let service = FacialService::new(test_config(&root, None));
        service.set_match_immersive_fullscreen(true).unwrap();
        {
            let state = service.match_store.lock().unwrap();
            assert!(state.external_holds.fullscreen());
            assert!(state.store.is_none());
            assert!(!state.initializing);
            assert!(state.worker.is_none());
        }
        service.set_match_immersive_fullscreen(false).unwrap();
        {
            let state = service.match_store.lock().unwrap();
            assert!(!state.external_holds.fullscreen());
            assert!(state.store.is_none());
            assert!(!state.initializing);
            assert!(state.worker.is_none());
        }
        drop(service);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wp086_atomic_holds_survive_workspace_switch_and_stale_reconciliation() {
        let root = test_root("wp086-hold-workspace");
        let next = root.join("next-workspace");
        let mut service = FacialService::new(test_config(&root, None));
        let holds = service.match_external_holds();
        // A presentation publication is independent of the runtime mutex.
        {
            let state = service.match_store.lock().unwrap();
            holds.set_playback(true);
            holds.set_fullscreen(true);
            assert!(!state.initializing);
            assert!(state.store.is_none());
        }
        service.set_workspace_root(&next.to_string_lossy()).unwrap();
        assert!(Arc::ptr_eq(&holds, &service.match_external_holds()));
        assert!(Arc::ptr_eq(
            &holds,
            &service.match_store.lock().unwrap().external_holds
        ));
        holds.set_playback(false);
        let stale_epoch = holds.set_fullscreen(false);
        holds.set_playback(true);
        service
            .reconcile_match_external_holds(Arc::new(crate::media_io::MediaIoCoordinator::new()))
            .unwrap();
        assert_ne!(holds.snapshot(), stale_epoch);
        assert!(
            holds.playback(),
            "a delayed release reconciliation must read live holds"
        );
        assert!(!holds.fullscreen());
        let state = service.match_store.lock().unwrap();
        assert!(state.store.is_none());
        assert!(!state.initializing);
        assert!(state.worker.is_none());
        drop(state);
        drop(service);
        assert!(
            !crate::media_db::MediaDb::db_path(&root).exists(),
            "publishing holds must not initialize the original workspace database"
        );
        assert!(
            !crate::media_db::MediaDb::db_path(&next).exists(),
            "switching workspace and reconciling dormant holds must not initialize its database"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_index_helpers_are_deterministic_private_and_root_relative() {
        let nested = Path::new("People").join("Alice").join("Portrait.JPG");
        let key = match_media_key("root-opaque", &nested);
        assert!(key.starts_with("root-opaque/"));
        assert!(key.ends_with(".jpg"));
        assert!(!key.to_ascii_lowercase().contains("alice"));
        assert!(!key.to_ascii_lowercase().contains("portrait"));
        assert_eq!(key, match_media_key("root-opaque", &nested));

        let exclusions = vec!["exports".to_string(), "cache/thumbs".to_string()];
        assert!(match_path_is_excluded(
            Path::new("exports").join("render.png").as_path(),
            &exclusions
        ));
        assert!(match_path_is_excluded(
            Path::new("CACHE").join("thumbs").join("x.jpg").as_path(),
            &exclusions
        ));
        assert!(!match_path_is_excluded(
            Path::new("cache").join("originals").join("x.jpg").as_path(),
            &exclusions
        ));
        assert!(!match_path_is_excluded(
            Path::new("exported").join("render.png").as_path(),
            &exclusions
        ));
    }

    #[test]
    fn discovery_filesystem_admission_is_bounded_and_pausing_settles_admitted_result() {
        use crate::media_io::{
            IoPolicy, MediaIoCoordinator, PermitOutcome, RootIdentity, RootKind, RootLimits,
        };

        let root = test_root("match_discovery_filesystem_admission");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        let source = media_root.join("production-source.jpg");
        std::fs::write(&source, b"production-snapshot").unwrap();
        let store = crate::match_store::MatchStore::open(&root).unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        let canonical_media_root = media_root.canonicalize().unwrap();
        store
            .register_model_generation("filesystem-admission-model", true)
            .unwrap();
        let job = store
            .create_job(&configured.root_id, "filesystem-admission-model")
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, crate::match_store::JobLifecycle::Running)
            .unwrap();

        let one = RootLimits {
            total: 1,
            interactive_reserved: 0,
            bulk_reserved: 0,
            playback_bulk_limit: 1,
        };
        let coordinator = Arc::new(MediaIoCoordinator::with_policy(IoPolicy {
            local: one,
            remote: one,
            unknown: one,
            playback_hysteresis: std::time::Duration::from_millis(1),
            priority_burst: 1,
        }));
        let identity = RootIdentity::new(
            configured.root_id.clone(),
            job.identity_revision,
            RootKind::Unknown,
        );
        let cancelled = Arc::new(AtomicBool::new(false));

        // Simulate two walk/stat operations. The second operation cannot enter
        // while the first owns the only background filesystem permit.
        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let (second_entered_tx, second_entered_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let first_store = store.clone();
            let first_job = job.job_id.clone();
            let first_coordinator = Arc::clone(&coordinator);
            let first_identity = identity.clone();
            let first_cancelled = Arc::clone(&cancelled);
            scope.spawn(move || {
                let (io, _observation) = begin_match_discovery_io(
                    &first_store,
                    &first_job,
                    &first_coordinator,
                    &first_identity,
                    &first_cancelled,
                )
                .unwrap()
                .unwrap();
                first_entered_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                io.finish(PermitOutcome::Success);
            });
            first_entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();

            let second_store = store.clone();
            let second_job = job.job_id.clone();
            let second_coordinator = Arc::clone(&coordinator);
            let second_identity = identity.clone();
            let second_cancelled = Arc::clone(&cancelled);
            let second_source = source.clone();
            let second_root = canonical_media_root.clone();
            scope.spawn(move || {
                let snapshot = admitted_match_file_snapshot(
                    &second_store,
                    &second_job,
                    &second_source,
                    &second_root,
                    &second_coordinator,
                    &second_identity,
                    &second_cancelled,
                )
                .unwrap();
                second_entered_tx.send(snapshot.bytes.len()).unwrap();
            });
            assert!(second_entered_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err());
            release_first_tx.send(()).unwrap();
            assert_eq!(
                second_entered_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                b"production-snapshot".len()
            );
        });

        // A filesystem result admitted while Running may acquire its writer
        // only after the operation and still settle while Pausing.
        let (io, observation) =
            begin_match_discovery_io(&store, &job.job_id, &coordinator, &identity, &cancelled)
                .unwrap()
                .unwrap();
        store.control_job(&job.job_id, "pause").unwrap();
        io.finish(PermitOutcome::Success);
        let permit = store
            .acquire_discovery_write_for_observation(
                &job.job_id,
                "admitted/stat.jpg",
                match_stage_request(crate::match_store::JobStage::Discover, 64 * 1024, 0),
                &observation,
            )
            .unwrap();
        store
            .record_discovery_failure(
                &job.job_id,
                "admitted/stat.jpg",
                None,
                "io",
                "admitted stat result settled during Pausing",
                &permit,
            )
            .unwrap();
        assert_eq!(
            store.job(&job.job_id).unwrap().lifecycle().unwrap(),
            crate::match_store::JobLifecycle::Pausing
        );
        assert!(
            begin_match_discovery_io(&store, &job.job_id, &coordinator, &identity, &cancelled,)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.job(&job.job_id).unwrap().lifecycle().unwrap(),
            crate::match_store::JobLifecycle::Paused
        );
        let paused_snapshot_error = admitted_match_file_snapshot(
            &store,
            &job.job_id,
            &canonical_media_root.join("must-not-be-opened.jpg"),
            &canonical_media_root,
            &coordinator,
            &identity,
            &cancelled,
        )
        .err()
        .unwrap();
        assert!(paused_snapshot_error.contains("paused, held, or terminal"));
        assert!(!paused_snapshot_error.contains("canonicalize Match source"));
        let snapshot_request = match_snapshot_resource_request(MATCH_MAX_SOURCE_FILE_BYTES);
        assert_eq!(snapshot_request.surreal_writes, 0);
        assert_eq!(
            store.governor().usage().unwrap(),
            crate::match_store::ResourceUsage::default()
        );

        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_walk_error_is_durable_and_retry_requires_positive_reobservation() {
        let root = test_root("match_walk_directory_failure");
        let media_root = root.join("media-root");
        let unreadable = media_root.join("nested.directory");
        std::fs::create_dir_all(&unreadable).unwrap();
        let store = crate::match_store::MatchStore::open(&root).unwrap();
        let configured = store.configure_index_root(&media_root, Vec::new()).unwrap();
        store
            .register_model_generation("walk-error-model", true)
            .unwrap();
        let job = store
            .create_job(&configured.root_id, "walk-error-model")
            .unwrap();
        store
            .set_job_lifecycle(&job.job_id, crate::match_store::JobLifecycle::Running)
            .unwrap();

        let expected_media_key = match_media_key(
            &configured.root_id,
            unreadable.strip_prefix(&media_root).unwrap(),
        );
        let failure_permit = store
            .acquire_discovery_write(
                &job.job_id,
                &expected_media_key,
                match_stage_request(crate::match_store::JobStage::Discover, 64 * 1024, 0),
            )
            .unwrap();
        let media_key = record_match_walk_failure(
            &store,
            &failure_permit,
            &job.job_id,
            &configured.root_id,
            &media_root,
            Some(&unreadable),
            "injected permission denied",
        )
        .unwrap();
        let durable = store.job(&job.job_id).unwrap();
        assert_eq!(durable.failed, 1);
        assert_eq!(durable.discovered, 1);
        let asset = store.job_assets(&job.job_id).unwrap().pop().unwrap();
        assert_eq!(asset.media_key, media_key);
        assert_eq!(asset.failure_code.as_deref(), Some("io"));
        assert!(asset
            .failure_message
            .as_deref()
            .is_some_and(|message| message.contains("nested.directory")));

        store
            .set_job_lifecycle(&job.job_id, crate::match_store::JobLifecycle::Failed)
            .unwrap();
        let retrying = store.control_job(&job.job_id, "retry").unwrap();
        assert_eq!(retrying.failed, 1);
        assert_ne!(
            retrying.lifecycle().unwrap(),
            crate::match_store::JobLifecycle::Completed
        );
        let resolution_permit = store
            .acquire_discovery_write(
                &job.job_id,
                &media_key,
                match_stage_request(crate::match_store::JobStage::Discover, 64 * 1024, 0),
            )
            .unwrap();
        assert!(store
            .resolve_discovery_failure(&job.job_id, &media_key, &resolution_permit)
            .unwrap());
        let resolved = store.job(&job.job_id).unwrap();
        assert_eq!(resolved.failed, 0);
        assert_eq!(resolved.discovered, 0);
        assert!(store.job_assets(&job.job_id).unwrap().is_empty());

        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wp085_legacy_face_snapshot_preserves_fingerprint_and_id_without_job_asset() {
        use crate::match_store::{derived_face_id, FaceObservation, MatchStore};
        const MATCH_SCHEMA_GENERATION: &str = "match-schema-v2";
        let root = test_root("wp085-legacy-snapshot");
        let source = root.join("source.bin");
        fs::write(&source, b"historical-observation").unwrap();
        let snapshot = match_file_snapshot(&source, &root.canonicalize().unwrap()).unwrap();
        let legacy = format!("sha256:{}", snapshot.fingerprint);
        let media_key = "source.bin";
        let face_id = derived_face_id(media_key, &legacy, 0, MATCH_SCHEMA_GENERATION);
        let face = FaceObservation {
            face_id: face_id.clone(),
            media_key: media_key.to_string(),
            media_fingerprint: legacy.clone(),
            source_index: 0,
            source_width: Some(100),
            source_height: Some(100),
            exif_orientation: Some(1),
            bounds_normalized: vec![0.1, 0.1, 0.2, 0.2],
            landmarks_normalized: Vec::new(),
            alignment_valid: false,
            quality: 0.9,
            pose_bucket: "invalid".to_string(),
            operator_owned: false,
            schema_generation: MATCH_SCHEMA_GENERATION.to_string(),
            face_revision: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let store = MatchStore::open(&root).unwrap();
        let database =
            crate::surreal_store::open(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        let db = database.db();
        let persisted = face.clone();
        crate::surreal_store::run(async move {
            db.query("CREATE type::record('match_face_observation', $face_id) CONTENT $face")
                .bind(("face_id", persisted.face_id.clone()))
                .bind(("face", persisted))
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();
        let retained = store
            .observed_media_fingerprint(media_key, &snapshot.fingerprint)
            .unwrap()
            .unwrap();
        assert_eq!(retained, legacy);
        assert!(match_source_fingerprint_matches(
            &snapshot.fingerprint,
            &retained
        ));
        assert_eq!(
            derived_face_id(media_key, &retained, 0, MATCH_SCHEMA_GENERATION),
            face_id
        );
        assert_eq!(store.media_faces(media_key).unwrap().rows[0].face, face);
        for invalid in [
            format!("SHA256:{}", snapshot.fingerprint),
            format!("sha256:{}", snapshot.fingerprint.to_uppercase()),
            format!("sha256:{} ", snapshot.fingerprint),
            "sha256:bad".to_string(),
        ] {
            assert!(!match_source_fingerprint_matches(
                &snapshot.fingerprint,
                &invalid
            ));
        }
        fs::write(&source, b"changed-observation").unwrap();
        let changed = match_file_snapshot(&source, &root.canonicalize().unwrap()).unwrap();
        assert!(!match_source_fingerprint_matches(
            &changed.fingerprint,
            &retained
        ));
        assert!(store
            .observed_media_fingerprint(media_key, &changed.fingerprint)
            .unwrap()
            .is_none());
        drop(database);
        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_file_snapshot_binds_fingerprint_to_owned_inference_bytes() {
        let root = test_root("match_file_fingerprint");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        std::fs::write(&source, b"wp083-fingerprint").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let snapshot = match_file_snapshot(&source, &canonical_root).unwrap();
        assert_eq!(
            snapshot.fingerprint,
            format!("{:x}", Sha256::digest(b"wp083-fingerprint"))
        );
        assert_eq!(snapshot.fingerprint.len(), 64);
        assert!(snapshot
            .fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_eq!(snapshot.final_path, source.canonicalize().unwrap());

        // Replacing the pathname with same-length bytes after capture cannot
        // change what the inference API consumes or what the fingerprint covers.
        std::fs::write(&source, b"WP083-FINGERPRINT").unwrap();
        assert_eq!(snapshot.bytes, b"wp083-fingerprint");
        assert_eq!(
            snapshot.fingerprint,
            format!("{:x}", Sha256::digest(&snapshot.bytes))
        );
        assert_ne!(std::fs::read(&source).unwrap(), snapshot.bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn tiny_bmp_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 54];
        bytes[0..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&(54_u32).to_le_bytes());
        bytes[10..14].copy_from_slice(&(54_u32).to_le_bytes());
        bytes[14..18].copy_from_slice(&(40_u32).to_le_bytes());
        bytes[18..22].copy_from_slice(&width.to_le_bytes());
        bytes[22..26].copy_from_slice(&height.to_le_bytes());
        bytes[26..28].copy_from_slice(&(1_u16).to_le_bytes());
        bytes[28..30].copy_from_slice(&(24_u16).to_le_bytes());
        bytes
    }

    #[test]
    fn tiny_encoded_large_dimension_header_is_rejected_before_decode() {
        let encoded = tiny_bmp_header(65_535, 65_535);
        let error = match_decoded_working_bytes(&encoded, Path::new("header-bomb.bmp"))
            .expect_err("large decoded dimensions must fail before pixel allocation");
        assert!(error.contains("per-image limit"), "{error}");
    }

    #[test]
    fn concurrent_detect_leases_charge_checked_decoded_working_bytes() {
        use crate::match_store::{JobStage, MatchResourceGovernor, ResourceBudget};

        let encoded = tiny_bmp_header(3_000, 3_000);
        let decoded = match_decoded_working_bytes(&encoded, Path::new("bounded.bmp")).unwrap();
        assert_eq!(
            decoded,
            3_000_u64 * 3_000 * MATCH_DECODED_WORKING_BYTES_PER_PIXEL
        );
        let request = match_stage_request(JobStage::Detect, encoded.len() as u64, decoded);

        let default_governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
        let first = default_governor.try_acquire(request).unwrap();
        let second = default_governor.try_acquire(request).unwrap();
        assert_eq!(default_governor.usage().unwrap().decoded_bytes, decoded * 2);
        drop((first, second));
        assert_eq!(default_governor.usage().unwrap().decoded_bytes, 0);

        let constrained = MatchResourceGovernor::new(ResourceBudget {
            worker_memory_bytes: 0,
            admitted_items: 2,
            queued_items: 2,
            queued_bytes: 128 * 1024,
            cpu_inference: 2,
            decoded_bytes: decoded + decoded / 2,
            gpu_vram_bytes: 1,
            surreal_writes: 2,
            vector_index_builds: 1,
        })
        .unwrap();
        let first = constrained.try_acquire(request).unwrap();
        assert_eq!(
            constrained.try_acquire(request).err().as_deref(),
            Some("resource_pressure")
        );
        drop(first);
        assert!(constrained.try_acquire(request).is_ok());
    }

    #[test]
    fn match_resource_pressure_retries_only_the_current_unit_with_bounded_backoff() {
        let cancelled = AtomicBool::new(false);
        let admission_checks = std::sync::atomic::AtomicUsize::new(0);
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let completed = retry_match_resource_pressure(
            &cancelled,
            || {
                admission_checks.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err("resource_pressure".to_string())
                } else {
                    Ok("same-durable-unit")
                }
            },
        )
        .unwrap();
        assert_eq!(completed, "same-durable-unit");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(admission_checks.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cancelling_match_runtime_returns_owned_index_workers_for_join() {
        let root = test_root("match_index_worker_join");
        let runtime = match_store_runtime(root.clone());
        let cancelled = runtime.lock().unwrap().cancelled.clone();
        let worker = std::thread::Builder::new()
            .name("match-index-worker-join-fixture".to_string())
            .spawn(move || {
                while !cancelled.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            })
            .unwrap();
        {
            let mut state = runtime.lock().unwrap();
            state.active_index_jobs.insert("fixture-job".to_string());
            state
                .index_workers
                .insert("fixture-job".to_string(), worker);
        }
        let workers = cancel_match_store_runtime(&runtime);
        assert_eq!(workers.len(), 1);
        for worker in workers {
            worker.join().unwrap();
        }
        let state = runtime.lock().unwrap();
        assert!(state.cancelled.load(Ordering::Acquire));
        assert!(state.active_index_jobs.is_empty());
        assert!(state.index_workers.is_empty());
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_quiescence_fails_closed_until_workers_settle() {
        let root = test_root("match-maintenance-quiescence");
        let runtime = match_store_runtime(root.clone());
        let worker_cancelled = Arc::new(AtomicBool::new(false));
        let store = crate::match_store::MatchStore::open(&root).unwrap();
        {
            let mut state = runtime.lock().unwrap();
            state.store = Some(store.clone());
        }

        let running_error = require_match_index_workers_quiescent(&runtime).unwrap_err();
        assert!(running_error.contains("persisted operator_paused"));
        store
            .set_desired_mode(crate::match_store::DesiredMode::OperatorPaused)
            .unwrap();

        {
            let mut state = runtime.lock().unwrap();
            let worker_cancelled = Arc::clone(&worker_cancelled);
            let worker = std::thread::Builder::new()
                .name("match-maintenance-quiescence-fixture".to_string())
                .spawn(move || {
                    while !worker_cancelled.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                })
                .unwrap();
            state.active_index_jobs.insert("fixture-job".to_string());
            state.pending_index_jobs.insert("fixture-job".to_string());
            state
                .index_workers
                .insert("fixture-job".to_string(), worker);
        }

        let error = require_match_index_workers_quiescent(&runtime).unwrap_err();
        assert!(error.contains("requires Pause all"));
        {
            let state = runtime.lock().unwrap();
            assert!(!state.cancelled.load(Ordering::Acquire));
            assert!(state.active_index_jobs.contains("fixture-job"));
            assert!(state.pending_index_jobs.contains("fixture-job"));
            assert!(state.index_workers.contains_key("fixture-job"));
        }

        worker_cancelled.store(true, Ordering::Release);
        let worker = {
            let mut state = runtime.lock().unwrap();
            state.active_index_jobs.clear();
            state.pending_index_jobs.clear();
            state.index_workers.remove("fixture-job").unwrap()
        };
        worker.join().unwrap();
        require_match_index_workers_quiescent(&runtime).unwrap();
        drop(runtime);
        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_worker_settlement_cannot_lose_a_pending_wake() {
        let root = test_root("match-worker-atomic-settlement");
        let mut runtime = MatchStoreRuntime::dormant(root.clone());
        let job_id = "fixture-job";
        runtime.active_index_jobs.insert(job_id.to_string());
        runtime.pending_index_jobs.insert(job_id.to_string());

        assert!(settle_match_worker_iteration(&mut runtime, job_id));
        assert!(runtime.active_index_jobs.contains(job_id));
        assert!(!runtime.pending_index_jobs.contains(job_id));

        assert!(!settle_match_worker_iteration(&mut runtime, job_id));
        assert!(!runtime.active_index_jobs.contains(job_id));
        // A producer entering after the same mutex is released now observes
        // no owner and can atomically claim a fresh worker.
        assert!(runtime.active_index_jobs.insert(job_id.to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn match_model_load_failure_settlement_consumes_a_concurrent_retry_wake() {
        let root = test_root("match-model-load-failure-settlement");
        let mut runtime = MatchStoreRuntime::dormant(root.clone());
        let job_id = "model-load-failure-job";
        runtime.active_index_jobs.insert(job_id.to_string());

        // This is the exact state produced when Retry observes the still-active
        // worker after its durable model-load failure has been published.
        runtime.pending_index_jobs.insert(job_id.to_string());
        assert!(settle_match_worker_iteration(&mut runtime, job_id));
        assert!(runtime.active_index_jobs.contains(job_id));
        assert!(!runtime.pending_index_jobs.contains(job_id));

        assert!(!settle_match_worker_iteration(&mut runtime, job_id));
        assert!(!runtime.active_index_jobs.contains(job_id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "loads the bundled 174 MB identity model for a real background-worker proof"]
    fn match_index_worker_isolates_bad_sources_and_preserves_partial_results() {
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("w600k_r50.onnx");
        if !model.is_file() {
            return;
        }
        let root = test_root("match_real_worker_failure");
        let media_root = root.join("media-root");
        std::fs::create_dir_all(&media_root).unwrap();
        std::fs::write(media_root.join("invalid.jpg"), b"not-an-image").unwrap();
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 255, 255, 255]))
            .save(media_root.join("valid-no-face.png"))
            .unwrap();
        let oversized = std::fs::File::create(media_root.join("oversized.jpg")).unwrap();
        oversized.set_len(MATCH_MAX_SOURCE_FILE_BYTES + 1).unwrap();
        drop(oversized);
        let manifest_path = root
            .join(".facial")
            .join("models")
            .join("match-inference-manifest-v1.json");
        drop(IdentityEngine::provision(&model, None, &manifest_path).unwrap());
        let mut config = test_config(&root, None);
        config.identity_manifest_path = Some(manifest_path);
        let service = FacialService::new(config);
        let configured = service
            .match_configure_root(&media_root.to_string_lossy(), Vec::new())
            .unwrap();
        let job = service
            .match_start_job(configured["root_id"].as_str().unwrap())
            .unwrap();
        let job_id = job["job_id"].as_str().unwrap().to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let terminal = loop {
            let settings = service.match_settings_snapshot().unwrap();
            let current = settings["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["job_id"].as_str() == Some(job_id.as_str()))
                .cloned()
                .unwrap();
            if matches!(
                current["lifecycle"].as_str(),
                Some("failed" | "partial" | "completed")
            ) {
                break (settings, current);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Match worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert_eq!(terminal.1["lifecycle"], "partial");
        assert_eq!(terminal.1["failed"], 2);
        assert_eq!(terminal.1["skipped"], 1);
        let failed_assets = terminal.0["failed_assets"].as_array().unwrap();
        assert!(failed_assets.iter().any(|asset| {
            asset["failure_code"] == "decode"
                && asset["source_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("invalid.jpg"))
        }));
        assert!(failed_assets.iter().any(|asset| {
            asset["failure_code"] == "resource_pressure"
                && asset["source_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("oversized.jpg"))
                && asset["failure_message"]
                    .as_str()
                    .is_some_and(|message| message.contains("oversized.jpg"))
        }));
        assert!(failed_assets.iter().any(|asset| {
            asset["skipped_code"] == "no_face"
                && asset["source_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("valid-no-face.png"))
        }));
        drop(service);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_face_maps_to_legacy_no_face_verdict() {
        let row = FacialService::identity_gate_failure_row(
            "blank.png",
            crate::identity::IdentityError {
                code: "missing_face".to_string(),
                message: "no face".to_string(),
            },
            "generation-1",
            crate::identity::EMBEDDING_DIM,
        );
        assert_eq!(row["verdict"], "no_face");
        assert_eq!(row["face_count"], 0);
        assert!(row["error"].is_null());
        assert_eq!(row["model_generation"], "generation-1");
    }

    #[test]
    fn identity_settings_save_failure_preserves_manifest_engine_and_restart() {
        let model = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("w600k_r50.onnx");
        if !model.is_file() {
            return;
        }
        let root = test_root("identity_save_rollback");
        let manifest_path = root
            .join(".facial")
            .join("models")
            .join("match-inference-manifest-v1.json");
        let accepted = IdentityEngine::provision(&model, None, &manifest_path).unwrap();
        let accepted_generation = accepted.generation().to_string();
        drop(accepted);
        let accepted_manifest = fs::read(&manifest_path).unwrap();

        let mut config = test_config(&root, None);
        config.identity_model_path = Some(model.clone());
        config.identity_manifest_path = Some(manifest_path.clone());
        let settings_path = root.join("product").join("config").join("default.json");
        crate::config::save_identity_paths(
            &config,
            &config.identity_model_path,
            &config.identity_detector_path,
            &config.identity_manifest_path,
        )
        .unwrap();
        let accepted_settings = fs::read(&settings_path).unwrap();

        let mut service = FacialService::new(config.clone());
        assert_eq!(
            service.identity_status()["model_generation"],
            accepted_generation
        );
        crate::config::inject_settings_write_failure_after(Some(24));
        let provision_result = service.set_identity_paths(&model.to_string_lossy(), "");
        crate::config::inject_settings_write_failure_after(None);
        let error = provision_result.unwrap_err();
        assert!(error.contains("identity settings save failed"), "{error}");
        assert_eq!(fs::read(&manifest_path).unwrap(), accepted_manifest);
        assert_eq!(fs::read(&settings_path).unwrap(), accepted_settings);
        assert_eq!(
            service.identity_status()["model_generation"],
            accepted_generation
        );

        let _env_guard = crate::config::test_env_lock().lock().unwrap();
        let prior_repo_root = std::env::var_os("FACIAL_REPO_ROOT");
        let prior_config_path = std::env::var_os("FACIAL_CONFIG_PATH");
        let prior_workspace_root = std::env::var_os("FACIAL_WORKSPACE_ROOT");
        let prior_manifest = std::env::var_os("FACIAL_IDENTITY_MANIFEST");
        std::env::set_var("FACIAL_REPO_ROOT", &root);
        std::env::set_var("FACIAL_CONFIG_PATH", &settings_path);
        std::env::remove_var("FACIAL_WORKSPACE_ROOT");
        std::env::remove_var("FACIAL_IDENTITY_MANIFEST");
        let disk_config = crate::config::load_config();
        let restarted = FacialService::new(disk_config);
        assert_eq!(restarted.identity_status()["state"], "ready");
        assert_eq!(
            restarted.identity_status()["model_generation"],
            accepted_generation
        );
        for (key, prior) in [
            ("FACIAL_REPO_ROOT", prior_repo_root),
            ("FACIAL_CONFIG_PATH", prior_config_path),
            ("FACIAL_WORKSPACE_ROOT", prior_workspace_root),
            ("FACIAL_IDENTITY_MANIFEST", prior_manifest),
        ] {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        drop(restarted);
        drop(service);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wp086_compute_requests_exclude_database_writer_and_index_claims() {
        let request = match_compute_request(1024, 4096);
        assert_eq!(request.surreal_writes, 0);
        assert_eq!(request.vector_index_builds, 0);
        assert_eq!(request.cpu_inference, 1);
        assert_eq!(request.decoded_bytes, 4096);
        assert!(match_admission_interrupted(
            "worker admission paused or held"
        ));
        assert!(!match_admission_interrupted("safe_unit_timeout"));
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_production_cpu_two_active_admission_releases_idle_and_honors_hold() {
        use crate::match_store::{JobLifecycle, MatchExternalHolds, MatchStore};
        use crate::match_worker::{CpuExecutionPolicy, IsolatedMatchWorker};
        use crate::media_io::{MediaIoCoordinator, PermitOutcome, RootIdentity, RootKind};
        let root = test_root("wp086-production-cpu-two");
        let holds = Arc::new(MatchExternalHolds::default());
        let store = MatchStore::open(&root)
            .unwrap()
            .with_external_holds(holds.clone());
        let configured = store.configure_index_root(&root, vec![]).unwrap();
        store
            .register_model_generation("cpu-two-model", true)
            .unwrap();
        let job = store
            .create_job(&configured.root_id, "cpu-two-model")
            .unwrap();
        let job = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let coordinator = MediaIoCoordinator::new();
        let identity =
            RootIdentity::new(configured.root_id, job.identity_revision, RootKind::Unknown);
        let request = match_cpu_compute_request(CpuExecutionPolicy::PrivateTwoThread, 65536, 1);
        assert_eq!(request.cpu_inference, 2);
        assert_eq!(
            match_cpu_compute_request(CpuExecutionPolicy::Baseline, 65536, 1).cpu_inference,
            1
        );
        let mut worker = IsolatedMatchWorker::spawn_fault_harness().unwrap();
        assert!(worker.production_cpu_acknowledgement().is_none());
        let one = store
            .acquire_worker_compute(
                &coordinator,
                identity.clone(),
                &job.job_id,
                None,
                match_compute_request(65536, 1),
                &mut None,
            )
            .unwrap();
        let fence = match_worker_fence(&job, None, one.admission_epoch());
        assert!(worker
            .initialize_production_cpu(CpuExecutionPolicy::PrivateTwoThread, &one, &fence)
            .is_err());
        assert!(worker.production_cpu_acknowledgement().is_none());
        one.finish(PermitOutcome::Cancelled);
        let compute = store
            .acquire_worker_compute(
                &coordinator,
                identity.clone(),
                &job.job_id,
                None,
                request,
                &mut None,
            )
            .unwrap();
        let fence = match_worker_fence(&job, None, compute.admission_epoch());
        worker
            .initialize_production_cpu(CpuExecutionPolicy::PrivateTwoThread, &compute, &fence)
            .unwrap();
        let acknowledged =
            serde_json::to_value(worker.production_cpu_acknowledgement().unwrap()).unwrap();
        assert_eq!(acknowledged["threads"], 2);
        assert_eq!(acknowledged["model_generation"], "cpu-two-model");
        assert_eq!(acknowledged["worker_id"], worker.worker_id());
        assert_eq!(store.governor().usage().unwrap().cpu_inference, 2);
        assert!(store.governor().try_acquire(request).is_err());
        compute.finish_after_worker(PermitOutcome::Success, &worker);
        assert!(!worker.confirmed_dead());
        assert_eq!(store.governor().usage().unwrap().cpu_inference, 0);
        holds.set_fullscreen(true);
        assert!(store
            .acquire_worker_compute(
                &coordinator,
                identity.clone(),
                &job.job_id,
                None,
                request,
                &mut None
            )
            .is_err());
        holds.set_fullscreen(false);
        let resumed = store
            .acquire_worker_compute(
                &coordinator,
                identity,
                &job.job_id,
                None,
                request,
                &mut None,
            )
            .unwrap();
        assert_ne!(resumed.admission_epoch(), fence.admission_epoch);
        resumed.finish(PermitOutcome::Cancelled);
        assert!(worker.shutdown_and_confirm());
        drop(worker);
        let db_path = crate::media_db::MediaDb::db_path(&root);
        drop(store);
        crate::surreal_store::wait_until_closed(&db_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_late_owned_job_exit_acknowledges_quarantine_and_admits_retry() {
        use crate::match_store::{JobLifecycle, MatchStore};
        use crate::match_worker::IsolatedMatchWorker;
        let root = test_root("wp086-late-owned-exit");
        let store = MatchStore::open(&root).unwrap();
        let configured = store.configure_index_root(&root, vec![]).unwrap();
        store
            .register_model_generation("late-exit-model", true)
            .unwrap();
        let job = store
            .create_job(&configured.root_id, "late-exit-model")
            .unwrap();
        let job = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        // This is a real owned, idle child waiting on its private request pipe.
        // Keep it alive beyond the foreground receipt window, then confirm the
        // exact process/Job exit through duplicated native observation handles.
        let mut child = IsolatedMatchWorker::spawn_fault_harness().unwrap();
        let observer = child.owned_exit_observer().unwrap();
        assert!(!observer().unwrap());
        store
            .record_worker_quarantine(
                &job.job_id,
                None,
                child.worker_id(),
                &job.model_generation,
                "worker_failed",
                "late owned exit test",
                false,
            )
            .unwrap();
        let receipt =
            schedule_confirmed_match_worker_exit(store.clone(), child.worker_id().into(), observer)
                .unwrap();
        assert!(store
            .control_job(&job.job_id, "retry")
            .unwrap_err()
            .contains("exit is not confirmed"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!child.confirmed_dead());
        assert!(store
            .control_job(&job.job_id, "retry")
            .unwrap_err()
            .contains("exit is not confirmed"));
        assert!(child.shutdown_and_confirm());
        receipt.join().unwrap();
        let retried = store.control_job(&job.job_id, "retry").unwrap();
        assert_eq!(retried.lifecycle, "retrying");
        let coordinator = crate::media_io::MediaIoCoordinator::new();
        let compute = store
            .acquire_worker_compute(
                &coordinator,
                crate::media_io::RootIdentity::new(
                    configured.root_id,
                    retried.identity_revision,
                    crate::media_io::RootKind::Unknown,
                ),
                &retried.job_id,
                None,
                match_compute_request(65_536, 1),
                &mut None,
            )
            .unwrap();
        compute.finish(crate::media_io::PermitOutcome::Success);
        let fresh = IsolatedMatchWorker::spawn_retry(&mut child).unwrap();
        assert_ne!(fresh.worker_id(), child.worker_id());
        drop(fresh);
        drop(child);
        let db_path = crate::media_db::MediaDb::db_path(&root);
        drop(store);
        crate::surreal_store::wait_until_closed(&db_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_isolated_timeout_releases_compute_and_preserves_operator_pause() {
        use crate::match_store::{DesiredMode, JobLifecycle, MatchExternalHolds, MatchStore};
        use crate::match_worker::{HarnessFault, IsolatedMatchWorker};
        use crate::media_io::{MediaIoCoordinator, PermitOutcome, RootIdentity, RootKind};
        let root = test_root("wp086_timeout_quarantine");
        let holds = Arc::new(MatchExternalHolds::default());
        let store = MatchStore::open(&root)
            .unwrap()
            .with_external_holds(Arc::clone(&holds));
        let configured = store.configure_index_root(&root, vec![]).unwrap();
        store
            .register_model_generation("wp086-timeout-model", true)
            .unwrap();
        let job = store
            .create_job(&configured.root_id, "wp086-timeout-model")
            .unwrap();
        let job = store
            .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
            .unwrap();
        let coordinator = MediaIoCoordinator::new();
        let root_identity =
            RootIdentity::new(configured.root_id, job.identity_revision, RootKind::Unknown);
        let compute = store
            .acquire_worker_compute(
                &coordinator,
                root_identity,
                &job.job_id,
                None,
                match_compute_request(65536, 1),
                &mut None,
            )
            .unwrap();
        let admitted = match_worker_fence(&job, None, compute.admission_epoch());
        let usage = store.status().unwrap();
        assert_eq!(usage["execution"]["resource_usage"]["surreal_writes"], 0);
        let mut child = IsolatedMatchWorker::spawn_fault_harness().unwrap();
        // Both controls land after compute admission; timeout settlement must
        // retain them and must not wait for them to release before reporting.
        holds.set_fullscreen(true);
        store.set_desired_mode(DesiredMode::OperatorPaused).unwrap();
        let error = child
            .exercise_fault(HarnessFault::Hang, &admitted)
            .unwrap_err();
        compute.finish_after_worker(PermitOutcome::Error, &child);
        assert!(
            settle_match_worker_failure(&store, &job, None, &child, &error)
                .unwrap_err()
                .contains("safe_unit_timeout")
        );
        let failed = store.job(&job.job_id).unwrap();
        assert_eq!(failed.failure_code.as_deref(), Some("safe_unit_timeout"));
        assert_eq!(failed.lifecycle, "failed");
        let status = store.status().unwrap();
        assert_eq!(status["execution"]["desired_mode"], "operator_paused");
        assert!(holds.fullscreen());
        for axis in [
            "admitted_items",
            "queued_items",
            "queued_bytes",
            "cpu_inference",
            "decoded_bytes",
            "surreal_writes",
            "vector_index_builds",
        ] {
            assert_eq!(
                status["execution"]["resource_usage"][axis], 0,
                "leaked compute axis {axis}"
            );
        }
        assert!(child.confirmed_dead());
        let fresh = IsolatedMatchWorker::spawn_retry(&mut child).unwrap();
        assert_ne!(fresh.worker_id(), child.worker_id());
        drop(fresh);
        drop(child);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial_service_test_{}_{}",
            name,
            Uuid::new_v4().to_string().replace('-', "_")
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_config(root: &Path, copy_location: Option<PathBuf>) -> AppConfig {
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
            copy_location,
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

    fn write_test_image(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not-a-real-image").unwrap();
    }

    // WP-085 boundary proof: exercise the real service/store exchange route.
    #[test]
    fn wp085_identity_maintenance_service_boundary_round_trips_actual_receipts() {
        use crate::api::{MatchMaintenanceAction as Action, MatchMaintenanceRequest};

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
        let source_root = test_root("match_identity_maintenance_source");
        let mut source = FacialService::new(test_config(&source_root, None));
        let created = source
            .match_create_person("Boundary Person", vec!["Boundary Alias".to_string()])
            .unwrap();
        let person_id = created["person_id"].as_str().unwrap().to_string();

        let preview = source
            .match_maintenance(
                "identity-export-preview",
                &request(Action::IdentityExportPreview),
            )
            .unwrap();
        assert_eq!(preview["format"], "facial-identity-bundle");
        assert_eq!(preview["version"], 1);
        assert!(preview["content_sha256"]
            .as_str()
            .is_some_and(|v| v.len() == 64));
        assert!(preview["canonical_bytes"].as_u64().unwrap() > 0);
        assert!(preview["entity_count"].as_u64().unwrap() > 0);
        assert_eq!(preview["excluded_payloads"][0], "face_embeddings");

        let bundle_path = source_root.join("boundary-identity.facial-identity.json");
        let mut export = request(Action::IdentityExport);
        export.path = Some(bundle_path.to_string_lossy().to_string());
        export.expected_digest = preview["content_sha256"].as_str().map(str::to_string);
        export.confirmed = true;
        let exported = source
            .match_maintenance("identity-export", &export)
            .unwrap();
        assert_eq!(exported["content_sha256"], preview["content_sha256"]);
        assert_eq!(exported["canonical_bytes"], preview["canonical_bytes"]);
        assert!(bundle_path.is_file());

        let target_root = test_root("match_identity_maintenance_target");
        let mut target = FacialService::new(test_config(&target_root, None));
        let mut dry_run = request(Action::IdentityImportDryRun);
        dry_run.path = Some(bundle_path.to_string_lossy().to_string());
        dry_run.conflict_policy = Some("replace".to_string());
        let plan = target
            .match_maintenance("identity-import-dry-run", &dry_run)
            .unwrap();
        assert_eq!(plan["mode"], "replace");
        assert_eq!(plan["content_sha256"], preview["content_sha256"]);
        assert!(plan["creates"].as_u64().unwrap() > 0);
        assert_eq!(plan["updates"], 0);
        assert_eq!(plan["deletes"], 0);
        assert_eq!(plan["conflict_count"], 0);
        assert_eq!(plan["unresolved_root_count"], 0);
        assert_eq!(plan["unresolved_media_count"], 0);
        let import_token = plan["plan_token"].as_str().unwrap().to_string();

        let mut apply = request(Action::IdentityImport);
        apply.path = Some(bundle_path.to_string_lossy().to_string());
        apply.confirmation_token = Some(import_token.clone());
        apply.conflict_policy = Some("replace".to_string());
        apply.confirmed = true;
        target.match_set_operator_paused(true).unwrap();
        let imported = target.match_maintenance("identity-import", &apply).unwrap();
        assert_eq!(imported["receipt"]["plan_token"], import_token);
        assert_eq!(imported["receipt"]["created"], plan["creates"]);
        assert!(imported["receipt"]["post_state_sha256"]
            .as_str()
            .is_some_and(|v| v.len() == 64));
        assert!(imported["receipt"].get("rollback").is_none());
        let rollback_path =
            PathBuf::from(imported["rollback_bundle"]["output_path"].as_str().unwrap());
        assert!(rollback_path.is_file());
        let rollback_bundle =
            crate::match_store::MatchStore::read_identity_bundle(&rollback_path).unwrap();
        assert_eq!(rollback_bundle.manifest.format, "facial-identity-bundle");
        assert_eq!(
            rollback_bundle.manifest.content_sha256,
            imported["rollback_bundle"]["content_sha256"]
                .as_str()
                .unwrap()
        );
        assert!(target
            .match_person_search_autocomplete("boundary", 8)
            .unwrap()
            .iter()
            .any(|person| person.person_id == person_id));

        let mut rollback_preview = request(Action::IdentityImportDryRun);
        rollback_preview.path = Some(rollback_path.to_string_lossy().to_string());
        rollback_preview.conflict_policy = Some("replace".to_string());
        let rollback_plan = target
            .match_maintenance("identity-rollback-preview", &rollback_preview)
            .unwrap();
        assert!(rollback_plan["deletes"].as_u64().unwrap() > 0);
        let rollback_token = rollback_plan["plan_token"].as_str().unwrap().to_string();
        let mut rollback = request(Action::IdentityImportRollback);
        rollback.path = Some(rollback_path.to_string_lossy().to_string());
        rollback.confirmation_token = Some(rollback_token.clone());
        rollback.conflict_policy = Some("replace".to_string());
        rollback.confirmed = true;
        let rolled_back = target
            .match_maintenance("identity-import-rollback", &rollback)
            .unwrap();
        assert_eq!(rolled_back["plan_token"], rollback_token);
        assert_eq!(rolled_back["deleted"], rollback_plan["deletes"]);
        assert!(target
            .match_person_search_autocomplete("boundary", 8)
            .unwrap()
            .is_empty());

        drop(target);
        drop(source);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&target_root))
            .unwrap();
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&source_root))
            .unwrap();
        fs::remove_dir_all(target_root).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn copy_mode_imports_and_runs_under_selected_copy_location() {
        let root = test_root("copy_mode");
        let source = root.join("source").join("face.jpg");
        let output = root.join("selected-output");
        write_test_image(&source);
        let mut service = FacialService::new(test_config(&root, Some(output.clone())));

        let imports = service.ingest_images(
            "Client Shoot",
            &[source.to_string_lossy().to_string()],
            false,
        );

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].mode, "copy");
        assert!(Path::new(&imports[0].destination).starts_with(&output));
        assert!(Path::new(&imports[0].destination).is_file());

        let summary = service
            .run_pipeline(
                "Client Shoot",
                &[imports[0].destination.clone()],
                &["invalid-feature-key".to_string()],
                None,
                false,
            )
            .unwrap();

        assert!(Path::new(&summary.output_path).starts_with(output.join("runs")));
    }

    #[test]
    fn in_place_mode_keeps_original_paths_and_runs_under_source_parent() {
        let root = test_root("in_place");
        let source_parent = root.join("shoot");
        let source = source_parent.join("face.jpg");
        let output = root.join("selected-output");
        write_test_image(&source);
        let mut service = FacialService::new(test_config(&root, Some(output)));

        let imports = service.ingest_images(
            "Client Shoot",
            &[source.to_string_lossy().to_string()],
            true,
        );

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].mode, "in_place");
        assert_eq!(PathBuf::from(&imports[0].destination), source);

        let summary = service
            .run_pipeline(
                "Client Shoot",
                &[imports[0].destination.clone()],
                &["invalid-feature-key".to_string()],
                None,
                true,
            )
            .unwrap();

        assert!(
            Path::new(&summary.output_path).starts_with(source_parent.join(".facial").join("runs"))
        );
    }

    #[test]
    fn lane_state_is_reachable_through_service_methods() {
        let root = test_root("lanes_service");
        let source = root.join("source");
        write_test_image(&source.join("a.jpg"));
        let mut service = FacialService::new(test_config(&root, None));

        let lanes = service.list_lanes().unwrap();
        assert_eq!(lanes.len(), 2);

        let updated = service
            .set_lane(
                "lane-001",
                "Shoot A",
                "batch",
                &source.to_string_lossy(),
                true,
                &["facet:quality_pass".to_string()],
            )
            .unwrap();
        assert_eq!(updated.lane_id, "lane-001");
        assert_eq!(updated.item_count, 0);

        let scan = service.scan_lane("lane-001").unwrap();
        assert_eq!(scan.item_count, 1);

        let claimed = service.claim_lane("lane-001", "agent-a", false).unwrap();
        assert_eq!(claimed.claim_owner.as_deref(), Some("agent-a"));
        assert!(service.claim_lane("lane-001", "agent-b", false).is_err());

        let released = service.release_lane("lane-001", "agent-a", false).unwrap();
        assert_eq!(released.claim_owner, None);

        let status = service.lane_status(Some("lane-001")).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].item_count, 1);
    }

    #[test]
    fn lane_batch_runs_scanned_lane_inventory() {
        let root = test_root("lane_batch");
        let source = root.join("source");
        let output = root.join("out");
        write_test_image(&source.join("a.jpg"));
        let mut service = FacialService::new(test_config(&root, Some(output)));
        service
            .set_lane(
                "lane-001",
                "Lane One",
                "batch",
                &source.to_string_lossy(),
                true,
                &["invalid-feature-key".to_string()],
            )
            .unwrap();
        service.scan_lane("lane-001").unwrap();

        let result = service
            .start_lane_batch(
                "lane-001",
                "Batch Project",
                &[],
                false,
                Some("agent-a"),
                false,
            )
            .unwrap();

        assert_eq!(result.lane_id, "lane-001");
        assert_eq!(result.item_count, 1);
        assert_eq!(result.status, "partial");
        assert!(result.run_id.is_some());
        assert!(Path::new(result.output_path.as_deref().unwrap()).is_file());
    }

    #[test]
    fn all_lane_batches_report_mixed_success_without_aborting() {
        let root = test_root("all_lane_batches");
        let source = root.join("source");
        let output = root.join("out");
        write_test_image(&source.join("a.jpg"));
        let mut service = FacialService::new(test_config(&root, Some(output)));
        service
            .set_lane(
                "lane-001",
                "Valid",
                "batch",
                &source.to_string_lossy(),
                true,
                &["invalid-feature-key".to_string()],
            )
            .unwrap();
        service.scan_lane("lane-001").unwrap();
        service
            .set_lane(
                "lane-002",
                "Missing",
                "batch",
                "",
                true,
                &["invalid-feature-key".to_string()],
            )
            .unwrap();

        let aggregate = service
            .start_all_lane_batches("Batch Project", &[], 2, false, Some("agent-a"), false)
            .unwrap();

        assert_eq!(aggregate.concurrency_limit, 2);
        assert_eq!(aggregate.total_lanes, 2);
        assert_eq!(aggregate.results.len(), 2);
        assert_eq!(aggregate.ok, 1);
        assert_eq!(aggregate.failed, 1);
        let valid = aggregate
            .results
            .iter()
            .find(|result| result.lane_id == "lane-001")
            .unwrap();
        assert!(valid.run_id.is_some());
        let missing = aggregate
            .results
            .iter()
            .find(|result| result.lane_id == "lane-002")
            .unwrap();
        assert!(missing
            .error
            .as_deref()
            .unwrap()
            .contains("scanned inventory"));
    }

    #[test]
    fn failed_lane_batch_persists_recovery_error_in_lane_status() {
        let root = test_root("failed_lane_batch_status");
        let output = root.join("out");
        let mut service = FacialService::new(test_config(&root, Some(output)));
        service
            .set_lane(
                "lane-001",
                "Missing Scan",
                "batch",
                "",
                true,
                &["invalid-feature-key".to_string()],
            )
            .unwrap();

        let err = service
            .start_lane_batch(
                "lane-001",
                "Batch Project",
                &[],
                false,
                Some("agent-a"),
                false,
            )
            .unwrap_err();

        assert!(err.contains("scanned inventory"));
        let status = service.lane_status(Some("lane-001")).unwrap();
        assert_eq!(status.len(), 1);
        assert!(status[0].last_error.contains("scanned inventory"));
    }
}

fn is_image_path(path: &Path) -> bool {
    match path.extension().and_then(|value| value.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "gif"
        ),
        None => false,
    }
}

fn normalize_paths(image_paths: &[String], fallback: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for raw in image_paths {
        let path = Path::new(raw);
        if path.is_file() && is_image_path(path) {
            out.push(path.to_string_lossy().to_string());
        } else if path.is_dir() {
            for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
                if entry.path().is_file() && is_image_path(entry.path()) {
                    out.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    if out.is_empty() && fallback.join("images").exists() {
        for entry in WalkDir::new(fallback.join("images"))
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.path().is_file() && is_image_path(entry.path()) {
                out.push(entry.path().to_string_lossy().to_string());
            }
        }
    }
    out
}

fn common_image_parent(image_paths: &[String]) -> Option<PathBuf> {
    let mut parents = image_paths
        .iter()
        .filter_map(|path| Path::new(path).parent().map(Path::to_path_buf));
    let mut common = parents.next()?;
    for parent in parents {
        while !parent.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

fn parse_lane_mode(raw: &str) -> Result<LaneMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "compare" => Ok(LaneMode::Compare),
        "review" => Ok(LaneMode::Review),
        "batch" => Ok(LaneMode::Batch),
        other => Err(format!("unknown lane mode: {other}")),
    }
}

fn effective_batch_features(lane: &LaneRecord, override_keys: &[String]) -> Vec<String> {
    if override_keys.is_empty() {
        lane.feature_keys.clone()
    } else {
        override_keys.to_vec()
    }
}

fn lane_batch_error(
    lane: &LaneRecord,
    action_id: &str,
    feature_keys: &[String],
    err: String,
) -> LaneBatchResult {
    LaneBatchResult {
        lane_id: lane.lane_id.clone(),
        action_id: action_id.to_string(),
        status: "error".to_string(),
        item_count: lane.item_count,
        feature_keys: effective_batch_features(lane, feature_keys),
        run_id: None,
        output_path: None,
        error: Some(err),
    }
}
