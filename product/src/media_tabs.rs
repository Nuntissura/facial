//! Pure Media-tab session state and persistence codec.
//!
//! Rendering and asynchronous Media work remain owned by `ui.rs`. This module
//! deliberately owns no database, scanner, thumbnail, video, or egui handle:
//! all tabs share those services and only the active tab is materialized into
//! the existing Media viewport. Inactive tabs are durable viewport snapshots.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// One transactional value in MediaDb's existing settings table.
pub const MEDIA_TABS_SETTING_KEY: &str = "media_tabs_v1";
/// Last rejected raw session value. It is retained separately so a safe
/// fallback cannot overwrite the operator's only recoverable tab state.
pub const MEDIA_TABS_RECOVERY_SETTING_KEY: &str = "media_tabs_v1_rejected";
pub const MEDIA_TABS_SCHEMA_VERSION: u32 = 1;

const MAX_TABS: usize = 256;
pub const MEDIA_TABS_MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MediaTabId(String);

impl MediaTabId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTabFilter {
    #[default]
    All,
    Images,
    Videos,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTabQueryMode {
    #[default]
    Name,
    Fuzzy,
    Semantic,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTabViewMode {
    #[default]
    LibraryViewer,
    FullGrid,
}

/// What a Media tab shows (WP-067).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTabKind {
    /// A filesystem folder. The historical and default behavior.
    #[default]
    Folder,
    /// A curated collection built from the metadata database: favorites and
    /// created color labels. No filesystem scan is involved.
    Collection,
}

/// Sub-view inside a collection tab (WP-067).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCollectionView {
    #[default]
    FavoriteVideos,
    FavoriteImages,
    Labels,
}

impl MediaCollectionView {
    pub fn label(self) -> &'static str {
        match self {
            Self::FavoriteVideos => "Fav videos",
            Self::FavoriteImages => "Fav images",
            Self::Labels => "Color labels",
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::FavoriteVideos, Self::FavoriteImages, Self::Labels]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTabSort {
    #[default]
    Name,
    Modified,
    Size,
    /// WP-068. Records written before this variant existed simply never carry
    /// it; `#[serde(default)]` on the viewport keeps them loadable.
    Created,
}

/// State that must follow a Media tab when another tab becomes active.
///
/// Paths are MediaDb canonical keys rather than display paths. Workspace-local
/// keys therefore survive relocation, while external/NAS keys retain their
/// absolute form under MediaDb's existing portability contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaTabViewport {
    /// WP-067: what this tab shows. `Folder` is the historical behavior and the
    /// default, so records written before this field existed load unchanged.
    pub kind: MediaTabKind,
    /// WP-067: which sub-view a collection tab is showing.
    pub collection_view: MediaCollectionView,
    /// WP-067: stable label ID selected in the labels sub-view. Stored by ID,
    /// never by visible name, so renaming a label cannot break the tab.
    pub collection_label_id: String,
    pub folder_key: String,
    /// Keyboard/controller focus inside the Library grid, independent of the
    /// Viewer selection and multi-selection set.
    pub cursor_key: Option<String>,
    pub selected_key: Option<String>,
    /// Multi-selection is retained while switching tabs and across restarts.
    /// The UI resolves keys against the freshly scanned inventory; missing
    /// assets are ignored rather than converted into stale numeric indices.
    pub selected_keys: Vec<String>,
    pub recursive: bool,
    /// WP-066: restrict search results to files directly inside the tab's
    /// folder, independent of `recursive`. The recursive inventory is kept, so
    /// toggling this never triggers a rescan.
    pub search_folder_only: bool,
    pub filter: MediaTabFilter,
    pub search_query: String,
    pub query_mode: MediaTabQueryMode,
    pub view_mode: MediaTabViewMode,
    pub split_ratio: f32,
    pub tile_edge: f32,
    pub show_names: bool,
    pub strip_height: f32,
    pub sort: MediaTabSort,
    pub sort_descending: bool,
    pub library_scroll_top: f32,
    pub folder_navigator_key: String,
    pub folder_location_input: String,
}

