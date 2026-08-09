use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
};

use chrono::Utc;
use regex::Regex;
use serde_json::json;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    config::AppConfig,
    debug::{DebugBus, DebugEvent},
    identity::IdentityEngine,
    lanes::{
        LaneBatchAggregate, LaneBatchResult, LaneMode, LaneRecord, LaneScanResult, LaneStore,
        LaneUpdate,
    },
    models::{IngestResult, ModelRecord, PluginRunResult, RunSummary},
    plugin_host::PluginHost,
};

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

pub struct FacialService {
    config: AppConfig,
    debug: DebugBus,
    registry: ModelRegistry,
    plugins: PluginHost,
    worktrees: WorktreeManager,
    copy_location: Option<PathBuf>,
    run_results_index: Vec<PathBuf>,
    identity: Option<IdentityEngine>,
    identity_refs: Option<Vec<Vec<f32>>>,
    identity_negs: Option<Vec<Vec<f32>>>,
    /// Lazy PIPNet 98-pt landmark engine (WP-021); loaded on first gate use.
    landmarks: Option<crate::landmarks::LandmarkEngine>,
    landmarks_load_attempted: bool,
}

impl FacialService {
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
        let identity = config.identity_model_path.as_ref().and_then(|path| {
            match IdentityEngine::load(path, config.identity_detector_path.as_deref()) {
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
                    Some(engine)
                }
                Err(err) => {
                    debug.emit(
                        "WARN",
                        "Identity",
                        &format!("identity model load failed: {err}"),
                        None,
                    );
                    None
                }
            }
        });
        let mut service = Self {
            config,
            debug,
            registry,
            plugins,
            worktrees,
            copy_location,
            run_results_index: Vec::new(),
            identity,
            identity_refs: None,
            identity_negs: None,
            landmarks: None,
            landmarks_load_attempted: false,
        };
        service.sync_detector_registry();
        service
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

    /// Set + reload the identity engine from new model/detector paths.
    /// Persists paths to default.json so the GUI loads identity on next launch.
    /// Pass empty string for detector_path to clear the detector (resize fallback).
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
        let engine = IdentityEngine::load(&model_pb, det_pb.as_deref())
            .map_err(|e| format!("identity engine load failed: {e}"))?;
        let align = engine.align_method().to_string();
        let sha = engine.model_sha256().to_string();
        let detector_origin = engine.detector_origin().to_string();
        self.config.identity_model_path = Some(model_pb.clone());
        self.config.identity_detector_path = det_pb.clone();
        self.identity = Some(engine);
        self.identity_refs = None;
        self.identity_negs = None;
        self.sync_detector_registry();
        let _ = crate::config::save_identity_paths(
            &self.config,
            &self.config.identity_model_path.clone(),
            &self.config.identity_detector_path.clone(),
        );
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
                "model_path": engine.model_path().to_string_lossy(),
                "model_sha256": engine.model_sha256(),
                "align_capability": engine.align_method(),
                "detector": engine.has_detector(),
                "detector_origin": engine.detector_origin(),
                "detector_sha256": engine.detector_sha256(),
                "reference_dir": self.config.identity_reference_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                "negative_dir": self.config.identity_negative_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                "threshold": self.config.identity_threshold,
                "required_margin": self.config.identity_margin,
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
                "reason": "no identity model provisioned (set identity_model_path or FACIAL_IDENTITY_MODEL)",
            }),
        }
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
    fn gate_row(
        engine: &IdentityEngine,
        landmarks: Option<&crate::landmarks::LandmarkEngine>,
        refs: &[Vec<f32>],
        negs: &[Vec<f32>],
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
                return json!({
                    "image": image,
                    "verdict": "error",
                    "source": "real",
                    "error": e,
                    "face_count": 0,
                    "face_box": serde_json::Value::Null,
                    "face_frac": serde_json::Value::Null,
                    "face_score": serde_json::Value::Null,
                    "framing": "none",
                });
            }
        };
        let max_sim = |set: &[Vec<f32>]| -> f32 {
            set.iter()
                .map(|v| crate::identity::cosine(&detect.embedding, v))
                .fold(f32::NEG_INFINITY, f32::max)
        };
        let ref_sim = if refs.is_empty() { 0.0 } else { max_sim(refs) };
        let neg_sim = if negs.is_empty() { 0.0 } else { max_sim(negs) };
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
            "align": if detect.aligned { "yunet_112" } else { "resize_112" },
            "model_sha256": engine.model_sha256(),
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
            embedding: Vec<f32>,
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

        let embeddings: Vec<Vec<f32>> = items.iter().map(|i| i.embedding.clone()).collect();
        let assignment = crate::identity::cluster_embeddings(&embeddings, threshold);

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
                        "similarity_to_rep": crate::identity::cosine(
                            &items[idx].embedding,
                            &items[rep_idx].embedding
                        ),
                        "face_score": items[idx].face_score,
                        "face_crop_sharpness": items[idx].sharpness,
                    })
                })
                .collect();
            let sims: Vec<f32> = member_idxs
                .iter()
                .map(|&idx| {
                    crate::identity::cosine(&items[idx].embedding, &items[rep_idx].embedding)
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
                pairwise.push(crate::identity::cosine(&refs[i], &refs[j]));
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
            .map(|n| {
                refs.iter()
                    .map(|r| crate::identity::cosine(n, r))
                    .fold(f32::NEG_INFINITY, f32::max)
            })
            .collect();
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
            let sim = engine
                .embed_file(anchor)
                .ok()
                .map(|emb| crate::identity::cosine(&candidate_emb, &emb));
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
        let mut embs: Vec<(String, Vec<f32>)> = Vec::new();
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
                        let max_c = e.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                        let min_c = e.iter().cloned().fold(f32::INFINITY, f32::min);
                        let head: Vec<f32> = e.iter().cloned().take(12).collect();
                        serde_json::json!({
                            "path": path,
                            "source": "real",
                            "embedding_dim": e.len(),
                            "embedding_norm": 1.0,
                            "embedding": e,
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
                        let sim = crate::identity::cosine(&embs[i].1, &embs[j].1);
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
                        .map(|(_, (p, e))| (p.clone(), crate::identity::cosine(qe, e)))
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

    fn load_dir_embeddings(engine: &IdentityEngine, dir: Option<&Path>) -> Vec<Vec<f32>> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
