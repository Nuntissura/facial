use std::{
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};

pub const LANES_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneMode {
    Compare,
    Review,
    Batch,
}

impl Default for LaneMode {
    fn default() -> Self {
        Self::Compare
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaneRecord {
    pub lane_id: String,
    pub name: String,
    pub mode: LaneMode,
    pub folder: String,
    pub recursive: bool,
    #[serde(default)]
    pub feature_keys: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub item_count: usize,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub batch_status: String,
    #[serde(default)]
    pub batch_action_id: Option<String>,
    #[serde(default)]
    pub batch_updated_at: Option<String>,
    #[serde(default)]
    pub last_run_id: Option<String>,
    #[serde(default)]
    pub last_batch_error: String,
    #[serde(default)]
    pub claim_owner: Option<String>,
    #[serde(default)]
    pub claim_updated_at: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct LaneUpdate {
    pub lane_id: String,
    pub name: Option<String>,
    pub mode: Option<LaneMode>,
    pub folder: Option<String>,
    pub recursive: Option<bool>,
    pub feature_keys: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaneScanResult {
    pub lane_id: String,
    pub item_count: usize,
    pub files: Vec<String>,
    pub dir_errors: usize,
    #[serde(default)]
    pub last_error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaneBatchResult {
    pub lane_id: String,
    pub action_id: String,
    pub status: String,
    pub item_count: usize,
    pub feature_keys: Vec<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub output_path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaneBatchAggregate {
    pub concurrency_limit: usize,
    pub total_lanes: usize,
    pub ok: usize,
    pub failed: usize,
    pub results: Vec<LaneBatchResult>,
}

#[derive(Serialize, Deserialize)]
struct LaneRegistry {
    schema_version: u32,
    updated_at: String,
    lanes: Vec<LaneRecord>,
}

#[derive(Serialize, Deserialize)]
struct LaneClaim {
    lane_id: String,
    actor: String,
    claimed_at: String,
}

pub struct LaneStore {
    workspace_root: PathBuf,
}

impl LaneStore {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    pub fn list_lanes(&self) -> Result<Vec<LaneRecord>, String> {
        let mut lanes = self.load_registry()?.lanes;
        self.reconcile_claims(&mut lanes);
        Ok(lanes)
    }

    pub fn lane_status(&self, lane_id: Option<&str>) -> Result<Vec<LaneRecord>, String> {
        let mut lanes = self.load_registry()?.lanes;
        self.reconcile_claims(&mut lanes);
        match lane_id {
            Some(id) => Ok(lanes
                .into_iter()
                .filter(|lane| lane.lane_id == id)
                .collect()),
            None => Ok(lanes),
        }
    }

    pub fn set_lane(&self, update: LaneUpdate) -> Result<LaneRecord, String> {
        self.set_lane_for_actor(update, None, false)
    }

    pub fn set_lane_for_actor(
        &self,
        update: LaneUpdate,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        if update.lane_id.trim().is_empty() {
            return Err("lane_id is required".to_string());
        }
        self.ensure_mutation_allowed(&update.lane_id, actor, steal)?;
        let mut registry = self.load_registry()?;
        self.reconcile_claims(&mut registry.lanes);
        let pos = registry
            .lanes
            .iter()
            .position(|lane| lane.lane_id == update.lane_id);
        let index = match pos {
            Some(index) => index,
            None => {
                registry.lanes.push(default_lane(&update.lane_id));
                registry.lanes.len() - 1
            }
        };
        let lane = &mut registry.lanes[index];
        if let Some(name) = update.name {
            lane.name = name;
        }
        if let Some(mode) = update.mode {
            lane.mode = mode;
        }
        if let Some(folder) = update.folder {
            lane.folder = folder;
            lane.files.clear();
            lane.item_count = 0;
            lane.last_error.clear();
        }
        if let Some(recursive) = update.recursive {
            lane.recursive = recursive;
        }
        if let Some(feature_keys) = update.feature_keys {
            lane.feature_keys = feature_keys;
        }
        let out = lane.clone();
        self.save_registry(&mut registry)?;
        Ok(out)
    }

    pub fn scan_lane(&self, lane_id: &str) -> Result<LaneScanResult, String> {
        self.scan_lane_for_actor(lane_id, None, false)
    }

    pub fn scan_lane_for_actor(
        &self,
        lane_id: &str,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneScanResult, String> {
        self.ensure_mutation_allowed(lane_id, actor, steal)?;
        let mut registry = self.load_registry()?;
        self.reconcile_claims(&mut registry.lanes);
        let Some(lane) = registry
            .lanes
            .iter_mut()
            .find(|lane| lane.lane_id == lane_id)
        else {
            return Err(format!("unknown lane: {lane_id}"));
        };
        if lane.folder.trim().is_empty() {
            lane.last_error = "lane folder is empty".to_string();
            let err = lane.last_error.clone();
            self.save_registry(&mut registry)?;
            return Err(err);
        }
        match collect_image_paths(Path::new(&lane.folder), lane.recursive) {
            Ok((files, dir_errors)) => {
                lane.item_count = files.len();
                lane.files = files.clone();
                lane.last_error.clear();
                let result = LaneScanResult {
                    lane_id: lane_id.to_string(),
                    item_count: files.len(),
                    files,
                    dir_errors,
                    last_error: String::new(),
                };
                self.save_registry(&mut registry)?;
                Ok(result)
            }
            Err(err) => {
                lane.files.clear();
                lane.item_count = 0;
                lane.last_error = err.clone();
                self.save_registry(&mut registry)?;
                Err(err)
            }
        }
    }

    pub fn scan_all_lanes(&self) -> Result<Vec<LaneScanResult>, String> {
        self.scan_all_lanes_for_actor(None, false)
    }

    pub fn scan_all_lanes_for_actor(
        &self,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<Vec<LaneScanResult>, String> {
        let ids: Vec<String> = self
            .list_lanes()?
            .into_iter()
            .map(|lane| lane.lane_id)
            .collect();
        let mut out = Vec::new();
        for id in ids {
            match self.scan_lane_for_actor(&id, actor, steal) {
                Ok(result) => out.push(result),
                Err(err) => {
                    out.push(LaneScanResult {
                        lane_id: id,
                        item_count: 0,
                        files: Vec::new(),
                        dir_errors: 0,
                        last_error: err,
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn lane_for_actor(
        &self,
        lane_id: &str,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        self.ensure_mutation_allowed(lane_id, actor, steal)?;
        let mut lanes = self.load_registry()?.lanes;
        self.reconcile_claims(&mut lanes);
        lanes
            .into_iter()
            .find(|lane| lane.lane_id == lane_id)
            .ok_or_else(|| format!("unknown lane: {lane_id}"))
    }

    pub fn record_batch_started(
        &self,
        lane_id: &str,
        action_id: &str,
    ) -> Result<LaneRecord, String> {
        self.update_batch_state(lane_id, "running", Some(action_id.to_string()), None, None)
    }

    pub fn record_batch_result(&self, result: &LaneBatchResult) -> Result<LaneRecord, String> {
        self.update_batch_state(
            &result.lane_id,
            &result.status,
            Some(result.action_id.clone()),
            result.run_id.clone(),
            result.error.clone(),
        )
    }

    pub fn claim_lane(
        &self,
        lane_id: &str,
        actor: &str,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        if actor.trim().is_empty() {
            return Err("actor is required to claim a lane".to_string());
        }
        self.ensure_known_lane(lane_id)?;
        fs::create_dir_all(self.claims_dir()).map_err(|e| format!("create claims dir: {e}"))?;
        let claim_path = self.claim_path(lane_id);
        if claim_path.exists() {
            if !steal {
                let existing = read_claim(&claim_path)
                    .map(|claim| claim.actor)
                    .unwrap_or_else(|_| "unknown".to_string());
                return Err(format!("lane {lane_id} already claimed by {existing}"));
            }
            let _ = fs::remove_file(&claim_path);
        }
        let claim = LaneClaim {
            lane_id: lane_id.to_string(),
            actor: actor.to_string(),
            claimed_at: Utc::now().to_rfc3339(),
        };
        let serialized =
            serde_json::to_string_pretty(&claim).map_err(|e| format!("claim encode: {e}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claim_path)
            .map_err(|e| format!("claim lane: {e}"))?;
        file.write_all(serialized.as_bytes())
            .map_err(|e| format!("write claim: {e}"))?;
        self.update_claim_owner(lane_id, Some(actor.to_string()), Some(claim.claimed_at))
    }

    pub fn release_lane(
        &self,
        lane_id: &str,
        actor: &str,
        steal: bool,
    ) -> Result<LaneRecord, String> {
        self.ensure_known_lane(lane_id)?;
        let claim_path = self.claim_path(lane_id);
        if claim_path.exists() {
            let claim = read_claim(&claim_path)?;
            if claim.actor != actor && !steal {
                return Err(format!("lane {lane_id} is claimed by {}", claim.actor));
            }
            fs::remove_file(&claim_path).map_err(|e| format!("release claim: {e}"))?;
        }
        self.update_claim_owner(lane_id, None, None)
    }

    fn ensure_known_lane(&self, lane_id: &str) -> Result<(), String> {
        if self
            .load_registry()?
            .lanes
            .iter()
            .any(|lane| lane.lane_id == lane_id)
        {
            Ok(())
        } else {
            Err(format!("unknown lane: {lane_id}"))
        }
    }

    fn ensure_mutation_allowed(
        &self,
        lane_id: &str,
        actor: Option<&str>,
        steal: bool,
    ) -> Result<(), String> {
        let claim_path = self.claim_path(lane_id);
        if !claim_path.exists() {
            return Ok(());
        }
        let claim = read_claim(&claim_path)?;
        if claim.lane_id != lane_id {
            return Err(format!(
                "claim file mismatch for {lane_id}: contains {}",
                claim.lane_id
            ));
        }
        if steal || actor.is_some_and(|actor| actor == claim.actor) {
            Ok(())
        } else {
            Err(format!("lane {lane_id} is claimed by {}", claim.actor))
        }
    }

    fn reconcile_claims(&self, lanes: &mut [LaneRecord]) {
        for lane in lanes {
            let claim_path = self.claim_path(&lane.lane_id);
            if claim_path.exists() {
                match read_claim(&claim_path) {
                    Ok(claim) if claim.lane_id == lane.lane_id => {
                        lane.claim_owner = Some(claim.actor);
                        lane.claim_updated_at = Some(claim.claimed_at);
                    }
                    Ok(claim) => {
                        lane.claim_owner = None;
                        lane.claim_updated_at = None;
                        lane.last_error = format!(
                            "claim file mismatch: expected {}, found {}",
                            lane.lane_id, claim.lane_id
                        );
                    }
                    Err(err) => {
                        lane.claim_owner = None;
                        lane.claim_updated_at = None;
                        lane.last_error = format!("claim read error: {err}");
                    }
                }
            } else {
                lane.claim_owner = None;
                lane.claim_updated_at = None;
            }
        }
    }

    fn update_claim_owner(
        &self,
        lane_id: &str,
        owner: Option<String>,
        updated_at: Option<String>,
    ) -> Result<LaneRecord, String> {
        let mut registry = self.load_registry()?;
        let Some(lane) = registry
            .lanes
            .iter_mut()
            .find(|lane| lane.lane_id == lane_id)
        else {
            return Err(format!("unknown lane: {lane_id}"));
        };
        lane.claim_owner = owner;
        lane.claim_updated_at = updated_at;
        let out = lane.clone();
        self.save_registry(&mut registry)?;
        Ok(out)
    }

    fn update_batch_state(
        &self,
        lane_id: &str,
        status: &str,
        action_id: Option<String>,
        run_id: Option<String>,
        error: Option<String>,
    ) -> Result<LaneRecord, String> {
        let mut registry = self.load_registry()?;
        let Some(lane) = registry
            .lanes
            .iter_mut()
            .find(|lane| lane.lane_id == lane_id)
        else {
            return Err(format!("unknown lane: {lane_id}"));
        };
        lane.batch_status = status.to_string();
        lane.batch_action_id = action_id;
        lane.batch_updated_at = Some(Utc::now().to_rfc3339());
        lane.last_run_id = run_id;
        lane.last_batch_error = error.clone().unwrap_or_default();
        if let Some(error) = error {
            lane.last_error = error;
        } else if !matches!(status, "error" | "failed") {
            lane.last_error.clear();
        }
        let out = lane.clone();
        self.save_registry(&mut registry)?;
        Ok(out)
    }

    fn load_registry(&self) -> Result<LaneRegistry, String> {
        let path = self.registry_path();
        if !path.exists() {
            let mut registry = LaneRegistry {
                schema_version: LANES_SCHEMA_VERSION,
                updated_at: Utc::now().to_rfc3339(),
                lanes: vec![default_lane("lane-001"), default_lane("lane-002")],
            };
            self.save_registry(&mut registry)?;
            return Ok(registry);
        }
        let raw = fs::read_to_string(&path).map_err(|e| format!("read lanes.json: {e}"))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse lanes.json: {e}"))
    }

    fn save_registry(&self, registry: &mut LaneRegistry) -> Result<(), String> {
        registry.updated_at = Utc::now().to_rfc3339();
        let path = self.registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create lanes dir: {e}"))?;
        }
        let serialized =
            serde_json::to_string_pretty(registry).map_err(|e| format!("encode lanes: {e}"))?;
        atomic_write(&path, &serialized).map_err(|e| format!("write lanes.json: {e}"))
    }

    fn registry_path(&self) -> PathBuf {
        self.lanes_dir().join("lanes.json")
    }

    fn claim_path(&self, lane_id: &str) -> PathBuf {
        self.claims_dir().join(format!("{lane_id}.json"))
    }

    fn lanes_dir(&self) -> PathBuf {
        self.workspace_root.join(".facial").join("lanes")
    }

    fn claims_dir(&self) -> PathBuf {
        self.lanes_dir().join("claims")
    }
}

fn default_lane(lane_id: &str) -> LaneRecord {
    LaneRecord {
        lane_id: lane_id.to_string(),
        name: String::new(),
        mode: LaneMode::Compare,
        folder: String::new(),
        recursive: true,
        feature_keys: Vec::new(),
        files: Vec::new(),
        item_count: 0,
        last_error: String::new(),
        batch_status: String::new(),
        batch_action_id: None,
        batch_updated_at: None,
        last_run_id: None,
        last_batch_error: String::new(),
        claim_owner: None,
        claim_updated_at: None,
    }
}

fn read_claim(path: &Path) -> Result<LaneClaim, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read claim: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse claim: {e}"))
}

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
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn collect_image_paths(root: &Path, recursive: bool) -> Result<(Vec<String>, usize), String> {
    if !root.exists() {
        return Err(format!("folder not found: {}", root.display()));
    }
    if root.is_file() {
        if is_supported_image(root) {
            return Ok((vec![root.to_string_lossy().to_string()], 0));
        }
        return Err(format!("not a supported image: {}", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("not a folder: {}", root.display()));
    }
    let mut files = Vec::new();
    let mut dir_errors = 0usize;
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                dir_errors += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if recursive {
                    queue.push(path);
                }
            } else if path.is_file() && is_supported_image(&path) {
                files.push(path.to_string_lossy().to_string());
            }
        }
    }
    files.sort();
    if files.is_empty() {
        Err(format!("no supported images under {}", root.display()))
    } else {
        Ok((files, dir_errors))
    }
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "gif"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial_lanes_test_{}_{}",
            name,
            Uuid::new_v4().to_string().replace('-', "_")
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn lane_registry_defaults_and_persists_updates() {
        let root = test_root("registry");
        let store = LaneStore::new(root.clone());

        let lanes = store.list_lanes().unwrap();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].lane_id, "lane-001");
        assert_eq!(lanes[0].mode, LaneMode::Compare);

        store
            .set_lane(LaneUpdate {
                lane_id: "lane-003".to_string(),
                name: Some("Shoot A".to_string()),
                mode: Some(LaneMode::Batch),
                folder: Some("D:/shoot-a".to_string()),
                recursive: Some(false),
                feature_keys: Some(vec!["facet:quality_pass".to_string()]),
            })
            .unwrap();

        assert!(root
            .join(".facial")
            .join("lanes")
            .join("lanes.json")
            .is_file());
        let reloaded = LaneStore::new(root).list_lanes().unwrap();
        let lane = reloaded
            .iter()
            .find(|lane| lane.lane_id == "lane-003")
            .unwrap();
        assert_eq!(lane.name, "Shoot A");
        assert_eq!(lane.mode, LaneMode::Batch);
        assert_eq!(lane.folder, "D:/shoot-a");
        assert!(!lane.recursive);
        assert_eq!(lane.feature_keys, vec!["facet:quality_pass"]);
    }

    #[test]
    fn scan_lane_records_sorted_supported_images_only() {
        let root = test_root("scan");
        let source = root.join("source");
        write_file(&source.join("b.png"));
        write_file(&source.join("ignore.txt"));
        write_file(&source.join("nested").join("a.jpg"));
        let store = LaneStore::new(root);
        store
            .set_lane(LaneUpdate {
                lane_id: "lane-001".to_string(),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                ..LaneUpdate::default()
            })
            .unwrap();

        let scanned = store.scan_lane("lane-001").unwrap();

        assert_eq!(scanned.item_count, 2);
        let mut sorted = scanned.files.clone();
        sorted.sort();
        assert_eq!(scanned.files, sorted);
        assert!(scanned.files.iter().any(|path| path.ends_with("b.png")));
        assert!(scanned
            .files
            .iter()
            .any(|path| path.ends_with("nested\\a.jpg") || path.ends_with("nested/a.jpg")));
        assert!(!scanned
            .files
            .iter()
            .any(|path| path.ends_with("ignore.txt")));
        let status = store.lane_status(Some("lane-001")).unwrap();
        assert_eq!(status[0].item_count, 2);
        assert!(status[0].last_error.is_empty());
    }

    #[test]
    fn lane_claims_are_exclusive_until_stolen_or_released() {
        let root = test_root("claims");
        let store = LaneStore::new(root);

        let first = store.claim_lane("lane-001", "agent-a", false).unwrap();
        assert_eq!(first.claim_owner.as_deref(), Some("agent-a"));
        assert!(store.claim_lane("lane-001", "agent-b", false).is_err());

        let stolen = store.claim_lane("lane-001", "agent-b", true).unwrap();
        assert_eq!(stolen.claim_owner.as_deref(), Some("agent-b"));
        assert!(store.release_lane("lane-001", "agent-a", false).is_err());

        let released = store.release_lane("lane-001", "agent-b", false).unwrap();
        assert_eq!(released.claim_owner, None);
    }

    #[test]
    fn claimed_lanes_reject_non_owner_mutation_and_scan() {
        let root = test_root("claim_guard");
        let source = root.join("source");
        write_file(&source.join("a.jpg"));
        let store = LaneStore::new(root);

        store.claim_lane("lane-001", "agent-a", false).unwrap();

        assert!(store
            .set_lane_for_actor(
                LaneUpdate {
                    lane_id: "lane-001".to_string(),
                    name: Some("hijack".to_string()),
                    ..LaneUpdate::default()
                },
                Some("agent-b"),
                false,
            )
            .is_err());
        assert!(store
            .scan_lane_for_actor("lane-001", Some("agent-b"), false)
            .is_err());

        let updated = store
            .set_lane_for_actor(
                LaneUpdate {
                    lane_id: "lane-001".to_string(),
                    folder: Some(source.to_string_lossy().to_string()),
                    ..LaneUpdate::default()
                },
                Some("agent-a"),
                false,
            )
            .unwrap();
        assert_eq!(updated.claim_owner.as_deref(), Some("agent-a"));
        let scanned = store
            .scan_lane_for_actor("lane-001", Some("agent-a"), false)
            .unwrap();
        assert_eq!(scanned.item_count, 1);
    }

    #[test]
    fn lane_status_reconciles_stranded_claim_file() {
        let root = test_root("stranded_claim");
        let store = LaneStore::new(root.clone());
        let _ = store.list_lanes().unwrap();
        let claims_dir = root.join(".facial").join("lanes").join("claims");
        fs::create_dir_all(&claims_dir).unwrap();
        fs::write(
            claims_dir.join("lane-001.json"),
            r#"{"lane_id":"lane-001","actor":"stranded-agent","claimed_at":"2026-07-03T00:00:00Z"}"#,
        )
        .unwrap();

        let status = store.lane_status(Some("lane-001")).unwrap();

        assert_eq!(status[0].claim_owner.as_deref(), Some("stranded-agent"));
        assert_eq!(
            status[0].claim_updated_at.as_deref(),
            Some("2026-07-03T00:00:00Z")
        );
    }

    #[test]
    fn scan_all_lanes_reports_errors_per_lane() {
        let root = test_root("scan_all");
        let source = root.join("source");
        write_file(&source.join("a.jpg"));
        let store = LaneStore::new(root);
        store
            .set_lane(LaneUpdate {
                lane_id: "lane-001".to_string(),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                ..LaneUpdate::default()
            })
            .unwrap();

        let results = store.scan_all_lanes().unwrap();

        assert_eq!(results.len(), 2);
        let ok = results
            .iter()
            .find(|result| result.lane_id == "lane-001")
            .unwrap();
        assert_eq!(ok.item_count, 1);
        assert!(ok.last_error.is_empty());
        let empty = results
            .iter()
            .find(|result| result.lane_id == "lane-002")
            .unwrap();
        assert_eq!(empty.item_count, 0);
        assert!(empty.last_error.contains("lane folder is empty"));
    }

    #[test]
    fn scan_all_lanes_allows_owner_actor_to_scan_owned_claims() {
        let root = test_root("scan_all_owner");
        let source = root.join("source");
        write_file(&source.join("a.jpg"));
        let store = LaneStore::new(root);
        store
            .set_lane(LaneUpdate {
                lane_id: "lane-001".to_string(),
                folder: Some(source.to_string_lossy().to_string()),
                recursive: Some(true),
                ..LaneUpdate::default()
            })
            .unwrap();
        store.claim_lane("lane-001", "agent-a", false).unwrap();

        let results = store
            .scan_all_lanes_for_actor(Some("agent-a"), false)
            .unwrap();

        let owned = results
            .iter()
            .find(|result| result.lane_id == "lane-001")
            .unwrap();
        assert_eq!(owned.item_count, 1);
        assert!(owned.last_error.is_empty());
    }
}