impl Default for MediaTabViewport {
    fn default() -> Self {
        Self {
            kind: MediaTabKind::Folder,
            collection_view: MediaCollectionView::FavoriteVideos,
            collection_label_id: String::new(),
            folder_key: String::new(),
            cursor_key: None,
            selected_key: None,
            selected_keys: Vec::new(),
            recursive: true,
            search_folder_only: false,
            filter: MediaTabFilter::All,
            search_query: String::new(),
            query_mode: MediaTabQueryMode::Name,
            view_mode: MediaTabViewMode::LibraryViewer,
            split_ratio: 0.62,
            tile_edge: 500.0,
            show_names: false,
            strip_height: 132.0,
            sort: MediaTabSort::Name,
            sort_descending: false,
            library_scroll_top: 0.0,
            folder_navigator_key: String::new(),
            folder_location_input: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaTab {
    pub id: MediaTabId,
    pub viewport: MediaTabViewport,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaTabsState {
    schema_version: u32,
    next_serial: u64,
    active_tab_id: MediaTabId,
    tabs: Vec<MediaTab>,
}

impl Default for MediaTabsState {
    fn default() -> Self {
        let id = MediaTabId("media-tab-000001".to_string());
        Self {
            schema_version: MEDIA_TABS_SCHEMA_VERSION,
            next_serial: 2,
            active_tab_id: id.clone(),
            tabs: vec![MediaTab {
                id,
                viewport: MediaTabViewport::default(),
            }],
        }
    }
}

impl MediaTabsState {
    pub fn tabs(&self) -> &[MediaTab] {
        &self.tabs
    }

    pub fn active_id(&self) -> &MediaTabId {
        &self.active_tab_id
    }

    pub fn active(&self) -> &MediaTab {
        // Construction and decoding enforce this invariant. State mutations in
        // this module preserve it, so callers never need an empty-tab branch.
        self.tabs
            .iter()
            .find(|tab| tab.id == self.active_tab_id)
            .expect("MediaTabsState invariant: active tab exists")
    }

    pub fn active_mut(&mut self) -> &mut MediaTab {
        let id = self.active_tab_id.clone();
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .expect("MediaTabsState invariant: active tab exists")
    }

    pub fn tab(&self, id: &MediaTabId) -> Option<&MediaTab> {
        self.tabs.iter().find(|tab| tab.id == *id)
    }

    pub fn id_by_str(&self, id: &str) -> Option<MediaTabId> {
        self.tabs
            .iter()
            .find(|tab| tab.id.as_str() == id)
            .map(|tab| tab.id.clone())
    }

    pub fn activate_by_str(&mut self, id: &str) -> Result<(), String> {
        let id = self
            .id_by_str(id)
            .ok_or_else(|| format!("unknown media tab id: {id}"))?;
        self.activate(&id)
    }

    pub fn close_by_str(&mut self, id: &str) -> Result<MediaTabId, String> {
        let id = self
            .id_by_str(id)
            .ok_or_else(|| format!("unknown media tab id: {id}"))?;
        self.close(&id)
    }

    /// Add and activate a tab for an exact MediaDb folder key. Duplicate
    /// folders are allowed because their queries/layouts/selections may differ.
    pub fn open_folder_in_new_tab(&mut self, folder_key: String) -> Result<MediaTabId, String> {
        if folder_key.trim().is_empty() {
            return Err("folder key is empty".to_string());
        }
        if self.tabs.len() >= MAX_TABS {
            return Err(format!("media tab limit reached ({MAX_TABS})"));
        }
        let id = self.allocate_id()?;
        let mut viewport = MediaTabViewport::default();
        viewport.folder_key = folder_key;
        self.tabs.push(MediaTab {
            id: id.clone(),
            viewport,
        });
        self.active_tab_id = id.clone();
        Ok(id)
    }

    /// Open, or focus, the favorites/labels collection tab (WP-067). Only one
    /// is useful, so an existing collection tab is reused rather than stacking
    /// duplicates against the tab cap.
    pub fn open_collection_tab(&mut self) -> Result<MediaTabId, String> {
        if let Some(existing) = self
            .tabs
            .iter()
            .find(|tab| tab.viewport.kind == MediaTabKind::Collection)
            .map(|tab| tab.id.clone())
        {
            self.active_tab_id = existing.clone();
            return Ok(existing);
        }
        if self.tabs.len() >= MAX_TABS {
            return Err(format!("media tab limit reached ({MAX_TABS})"));
        }
        let id = self.allocate_id()?;
        let viewport = MediaTabViewport {
            kind: MediaTabKind::Collection,
            ..MediaTabViewport::default()
        };
        self.tabs.push(MediaTab {
            id: id.clone(),
            viewport,
        });
        self.active_tab_id = id.clone();
        Ok(id)
    }

    pub fn activate(&mut self, id: &MediaTabId) -> Result<(), String> {
        if self.tabs.iter().any(|tab| tab.id == *id) {
            self.active_tab_id = id.clone();
            Ok(())
        } else {
            Err(format!("unknown media tab id: {}", id.as_str()))
        }
    }

    /// Close a tab. The final tab is reset instead of removed, preserving the
    /// invariant that Media always has one renderable viewport.
    pub fn close(&mut self, id: &MediaTabId) -> Result<MediaTabId, String> {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == *id) else {
            return Err(format!("unknown media tab id: {}", id.as_str()));
        };
        if self.tabs.len() == 1 {
            self.tabs[0].viewport = MediaTabViewport::default();
            self.active_tab_id = self.tabs[0].id.clone();
            return Ok(self.active_tab_id.clone());
        }
        let was_active = self.active_tab_id == *id;
        self.tabs.remove(index);
        if was_active {
            let replacement = index.min(self.tabs.len() - 1);
            self.active_tab_id = self.tabs[replacement].id.clone();
        }
        Ok(self.active_tab_id.clone())
    }

    pub fn move_tab(&mut self, id: &MediaTabId, target_index: usize) -> Result<(), String> {
        let Some(source_index) = self.tabs.iter().position(|tab| tab.id == *id) else {
            return Err(format!("unknown media tab id: {}", id.as_str()));
        };
        let tab = self.tabs.remove(source_index);
        let target_index = target_index.min(self.tabs.len());
        self.tabs.insert(target_index, tab);
        Ok(())
    }

    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        let encoded = serde_json::to_string(self).map_err(|error| error.to_string())?;
        if encoded.len() > MEDIA_TABS_MAX_ENCODED_BYTES {
            return Err(format!(
                "media tab state exceeds {} byte persistence limit",
                MEDIA_TABS_MAX_ENCODED_BYTES
            ));
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &str) -> Result<Self, String> {
        if encoded.len() > MEDIA_TABS_MAX_ENCODED_BYTES {
            return Err(format!(
                "media tab state exceeds {} byte persistence limit",
                MEDIA_TABS_MAX_ENCODED_BYTES
            ));
        }
        let mut state: Self = serde_json::from_str(encoded).map_err(|error| error.to_string())?;
        state.normalize_viewports();
        state.validate()?;
        Ok(state)
    }

    fn allocate_id(&mut self) -> Result<MediaTabId, String> {
        loop {
            let serial = self.next_serial;
            self.next_serial = self
                .next_serial
                .checked_add(1)
                .ok_or_else(|| "media tab id space exhausted".to_string())?;
            let id = MediaTabId(format!("media-tab-{serial:06}"));
            if !self.tabs.iter().any(|tab| tab.id == id) {
                return Ok(id);
            }
        }
    }

    fn normalize_viewports(&mut self) {
        for tab in &mut self.tabs {
            tab.viewport.split_ratio = tab.viewport.split_ratio.clamp(0.25, 0.80);
            tab.viewport.tile_edge = tab.viewport.tile_edge.clamp(64.0, 512.0);
            tab.viewport.strip_height = tab.viewport.strip_height.clamp(56.0, 400.0);
            tab.viewport.library_scroll_top = tab.viewport.library_scroll_top.max(0.0);
            tab.viewport.selected_keys.sort();
            tab.viewport.selected_keys.dedup();
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != MEDIA_TABS_SCHEMA_VERSION {
            return Err(format!(
                "unsupported media tab schema {}; expected {}",
                self.schema_version, MEDIA_TABS_SCHEMA_VERSION
            ));
        }
        if self.tabs.is_empty() {
            return Err("media tab state contains no tabs".to_string());
        }
        if self.tabs.len() > MAX_TABS {
            return Err(format!("media tab state exceeds {MAX_TABS} tabs"));
        }
        let mut ids = HashSet::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            if tab.id.as_str().trim().is_empty() {
                return Err("media tab id is empty".to_string());
            }
            if !ids.insert(tab.id.as_str()) {
                return Err(format!("duplicate media tab id: {}", tab.id.as_str()));
            }
        }
        if !self.tabs.iter().any(|tab| tab.id == self.active_tab_id) {
            return Err(format!(
                "active media tab does not exist: {}",
                self.active_tab_id.as_str()
            ));
        }
        Ok(())
    }
}

/// Operator-facing tab label. No filesystem access is performed.
pub fn folder_tab_title(resolved_folder: &str) -> String {
    let trimmed = resolved_folder.trim().trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return "Media".to_string();
    }
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WP-067: tab records written before the collection feature existed must
    /// keep loading, and must come back as folder tabs.
    #[test]
    fn tab_records_without_a_kind_load_as_folder_tabs() {
        let legacy = r#"{"schema_version":1,"next_serial":2,"active_tab_id":"media-tab-000001",
            "tabs":[{"id":"media-tab-000001","viewport":{"folder_key":"shoots/day-1"}}]}"#;
        let state = MediaTabsState::decode(legacy).expect("legacy record still decodes");
        assert_eq!(state.tabs().len(), 1);
        assert_eq!(state.active().viewport.kind, MediaTabKind::Folder);
        assert_eq!(state.active().viewport.folder_key, "shoots/day-1");
        // WP-068's new sort key must also fall back safely.
        assert_eq!(state.active().viewport.sort, MediaTabSort::Name);
        assert!(!state.active().viewport.search_folder_only);
    }

    /// WP-067: only one favourites tab is useful, so opening it twice focuses
    /// the existing one instead of stacking duplicates against the cap.
    #[test]
    fn collection_tab_is_reused_rather_than_duplicated() {
        let mut state = MediaTabsState::default();
        let first = state.open_collection_tab().unwrap();
        let second = state.open_collection_tab().unwrap();
        assert_eq!(first, second);
        assert_eq!(
            state
                .tabs()
                .iter()
                .filter(|tab| tab.viewport.kind == MediaTabKind::Collection)
                .count(),
            1
        );
        assert_eq!(state.active().viewport.kind, MediaTabKind::Collection);
        // A collection tab carries no folder, so it can never be scanned.
        assert!(state.active().viewport.folder_key.is_empty());
    }

    /// WP-067: the labels sub-view keys on the stable label ID, so renaming a
    /// label cannot orphan an open collection tab.
    #[test]
    fn collection_tab_round_trips_its_subview_and_label_id() {
        let mut state = MediaTabsState::default();
        state.open_collection_tab().unwrap();
        state.active_mut().viewport.collection_view = MediaCollectionView::Labels;
        state.active_mut().viewport.collection_label_id = "label-abc".to_string();
        let encoded = state.encode().unwrap();
        let restored = MediaTabsState::decode(&encoded).unwrap();
        assert_eq!(
            restored.active().viewport.collection_view,
            MediaCollectionView::Labels
        );
        assert_eq!(restored.active().viewport.collection_label_id, "label-abc");
        assert_eq!(restored.active().viewport.kind, MediaTabKind::Collection);
    }

    #[test]
    fn new_tabs_allow_same_folder_with_independent_viewports() {
        let mut state = MediaTabsState::default();
        let a = state
            .open_folder_in_new_tab("shoots/day-1".to_string())
            .unwrap();
        state.active_mut().viewport.search_query = "close-up".to_string();
        let b = state
            .open_folder_in_new_tab("shoots/day-1".to_string())
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(state.active().viewport.search_query, "");
        assert_eq!(state.tab(&a).unwrap().viewport.search_query, "close-up");
    }

    #[test]
    fn active_close_selects_adjacent_and_final_close_resets() {
        let mut state = MediaTabsState::default();
        let original = state.active_id().clone();
        let second = state.open_folder_in_new_tab("D:/two".to_string()).unwrap();
        let third = state
            .open_folder_in_new_tab("D:/three".to_string())
            .unwrap();
        assert_eq!(state.close(&third).unwrap(), second);
        assert_eq!(state.close(&second).unwrap(), original);
        state.active_mut().viewport.search_query = "temporary".to_string();
        assert_eq!(state.close(&original).unwrap(), original);
        assert_eq!(state.tabs().len(), 1);
        assert_eq!(state.active().viewport, MediaTabViewport::default());
    }

    #[test]
    fn codec_round_trips_and_clamps_untrusted_viewport_numbers() {
        let mut state = MediaTabsState::default();
        state.active_mut().viewport.split_ratio = 9.0;
        state.active_mut().viewport.tile_edge = 2.0;
        state.active_mut().viewport.library_scroll_top = -8.0;
        state.active_mut().viewport.cursor_key = Some("cursor.mp4".into());
        state.active_mut().viewport.selected_keys = vec!["b".into(), "a".into(), "a".into()];
        let decoded = MediaTabsState::decode(&state.encode().unwrap()).unwrap();
        let viewport = &decoded.active().viewport;
        assert_eq!(viewport.split_ratio, 0.80);
        assert_eq!(viewport.tile_edge, 64.0);
        assert_eq!(viewport.library_scroll_top, 0.0);
        assert_eq!(viewport.cursor_key.as_deref(), Some("cursor.mp4"));
        assert_eq!(viewport.selected_keys, vec!["a", "b"]);
    }

    #[test]
    fn codec_rejects_duplicate_ids_and_missing_active_tab() {
        let duplicate = r#"{
            "schema_version":1,
            "next_serial":3,
            "active_tab_id":"media-tab-000001",
            "tabs":[
                {"id":"media-tab-000001","viewport":{}},
                {"id":"media-tab-000001","viewport":{}}
            ]
        }"#;
        assert!(MediaTabsState::decode(duplicate)
            .unwrap_err()
            .contains("duplicate media tab id"));

        let missing = r#"{
            "schema_version":1,
            "next_serial":2,
            "active_tab_id":"media-tab-999999",
            "tabs":[{"id":"media-tab-000001","viewport":{}}]
        }"#;
        assert!(MediaTabsState::decode(missing)
            .unwrap_err()
            .contains("active media tab does not exist"));
    }

    #[test]
    fn folder_titles_are_lexical_and_do_not_probe_the_filesystem() {
        assert_eq!(folder_tab_title("D:/shoots/day-1/"), "day-1");
        assert_eq!(folder_tab_title(r"\\nas\media\set-a"), "set-a");
        assert_eq!(folder_tab_title(""), "Media");
    }
}
