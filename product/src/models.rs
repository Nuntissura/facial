use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_path: String,
    pub status: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaAssetNote {
    pub path: String,
    pub notes: String,
    pub tags: Vec<String>,
    pub color_label: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaBrowserIndex {
    pub favorites: Vec<String>,
    pub assets: Vec<MediaAssetNote>,
}

impl MediaBrowserIndex {
    pub fn empty() -> Self {
        Self {
            favorites: Vec::new(),
            assets: Vec::new(),
        }
    }

    pub fn upsert_asset(
        &mut self,
        path: &str,
        mut notes: Option<String>,
        mut tag_delta: Option<Vec<String>>,
    ) {
        if let Some(entry) = self.assets.iter_mut().find(|entry| entry.path == path) {
            if let Some(update_notes) = notes.take() {
                entry.notes = update_notes;
            }
            if let Some(mut incoming_tags) = tag_delta.take() {
                let mut next = Vec::new();
                for t in incoming_tags.drain(..) {
                    if !next.iter().any(|value| value == &t) {
                        next.push(t);
                    }
                }
                entry.tags = next;
            }
            return;
        }
        self.assets.push(MediaAssetNote {
            path: path.to_string(),
            notes: notes.take().unwrap_or_default(),
            tags: tag_delta.unwrap_or_default(),
            color_label: None,
        });
    }

    pub fn set_favorite(&mut self, path: &str, value: bool) {
        if value {
            if !self.favorites.iter().any(|item| item == path) {
                self.favorites.push(path.to_string());
            }
            return;
        }
        self.favorites.retain(|item| item != path);
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct IngestResult {
    pub source: String,
    pub destination: String,
    pub mode: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PluginRunResult {
    pub plugin_id: String,
    pub feature_id: String,
    pub status: String,
    pub message: String,
    pub payload: serde_json::Value,
    pub artifacts: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub project_name: String,
    pub worktree: String,
    pub images: Vec<String>,
    pub feature_keys: Vec<String>,
    pub status: String,
    pub in_place: bool,
    pub totals: std::collections::BTreeMap<String, i32>,
    pub plugin_results: Vec<PluginRunResult>,
    pub output_path: String,
}
