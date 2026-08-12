//! Media metadata database (WP-042).
//!
//! redb-backed store for the media browser front surface: notes, tags, color
//! labels, favorites, and browser settings. Lives at
//! `<workspace_root>/.facial/media/media.redb`.
//!
//! Key contract (portability, GLOBAL-PORTABILITY): paths under the workspace
//! root are stored workspace-relative with `/` separators, so metadata survives
//! relocating the whole workspace folder. Paths outside the workspace are
//! stored absolute (slash-normalized). Keys are casefolded (lowercase) because
//! Windows filesystems are case-insensitive — the same file addressed with any
//! casing or separator style resolves to one row. `.`/`..` segments and the
//! Windows verbatim prefix (`\\?\`) are normalized away. Reads try the
//! normalized key first and fall back to the legacy absolute form so
//! pre-migration rows stay reachable.
//!
//! Concurrency: redb takes an EXCLUSIVE file lock for the lifetime of a
//! writer handle, and `ReadOnlyDatabase::open` needs a shared lock — so while
//! one process (e.g. the live GUI) holds the DB, a second process can neither
//! write NOR read. The second handle degrades to `Unavailable` with a clear
//! `status()` message; writes error, and command dispatch must surface reads
//! against an unavailable store as errors, never as ok-empty. The read-only
//! fallback only engages when the file itself is write-protected.

use redb::{Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NOTES: TableDefinition<&str, &str> = TableDefinition::new("notes");
const TAGS: TableDefinition<&str, &str> = TableDefinition::new("tags");
const LABELS: TableDefinition<&str, &str> = TableDefinition::new("color_labels");
/// Key = casefolded canonical key; value = original-case path for display.
const FAVORITES: TableDefinition<&str, &str> = TableDefinition::new("favorites");
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");
const INVENTORY_MANIFESTS: TableDefinition<&str, &str> =
    TableDefinition::new("inventory_manifests_v1");
const INVENTORY_ITEMS: TableDefinition<&str, &str> = TableDefinition::new("inventory_items_v1");
/// Namespace markers written before staged rows. On the next exclusive store
/// open, any marker left by a crashed process is reclaimed before scans start.
const INVENTORY_STAGING: TableDefinition<&str, &str> = TableDefinition::new("inventory_staging_v1");

/// Settings marker that makes the legacy-JSON migration one-shot even when
/// the archive rename fails or an old JSON reappears later.
const MIGRATED_MARKER: &str = "legacy_json_migrated";

/// Legacy built-in IDs retained by the WP-061 v2 catalog migration. The v2
/// catalog is arbitrary-length; these values are no longer the full vocabulary.
pub const COLOR_LABELS: [&str; 7] = ["red", "orange", "yellow", "green", "blue", "purple", "gray"];
const COLOR_LABEL_DEFINITIONS_KEY: &str = "color_label_definitions_v1";
const COLOR_LABEL_DEFINITIONS_V2_KEY: &str = "color_label_definitions_v2";
const COLOR_LABEL_SCHEMA_V2_MARKER: &str = "color_label_schema_v2_migrated";

/// Operator-editable presentation for one stable label ID. Asset rows store
/// only `id`, so changing a visible name or color never disconnects existing
/// assignments. `hex` is persisted for backend/API use but the GUI exposes it
/// through a native color picker rather than a text field.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ColorLabelDefinition {
    pub id: String,
    pub name: String,
    pub hex: String,
}

pub fn default_color_label_definitions() -> Vec<ColorLabelDefinition> {
    [
        ("red", "Red", "#C63E35"),
        ("orange", "Orange", "#E28A2C"),
        ("yellow", "Yellow", "#DEC33E"),
        ("green", "Green", "#54A65C"),
        ("blue", "Blue", "#4682C4"),
        ("purple", "Purple", "#945CC4"),
        ("gray", "Gray", "#808080"),
    ]
    .into_iter()
    .map(|(id, name, hex)| ColorLabelDefinition {
        id: id.to_string(),
        name: name.to_string(),
        hex: hex.to_string(),
    })
    .collect()
}

/// Parse `#RRGGBB` (or the same six digits without `#`) and return both its
/// canonical backend representation and RGB bytes.
pub fn normalize_hex_color(raw: &str) -> Option<(String, [u8; 3])> {
    let digits = raw.trim().strip_prefix('#').unwrap_or(raw.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let rgb = [
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ];
    Some((format!("#{value:06X}"), rgb))
}

pub fn validate_color_label_definitions(
    definitions: &[ColorLabelDefinition],
) -> Result<Vec<ColorLabelDefinition>, String> {
    let mut normalized = Vec::with_capacity(definitions.len());
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut colors = BTreeSet::new();
    for definition in definitions {
        let id = definition.id.trim();
        if id.is_empty()
            || id.chars().count() > 80
            || !id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            })
        {
            return Err(format!("invalid stable color label id: {}", definition.id));
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("duplicate color label id: {id}"));
        }
        let name = definition.name.trim();
        if name.is_empty() || name.chars().count() > 48 {
            return Err(format!(
                "color label {id} needs a name between 1 and 48 characters"
            ));
        }
        if !names.insert(name.to_lowercase()) {
            return Err(format!("duplicate color label name: {name}"));
        }
        let Some((hex, _)) = normalize_hex_color(&definition.hex) else {
            return Err(format!("invalid color label hex for {id}"));
        };
        if !colors.insert(hex.clone()) {
            return Err(format!("duplicate color label hex: {hex}"));
        }
        normalized.push(ColorLabelDefinition {
            id: id.to_string(),
            name: name.to_string(),
            hex,
        });
    }
    Ok(normalized)
}

/// Map arbitrary label input onto the fixed vocabulary.
/// Known labels pass through; empty clears; anything else becomes `gray`
/// (deterministic legacy mapping — WP-038 allowed free text).
pub fn normalize_label(raw: &str) -> Option<&'static str> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    COLOR_LABELS
        .iter()
        .find(|l| **l == t)
        .copied()
        .or(Some("gray"))
}

/// One combined metadata row for a media path.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MediaMeta {
    pub notes: String,
    pub tags: String,
    /// Ordered, deduplicated stable label IDs (WP-061).
    pub labels: Vec<String>,
    /// Backward-compatible first-label alias for older receipts/UI code.
    pub label: String,
    pub favorite: bool,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ColorLabelDeleteResult {
    pub id: String,
    pub usage_count: usize,
    pub assignments_removed: usize,
}

/// Last completely reconciled media set for one root/filter traversal.
///
/// Only zero-directory-error scans are allowed to replace this record. The
/// UI may therefore use it immediately while a slow/offline NAS root is being
/// checked in the background without interpreting an outage as deletion.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct MediaInventory {
    pub schema_version: u32,
    pub generation: u64,
    pub root_identity: String,
    pub root_display: String,
    pub recursive: bool,
    pub media_filter: String,
    pub completed_at: String,
    pub scan_elapsed_ms: u64,
    pub first_batch_ms: Option<u64>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct InventoryManifest {
    schema_version: u32,
    generation: u64,
    item_namespace: String,
    root_identity: String,
    root_display: String,
    recursive: bool,
    media_filter: String,
    completed_at: String,
    scan_elapsed_ms: u64,
    first_batch_ms: Option<u64>,
    item_count: usize,
}

/// Independently lockable redb store used by scanner workers. Keeping the
/// large inventory writer separate from metadata prevents a reconciliation
/// commit from holding the notes/settings writer used by the UI.
#[derive(Clone)]
pub struct MediaInventoryStore {
    db: Arc<Database>,
    session_id: Arc<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaInventoryCommit {
    pub generation: u64,
    superseded_namespace: Option<String>,
}

enum Handle {
    ReadWrite(Database),
    ReadOnly(ReadOnlyDatabase),
    Unavailable,
}

pub struct MediaDb {
    handle: Handle,
    inventory_store: Option<MediaInventoryStore>,
    inventory_status: Option<String>,
    workspace_root: PathBuf,
    /// Why the store is not writable (shown as a UI banner); None when healthy.
    status: Option<String>,
    /// Bumped on tag writes so the cached vocabulary refreshes lazily.
    tag_vocab_cache: std::cell::RefCell<Option<Vec<String>>>,
}

impl MediaDb {
    /// Directory holding media browser state for a workspace.
    pub fn media_state_dir(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".facial").join("media")
    }

    /// Database file path for a workspace.
    pub fn db_path(workspace_root: &Path) -> PathBuf {
        Self::media_state_dir(workspace_root).join("media.redb")
    }

    /// Legacy WP-038 JSON store path.
    pub fn legacy_json_path(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".facial").join("media_metadata.json")
    }

    /// Open (or create) the workspace media DB and run the one-shot JSON
    /// migration. Never panics: on failure the handle degrades (read-only or
    /// unavailable) and `status()` explains why.
    pub fn open(workspace_root: &Path) -> Self {
        let dir = Self::media_state_dir(workspace_root);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            return Self {
                handle: Handle::Unavailable,
                inventory_store: None,
                inventory_status: Some(format!(
                    "media inventory unavailable: cannot create {}: {err}",
                    dir.display()
                )),
                workspace_root: workspace_root.to_path_buf(),
                status: Some(format!(
                    "media db unavailable: cannot create {}: {err}",
                    dir.display()
                )),
                tag_vocab_cache: std::cell::RefCell::new(None),
            };
        }
        let inventory_path = dir.join("inventory.redb");
        let (inventory_store, inventory_status) = match Database::create(&inventory_path) {
            Ok(db) => {
                let store = MediaInventoryStore {
                    db: Arc::new(db),
                    session_id: Arc::new(uuid::Uuid::new_v4().simple().to_string()),
                };
                let maintenance = store.clone();
                let _ = std::thread::Builder::new()
                    .name("media-inventory-maintenance".to_string())
                    .spawn(move || maintenance.cleanup_abandoned_staging());
                (Some(store), None)
            }
            Err(error) => (
                None,
                Some(format!(
                    "media inventory unavailable at {}: {error}",
                    inventory_path.display()
                )),
            ),
        };
        let path = Self::db_path(workspace_root);
        match Database::create(&path) {
            Ok(db) => {
                let mut me = Self {
                    handle: Handle::ReadWrite(db),
                    inventory_store,
                    inventory_status,
                    workspace_root: workspace_root.to_path_buf(),
                    status: None,
                    tag_vocab_cache: std::cell::RefCell::new(None),
                };
                me.migrate_legacy_json();
                me.migrate_color_labels_v2();
                me
            }
            Err(create_err) => match ReadOnlyDatabase::open(&path) {
                Ok(db) => Self {
                    handle: Handle::ReadOnly(db),
                    inventory_store,
                    inventory_status,
                    workspace_root: workspace_root.to_path_buf(),
                    status: Some(
                        "media db is locked by another instance — metadata is read-only here"
                            .to_string(),
                    ),
                    tag_vocab_cache: std::cell::RefCell::new(None),
                },
                Err(_) => Self {
                    handle: Handle::Unavailable,
                    inventory_store,
                    inventory_status,
                    workspace_root: workspace_root.to_path_buf(),
                    status: Some(format!("media db unavailable: {create_err}")),
                    tag_vocab_cache: std::cell::RefCell::new(None),
                },
            },
        }
    }

    pub fn is_writable(&self) -> bool {
        matches!(self.handle, Handle::ReadWrite(_))
    }

    /// True when reads can return real data (ReadWrite or ReadOnly handle).
    /// `Unavailable` (e.g. another process holds the redb lock) must be
    /// surfaced as an error by callers — never as ok-with-empty.
    pub fn is_available(&self) -> bool {
        !matches!(self.handle, Handle::Unavailable)
    }

    /// Health/degradation banner text for the UI; None when fully writable.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Cloneable worker handle for the persistent last-good inventory.
    /// Failure is explicit so a caller cannot confuse an unreadable/locked
    /// cache with a healthy cache that simply has no generation yet.
    pub fn inventory_store(&self) -> Result<MediaInventoryStore, String> {
        self.inventory_store.clone().ok_or_else(|| {
            self.inventory_status
                .clone()
                .unwrap_or_else(|| "media inventory unavailable".to_string())
        })
    }

    // ------------------------------------------------------------------
    // Keys
    // ------------------------------------------------------------------

    /// Normalize a filesystem path into its storage key.
    /// Under the workspace root -> relative, `/`-separated. Else absolute,
    /// `/`-separated. Trailing separators trimmed. Empty input -> empty key.
    pub fn key_for(&self, path: &str) -> String {
        key_for_root(&self.workspace_root, path)
    }

    /// Resolve a storage key back to an absolute, native-separator path.
    pub fn path_for_key(&self, key: &str) -> String {
        if key.is_empty() {
            return String::new();
        }
        let is_abs = key.starts_with('/')
            || key.starts_with("//")
            || (key.len() >= 2 && key.as_bytes()[1] == b':');
        let joined = if is_abs {
            PathBuf::from(key)
        } else {
            self.workspace_root.join(key)
        };
        joined
            .to_string_lossy()
            .replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    /// Candidate keys for reading `path`: normalized (casefolded canonical)
    /// first, then the casefolded absolute form (legacy rows), deduped.
    fn read_keys(&self, path: &str) -> Vec<String> {
        let normalized = self.key_for(path);
        let absolute = slashify(path).to_lowercase();
        let mut keys = vec![normalized];
        if !keys.contains(&absolute) {
            keys.push(absolute);
        }
        keys
    }

    // ------------------------------------------------------------------
    // Generic table plumbing
    // ------------------------------------------------------------------

    fn read_str(&self, table: TableDefinition<&str, &str>, path: &str) -> Option<String> {
        fn get_first(
            t: &impl ReadableTable<&'static str, &'static str>,
            keys: &[String],
        ) -> Option<String> {
            keys.iter().find_map(|k| {
                t.get(k.as_str())
                    .ok()
                    .flatten()
                    .map(|v| v.value().to_string())
            })
        }
        let keys = self.read_keys(path);
        match &self.handle {
            Handle::ReadWrite(db) => {
                let txn = db.begin_read().ok()?;
                let t = txn.open_table(table).ok()?;
                get_first(&t, &keys)
            }
            Handle::ReadOnly(db) => {
                let txn = db.begin_read().ok()?;
                let t = txn.open_table(table).ok()?;
                get_first(&t, &keys)
            }
            Handle::Unavailable => None,
        }
    }

    fn write_str(
        &self,
        table: TableDefinition<&str, &str>,
        path: &str,
        value: Option<&str>,
    ) -> Result<(), String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let key = self.key_for(path);
        let legacy = slashify(path).to_lowercase();
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = txn.open_table(table).map_err(|e| e.to_string())?;
            // Clearing or rewriting always removes the legacy-keyed row so the
            // store converges onto normalized keys.
            if legacy != key {
                let _ = t.remove(legacy.as_str());
            }
            match value {
                Some(v) if !v.trim().is_empty() => {
                    t.insert(key.as_str(), v).map_err(|e| e.to_string())?;
                }
                _ => {
                    let _ = t.remove(key.as_str());
                }
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Write notes + tags + label for one path in a single transaction
    /// (atomic trio, one fsync). `None` leaves a field untouched; empty
    /// strings clear the row.
    pub fn set_meta(
        &self,
        path: &str,
        notes: Option<&str>,
        tags: Option<&str>,
        label: Option<&str>,
    ) -> Result<(), String> {
        let labels = label.map(|value| {
            self.resolve_color_label_id(value)
                .into_iter()
                .collect::<Vec<_>>()
        });
        self.set_meta_labels(path, notes, tags, labels.as_deref())
    }

    /// Write notes + tags + the ordered multi-label vector in one transaction.
    /// `None` leaves a field untouched; an empty label slice clears labels.
    pub fn set_meta_labels(
        &self,
        path: &str,
        notes: Option<&str>,
        tags: Option<&str>,
        labels: Option<&[String]>,
    ) -> Result<(), String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let key = self.key_for(path);
        let legacy = slashify(path).to_lowercase();
        if tags.is_some() {
            self.tag_vocab_cache.borrow_mut().take();
        }
        let normalized_labels = labels
            .map(|values| self.normalize_assigned_label_ids(values))
            .transpose()?;
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let write_field =
                |table: TableDefinition<&str, &str>, value: Option<String>| -> Result<(), String> {
                    let Some(value) = value else { return Ok(()) };
                    let mut t = txn.open_table(table).map_err(|e| e.to_string())?;
                    if legacy != key {
                        let _ = t.remove(legacy.as_str());
                    }
                    if value.trim().is_empty() {
                        let _ = t.remove(key.as_str());
                    } else {
                        t.insert(key.as_str(), value.as_str())
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(())
                };
            write_field(NOTES, notes.map(String::from))?;
            write_field(TAGS, tags.map(clean_tag_list))?;
            write_field(
                LABELS,
                normalized_labels
                    .as_ref()
                    .map(|values| encode_label_ids(values))
                    .transpose()?,
            )?;
        }
        txn.commit().map_err(|e| e.to_string())
    }

    fn table_rows(&self, table: TableDefinition<&str, &str>) -> BTreeMap<String, String> {
        fn collect(
            t: &impl ReadableTable<&'static str, &'static str>,
            out: &mut BTreeMap<String, String>,
        ) {
            if let Ok(iter) = t.iter() {
                for row in iter.flatten() {
                    out.insert(row.0.value().to_string(), row.1.value().to_string());
                }
            }
        }
        let mut out = BTreeMap::new();
        match &self.handle {
            Handle::ReadWrite(db) => {
                if let Ok(txn) = db.begin_read() {
                    if let Ok(t) = txn.open_table(table) {
                        collect(&t, &mut out);
                    }
                }
            }
            Handle::ReadOnly(db) => {
                if let Ok(txn) = db.begin_read() {
                    if let Ok(t) = txn.open_table(table) {
                        collect(&t, &mut out);
                    }
                }
            }
            Handle::Unavailable => {}
        }
        out
    }

    // ------------------------------------------------------------------
    // Notes / tags / labels
    // ------------------------------------------------------------------

    pub fn notes(&self, path: &str) -> Option<String> {
        self.read_str(NOTES, path)
    }

    pub fn set_notes(&self, path: &str, notes: &str) -> Result<(), String> {
        self.write_str(NOTES, path, Some(notes))
    }

    pub fn tags(&self, path: &str) -> Option<String> {
        self.read_str(TAGS, path)
    }

    /// Store a comma-separated tag list (trimmed, deduped, lowercased, sorted).
    pub fn set_tags(&self, path: &str, tags: &str) -> Result<(), String> {
        let cleaned = clean_tag_list(tags);
        self.tag_vocab_cache.borrow_mut().take();
        self.write_str(TAGS, path, Some(cleaned.as_str()))
    }

    pub fn label(&self, path: &str) -> Option<String> {
        self.labels(path).into_iter().next()
    }

    /// All stable label IDs assigned to one asset, in operator order.
    pub fn labels(&self, path: &str) -> Vec<String> {
        self.read_str(LABELS, path)
            .map(|raw| decode_label_ids(&raw))
            .unwrap_or_default()
    }

    /// Set (or clear, with empty input) the color label; input is normalized
    /// onto [`COLOR_LABELS`].
    pub fn set_label(&self, path: &str, label: &str) -> Result<(), String> {
        match self.resolve_color_label_id(label) {
            Some(label) => self.set_labels(path, &[label]).map(|_| ()),
            None => self.write_str(LABELS, path, None),
        }
    }

    /// Replace all assignments with an ordered, deduplicated label list.
    pub fn set_labels(&self, path: &str, labels: &[String]) -> Result<Vec<String>, String> {
        let labels = self.normalize_assigned_label_ids(labels)?;
        if labels.is_empty() {
            self.write_str(LABELS, path, None)?;
        } else {
            let encoded = encode_label_ids(&labels)?;
            self.write_str(LABELS, path, Some(&encoded))?;
        }
        Ok(labels)
    }

    pub fn add_label(&self, path: &str, label: &str) -> Result<Vec<String>, String> {
        let id = self
            .find_color_label_id(label)
            .ok_or_else(|| format!("unknown stable color-label id or name: {label}"))?;
        let mut labels = self.labels(path);
        if !labels.iter().any(|existing| existing == &id) {
            labels.push(id);
        }
        self.set_labels(path, &labels)
    }

    pub fn remove_label(&self, path: &str, label: &str) -> Result<Vec<String>, String> {
        let id = self
            .find_color_label_id(label)
            .ok_or_else(|| format!("unknown stable color-label id or name: {label}"))?;
        let mut labels = self.labels(path);
        labels.retain(|existing| existing != &id);
        self.set_labels(path, &labels)
    }

    pub fn clear_labels(&self, path: &str) -> Result<(), String> {
        self.write_str(LABELS, path, None)
    }

    /// Combined row for one path.
    pub fn meta(&self, path: &str) -> MediaMeta {
        let labels = self.labels(path);
        MediaMeta {
            notes: self.notes(path).unwrap_or_default(),
            tags: self.tags(path).unwrap_or_default(),
            label: labels.first().cloned().unwrap_or_default(),
            labels,
            favorite: self.is_favorite(path),
        }
    }

    /// All rows that carry any metadata, as `(canonical_key, meta)` pairs.
    /// Optional exact tag / label filters (tag matches list membership).
    pub fn list_meta_by_key(
        &self,
        tag: Option<&str>,
        label: Option<&str>,
    ) -> Vec<(String, MediaMeta)> {
        let notes = self.table_rows(NOTES);
        let tags = self.table_rows(TAGS);
        let labels = self.table_rows(LABELS);
        let favs: BTreeSet<String> = self
            .favorites_keyed()
            .into_iter()
            .map(|(key, _)| key)
            .collect();

        let mut keys: BTreeSet<String> = BTreeSet::new();
        keys.extend(notes.keys().cloned());
        keys.extend(tags.keys().cloned());
        keys.extend(labels.keys().cloned());
        keys.extend(favs.iter().cloned());

        let tag_filter = tag.map(|t| t.trim().to_ascii_lowercase());
        let label_filter = label.map(|l| l.trim().to_ascii_lowercase());
        let resolved_label_filter = label_filter.as_deref().map(|value| {
            self.find_color_label_id(value)
                .unwrap_or_else(|| value.to_string())
        });

        keys.into_iter()
            .filter_map(|key| {
                let label_ids = labels
                    .get(&key)
                    .map(|raw| decode_label_ids(raw))
                    .unwrap_or_default();
                let meta = MediaMeta {
                    notes: notes.get(&key).cloned().unwrap_or_default(),
                    tags: tags.get(&key).cloned().unwrap_or_default(),
                    label: label_ids.first().cloned().unwrap_or_default(),
                    labels: label_ids,
                    favorite: favs.contains(&key),
                };
                if let Some(t) = &tag_filter {
                    let has = meta
                        .tags
                        .split(',')
                        .map(|x| x.trim())
                        .any(|x| x.eq_ignore_ascii_case(t));
                    if !has {
                        return None;
                    }
                }
                if let Some(resolved) = &resolved_label_filter {
                    if !meta
                        .labels
                        .iter()
                        .any(|id| id.eq_ignore_ascii_case(resolved))
                    {
                        return None;
                    }
                }
                Some((key, meta))
            })
            .collect()
    }

    /// Like [`list_meta_by_key`] but with keys resolved to absolute paths
    /// (receipt-facing).
    pub fn list_meta(&self, tag: Option<&str>, label: Option<&str>) -> Vec<(String, MediaMeta)> {
        self.list_meta_by_key(tag, label)
            .into_iter()
            .map(|(key, meta)| (self.path_for_key(&key), meta))
            .collect()
    }

    /// Distinct tag vocabulary across all rows (sorted), cached until the next
    /// tag write. Feeds autocomplete cheaply.
    pub fn tag_vocab(&self) -> Vec<String> {
        if let Some(cached) = self.tag_vocab_cache.borrow().as_ref() {
            return cached.clone();
        }
        let mut vocab: BTreeSet<String> = BTreeSet::new();
        for tags in self.table_rows(TAGS).values() {
            for tag in tags.split(',') {
                let t = tag.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    vocab.insert(t);
                }
            }
        }
        let list: Vec<String> = vocab.into_iter().collect();
        *self.tag_vocab_cache.borrow_mut() = Some(list.clone());
        list
    }

    // ------------------------------------------------------------------
    // Favorites
    // ------------------------------------------------------------------

    pub fn is_favorite(&self, path: &str) -> bool {
        self.read_str(FAVORITES, path).is_some()
    }

    /// Add a favorite; the original-case path is kept as the display value.
    pub fn add_favorite(&self, path: &str) -> Result<(), String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let key = self.key_for(path);
        let legacy = slashify(path).to_lowercase();
        let display = slashify(path);
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = txn.open_table(FAVORITES).map_err(|e| e.to_string())?;
            if legacy != key {
                let _ = t.remove(legacy.as_str());
            }
            t.insert(key.as_str(), display.as_str())
                .map_err(|e| e.to_string())?;
        }
        txn.commit().map_err(|e| e.to_string())
    }

    pub fn remove_favorite(&self, path: &str) -> Result<(), String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = txn.open_table(FAVORITES).map_err(|e| e.to_string())?;
            for key in self.read_keys(path) {
                let _ = t.remove(key.as_str());
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    pub fn toggle_favorite(&self, path: &str) -> Result<bool, String> {
        if self.is_favorite(path) {
            self.remove_favorite(path).map(|_| false)
        } else {
            self.add_favorite(path).map(|_| true)
        }
    }

    /// Favorites as `(canonical_key, display_path)` pairs, sorted by key.
    /// Display paths keep original casing, native separators.
    pub fn favorites_keyed(&self) -> Vec<(String, String)> {
        self.table_rows(FAVORITES)
            .into_iter()
            .map(|(key, display)| {
                let native = display.replace('/', std::path::MAIN_SEPARATOR_STR);
                (key, native)
            })
            .collect()
    }

    /// Favorite entries as absolute, native-separator display paths.
    pub fn favorites(&self) -> Vec<String> {
        self.favorites_keyed().into_iter().map(|(_, p)| p).collect()
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        match &self.handle {
            Handle::ReadWrite(db) => {
                let txn = db.begin_read().ok()?;
                let t = txn.open_table(SETTINGS).ok()?;
                t.get(key).ok().flatten().map(|v| v.value().to_string())
            }
            Handle::ReadOnly(db) => {
                let txn = db.begin_read().ok()?;
                let t = txn.open_table(SETTINGS).ok()?;
                t.get(key).ok().flatten().map(|v| v.value().to_string())
            }
            Handle::Unavailable => None,
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = txn.open_table(SETTINGS).map_err(|e| e.to_string())?;
            if value.is_empty() {
                let _ = t.remove(key);
            } else {
                t.insert(key, value).map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Load the arbitrary-length v2 named/colorized catalog. Invalid persisted
    /// JSON is isolated to this preference and falls back to deterministic
    /// built-ins; asset rows retain stable IDs either way.
    pub fn color_label_definitions(&self) -> Vec<ColorLabelDefinition> {
        self.setting(COLOR_LABEL_DEFINITIONS_V2_KEY)
            .or_else(|| self.setting(COLOR_LABEL_DEFINITIONS_KEY))
            .and_then(|raw| serde_json::from_str::<Vec<ColorLabelDefinition>>(&raw).ok())
            .and_then(|definitions| validate_color_label_definitions(&definitions).ok())
            .unwrap_or_else(default_color_label_definitions)
    }

    pub fn set_color_label_definitions(
        &self,
        definitions: &[ColorLabelDefinition],
    ) -> Result<Vec<ColorLabelDefinition>, String> {
        let normalized = validate_color_label_definitions(definitions)?;
        let allowed: BTreeSet<&str> = normalized.iter().map(|item| item.id.as_str()).collect();
        let assigned: BTreeSet<String> = self
            .table_rows(LABELS)
            .values()
            .flat_map(|raw| decode_label_ids(raw))
            .collect();
        if let Some(orphan) = assigned.iter().find(|id| !allowed.contains(id.as_str())) {
            return Err(format!(
                "cannot remove assigned color label {orphan} through palette replacement; use delete_color_label"
            ));
        }
        let encoded = serde_json::to_string(&normalized).map_err(|error| error.to_string())?;
        self.set_setting(COLOR_LABEL_DEFINITIONS_V2_KEY, &encoded)?;
        Ok(normalized)
    }

    /// Strictly resolve a current stable ID or operator-visible name.
    pub fn find_color_label_id(&self, raw: &str) -> Option<String> {
        let value = raw.trim();
        self.color_label_definitions()
            .into_iter()
            .find(|definition| {
                definition.id.eq_ignore_ascii_case(value)
                    || definition.name.eq_ignore_ascii_case(value)
            })
            .map(|definition| definition.id)
    }

    /// Accept stable IDs and current visible names. Unknown legacy input keeps
    /// the old deterministic `gray` fallback; empty input clears the label.
    pub fn resolve_color_label_id(&self, raw: &str) -> Option<String> {
        let value = raw.trim();
        if value.is_empty() {
            return None;
        }
        self.find_color_label_id(value)
            .or_else(|| normalize_label(value).map(String::from))
    }

    fn normalize_assigned_label_ids(&self, labels: &[String]) -> Result<Vec<String>, String> {
        let definitions = self.color_label_definitions();
        let mut out = Vec::with_capacity(labels.len());
        let mut seen = BTreeSet::new();
        for raw in labels {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            let id = definitions
                .iter()
                .find(|definition| {
                    definition.id.eq_ignore_ascii_case(value)
                        || definition.name.eq_ignore_ascii_case(value)
                })
                .map(|definition| definition.id.clone())
                .ok_or_else(|| format!("unknown stable color-label id or name: {value}"))?;
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
        Ok(out)
    }

    pub fn create_color_label(
        &self,
        name: &str,
        hex: &str,
    ) -> Result<ColorLabelDefinition, String> {
        let mut definitions = self.color_label_definitions();
        let definition = next_color_label_definition(&definitions, name, hex)?;
        definitions.push(definition.clone());
        self.set_color_label_definitions(&definitions)?;
        Ok(definition)
    }

    /// Atomically create a reusable label and assign it to one asset.
    pub fn create_color_label_and_assign(
        &self,
        path: &str,
        name: &str,
        hex: &str,
    ) -> Result<ColorLabelDefinition, String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let mut definitions = self.color_label_definitions();
        let definition = next_color_label_definition(&definitions, name, hex)?;
        definitions.push(definition.clone());
        let definitions = validate_color_label_definitions(&definitions)?;
        let catalog_json =
            serde_json::to_string(&definitions).map_err(|error| error.to_string())?;
        let key = self.key_for(path);
        let legacy = slashify(path).to_lowercase();
        let mut assigned = self.labels(path);
        if !assigned.contains(&definition.id) {
            assigned.push(definition.id.clone());
        }
        let assignment_json = encode_label_ids(&assigned)?;
        let txn = db.begin_write().map_err(|error| error.to_string())?;
        {
            let mut settings = txn
                .open_table(SETTINGS)
                .map_err(|error| error.to_string())?;
            settings
                .insert(COLOR_LABEL_DEFINITIONS_V2_KEY, catalog_json.as_str())
                .map_err(|error| error.to_string())?;
        }
        {
            let mut labels = txn.open_table(LABELS).map_err(|error| error.to_string())?;
            if legacy != key {
                let _ = labels.remove(legacy.as_str());
            }
            labels
                .insert(key.as_str(), assignment_json.as_str())
                .map_err(|error| error.to_string())?;
        }
        txn.commit().map_err(|error| error.to_string())?;
        Ok(definition)
    }

    pub fn update_color_label(
        &self,
        id: &str,
        name: Option<&str>,
        hex: Option<&str>,
    ) -> Result<ColorLabelDefinition, String> {
        if name.is_none() && hex.is_none() {
            return Err("color label update requires a name and/or color".to_string());
        }
        let mut definitions = self.color_label_definitions();
        let definition = definitions
            .iter_mut()
            .find(|definition| definition.id == id)
            .ok_or_else(|| format!("unknown stable color-label id: {id}"))?;
        if let Some(name) = name {
            definition.name = name.to_string();
        }
        if let Some(hex) = hex {
            definition.hex = hex.to_string();
        }
        let normalized = validate_color_label_definitions(&definitions)?;
        let updated = normalized
            .iter()
            .find(|definition| definition.id == id)
            .cloned()
            .expect("validated catalog retains updated id");
        let encoded = serde_json::to_string(&normalized).map_err(|error| error.to_string())?;
        self.set_setting(COLOR_LABEL_DEFINITIONS_V2_KEY, &encoded)?;
        Ok(updated)
    }

    pub fn color_label_usage_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for raw in self.table_rows(LABELS).values() {
            for id in decode_label_ids(raw) {
                *counts.entry(id).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Remove a catalog definition and every assignment in one transaction.
    /// An in-use label requires `confirmed=true`; the returned usage count lets
    /// the UI present an exact confirmation before the destructive call.
    pub fn delete_color_label(
        &self,
        id: &str,
        confirmed: bool,
    ) -> Result<ColorLabelDeleteResult, String> {
        let Handle::ReadWrite(db) = &self.handle else {
            return Err(self
                .status
                .clone()
                .unwrap_or_else(|| "media db is not writable".to_string()));
        };
        let mut definitions = self.color_label_definitions();
        if !definitions.iter().any(|definition| definition.id == id) {
            return Err(format!("unknown stable color-label id: {id}"));
        }
        let usage_count = self
            .color_label_usage_counts()
            .get(id)
            .copied()
            .unwrap_or(0);
        if usage_count > 0 && !confirmed {
            return Err(format!(
                "color label {id} is assigned to {usage_count} asset(s); confirmation required"
            ));
        }
        definitions.retain(|definition| definition.id != id);
        let definitions = validate_color_label_definitions(&definitions)?;
        let catalog_json =
            serde_json::to_string(&definitions).map_err(|error| error.to_string())?;
        let txn = db.begin_write().map_err(|error| error.to_string())?;
        let mut assignments_removed = 0usize;
        {
            let mut labels = txn.open_table(LABELS).map_err(|error| error.to_string())?;
            let rows: Vec<(String, String)> = labels
                .iter()
                .map_err(|error| error.to_string())?
                .filter_map(|row| row.ok())
                .map(|(key, value)| (key.value().to_string(), value.value().to_string()))
                .collect();
            for (key, raw) in rows {
                let mut ids = decode_label_ids(&raw);
                let before = ids.len();
                ids.retain(|assigned| assigned != id);
                if ids.len() == before {
                    continue;
                }
                assignments_removed += 1;
                if ids.is_empty() {
                    let _ = labels.remove(key.as_str());
                } else {
                    let encoded = encode_label_ids(&ids)?;
                    labels
                        .insert(key.as_str(), encoded.as_str())
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        {
            let mut settings = txn
                .open_table(SETTINGS)
                .map_err(|error| error.to_string())?;
            settings
                .insert(COLOR_LABEL_DEFINITIONS_V2_KEY, catalog_json.as_str())
                .map_err(|error| error.to_string())?;
        }
        txn.commit().map_err(|error| error.to_string())?;
        Ok(ColorLabelDeleteResult {
            id: id.to_string(),
            usage_count,
            assignments_removed,
        })
    }

    // ------------------------------------------------------------------
    // Last-good media inventory (WP-055)
    // ------------------------------------------------------------------

    /// Read the last completely committed generation for this exact
    /// root/traversal/filter. Mapped-drive and UNC aliases share a row only
    /// when Windows itself proves the mapping through WNet.
    pub fn media_inventory(
        &self,
        root: &Path,
        recursive: bool,
        media_filter: &str,
    ) -> Result<Option<MediaInventory>, String> {
        self.inventory_store()?.load(root, recursive, media_filter)
    }

    // ------------------------------------------------------------------
    // Legacy migration (WP-038 JSON -> redb), one shot
    // ------------------------------------------------------------------

    /// One-shot WP-061 migration. Catalog definitions move to the arbitrary
    /// v2 key and every legacy singular assignment becomes a JSON ID array.
    /// Catalog + assignments + marker commit atomically.
    fn migrate_color_labels_v2(&mut self) {
        if self.setting(COLOR_LABEL_SCHEMA_V2_MARKER).as_deref() == Some("1") {
            return;
        }
        let Handle::ReadWrite(db) = &self.handle else {
            return;
        };
        let definitions = self
            .setting(COLOR_LABEL_DEFINITIONS_V2_KEY)
            .or_else(|| self.setting(COLOR_LABEL_DEFINITIONS_KEY))
            .and_then(|raw| serde_json::from_str::<Vec<ColorLabelDefinition>>(&raw).ok())
            .and_then(|definitions| validate_color_label_definitions(&definitions).ok())
            .unwrap_or_else(default_color_label_definitions);
        let catalog_json = match serde_json::to_string(&definitions) {
            Ok(value) => value,
            Err(error) => {
                self.status = Some(format!("color-label v2 migration failed: {error}"));
                return;
            }
        };
        let migrate = || -> Result<(), String> {
            let txn = db.begin_write().map_err(|error| error.to_string())?;
            {
                let mut labels = txn.open_table(LABELS).map_err(|error| error.to_string())?;
                let rows: Vec<(String, String)> = labels
                    .iter()
                    .map_err(|error| error.to_string())?
                    .filter_map(|row| row.ok())
                    .map(|(key, value)| (key.value().to_string(), value.value().to_string()))
                    .collect();
                for (key, raw) in rows {
                    let mut ids = decode_label_ids(&raw);
                    let mut seen = BTreeSet::new();
                    ids = ids
                        .into_iter()
                        .filter_map(|value| {
                            definitions
                                .iter()
                                .find(|definition| {
                                    definition.id.eq_ignore_ascii_case(&value)
                                        || definition.name.eq_ignore_ascii_case(&value)
                                })
                                .map(|definition| definition.id.clone())
                                .or_else(|| normalize_label(&value).map(String::from))
                        })
                        .filter(|id| seen.insert(id.clone()))
                        .collect();
                    if ids.is_empty() {
                        let _ = labels.remove(key.as_str());
                    } else {
                        let encoded = encode_label_ids(&ids)?;
                        labels
                            .insert(key.as_str(), encoded.as_str())
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            {
                let mut settings = txn
                    .open_table(SETTINGS)
                    .map_err(|error| error.to_string())?;
                settings
                    .insert(COLOR_LABEL_DEFINITIONS_V2_KEY, catalog_json.as_str())
                    .map_err(|error| error.to_string())?;
                settings
                    .insert(COLOR_LABEL_SCHEMA_V2_MARKER, "1")
                    .map_err(|error| error.to_string())?;
            }
            txn.commit().map_err(|error| error.to_string())
        };
        if let Err(error) = migrate() {
            self.status = Some(format!("color-label v2 migration failed: {error}"));
        }
    }

    fn migrate_legacy_json(&mut self) {
        let json_path = Self::legacy_json_path(&self.workspace_root);
        let Ok(raw) = std::fs::read_to_string(&json_path) else {
            return; // nothing to migrate
        };
        // One-shot guard: a marker row (written in the migration txn) makes
        // this idempotent even if the archive rename failed or an old JSON
        // reappears later — re-importing would clobber newer edits.
        if self.setting(MIGRATED_MARKER).is_some() {
            self.status = Some(format!(
                "legacy media_metadata.json present but already migrated — left untouched: {}",
                json_path.display()
            ));
            return;
        }
        #[derive(serde::Deserialize)]
        struct Legacy {
            #[serde(default)]
            notes: BTreeMap<String, String>,
            #[serde(default)]
            tags: BTreeMap<String, String>,
            #[serde(default)]
            color_labels: BTreeMap<String, String>,
            #[serde(default)]
            favorites: Vec<String>,
        }
        let Ok(legacy) = serde_json::from_str::<Legacy>(&raw) else {
            self.status = Some(format!(
                "legacy media_metadata.json is unreadable and was left in place: {}",
                json_path.display()
            ));
            return;
        };
        let Handle::ReadWrite(db) = &self.handle else {
            return;
        };
        let migrate = || -> Result<(), String> {
            let txn = db.begin_write().map_err(|e| e.to_string())?;
            {
                let mut notes = txn.open_table(NOTES).map_err(|e| e.to_string())?;
                for (path, value) in &legacy.notes {
                    if !value.trim().is_empty() {
                        let key = key_for_root(&self.workspace_root, path);
                        notes
                            .insert(key.as_str(), value.as_str())
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut tags = txn.open_table(TAGS).map_err(|e| e.to_string())?;
                for (path, value) in &legacy.tags {
                    let cleaned = clean_tag_list(value);
                    if !cleaned.is_empty() {
                        let key = key_for_root(&self.workspace_root, path);
                        tags.insert(key.as_str(), cleaned.as_str())
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut labels = txn.open_table(LABELS).map_err(|e| e.to_string())?;
                for (path, value) in &legacy.color_labels {
                    if let Some(label) = normalize_label(value) {
                        let key = key_for_root(&self.workspace_root, path);
                        labels
                            .insert(key.as_str(), label)
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut favs = txn.open_table(FAVORITES).map_err(|e| e.to_string())?;
                for path in &legacy.favorites {
                    let key = key_for_root(&self.workspace_root, path);
                    let display = slashify(path);
                    favs.insert(key.as_str(), display.as_str())
                        .map_err(|e| e.to_string())?;
                }
                let mut settings = txn.open_table(SETTINGS).map_err(|e| e.to_string())?;
                settings
                    .insert(MIGRATED_MARKER, chrono::Utc::now().to_rfc3339().as_str())
                    .map_err(|e| e.to_string())?;
            }
            txn.commit().map_err(|e| e.to_string())
        };
        match migrate() {
            Ok(()) => {
                let archived = json_path.with_extension("json.migrated");
                if let Err(err) = std::fs::rename(&json_path, &archived) {
                    self.status = Some(format!(
                        "migrated media_metadata.json but could not archive it: {err}"
                    ));
                }
            }
            Err(err) => {
                self.status = Some(format!("media metadata migration failed: {err}"));
            }
        }
    }
}

impl MediaInventoryStore {
    /// Read one manifest and its immutable item namespace. A count mismatch is
    /// treated as no cache rather than exposing a partial/corrupt generation.
    pub fn load(
        &self,
        root: &Path,
        recursive: bool,
        media_filter: &str,
    ) -> Result<Option<MediaInventory>, String> {
        let root_identity = stable_media_root_identity(root);
        self.load_with_identity(root, &root_identity, recursive, media_filter)
    }

    /// Load with a root identity already resolved by the configured-root scan
    /// generation. This keeps mapped-drive WNet proof to one call per
    /// reconciliation instead of repeating it for inventory and thumbnails.
    pub fn load_with_identity(
        &self,
        root: &Path,
        root_identity: &str,
        recursive: bool,
        media_filter: &str,
    ) -> Result<Option<MediaInventory>, String> {
        let normalized_filter = media_filter.trim().to_ascii_lowercase();
        let key = inventory_key(root_identity, recursive, &normalized_filter);
        let txn = self
            .db
            .begin_read()
            .map_err(|error| format!("inventory read transaction failed: {error}"))?;
        let manifests = txn
            .open_table(INVENTORY_MANIFESTS)
            .map_err(|error| format!("inventory manifest table failed: {error}"))?;
        let Some(raw) = manifests
            .get(key.as_str())
            .map_err(|error| format!("inventory manifest read failed: {error}"))?
        else {
            return Ok(None);
        };
        let manifest: InventoryManifest = serde_json::from_str(raw.value())
            .map_err(|error| format!("inventory manifest is invalid: {error}"))?;
        if manifest.schema_version != 1
            || manifest.root_identity != root_identity
            || manifest.recursive != recursive
            || manifest.media_filter != normalized_filter
        {
            return Ok(None);
        }
        drop(raw);
        let items = txn
            .open_table(INVENTORY_ITEMS)
            .map_err(|error| format!("inventory item table failed: {error}"))?;
        let prefix = inventory_item_prefix(&manifest.item_namespace);
        let end = format!("{prefix}~");
        let mut files = Vec::with_capacity(manifest.item_count);
        let rows = items
            .range(prefix.as_str()..end.as_str())
            .map_err(|error| format!("inventory item range failed: {error}"))?;
        for row in rows {
            let (_, value) = row.map_err(|error| format!("inventory item read failed: {error}"))?;
            files.push(value.value().to_string());
        }
        if files.len() != manifest.item_count {
            return Err(format!(
                "inventory item count mismatch: manifest={}, rows={}",
                manifest.item_count,
                files.len()
            ));
        }
        let requested_root_display = root.to_string_lossy().to_string();
        if manifest.root_display != requested_root_display {
            // The manifest key may deliberately be shared by an OS-proven
            // mapped-drive/UNC alias. Rows are persisted in the producer's
            // spelling, so rebase them to the spelling the caller can
            // currently access before exposing the last-good inventory.
            files = rebase_inventory_files(&files, Path::new(&manifest.root_display), root)
                .ok_or_else(|| {
                    format!(
                        "inventory rows escape stored root {}",
                        manifest.root_display
                    )
                })?;
        }
        Ok(Some(MediaInventory {
            schema_version: manifest.schema_version,
            generation: manifest.generation,
            root_identity: manifest.root_identity,
            root_display: requested_root_display,
            recursive: manifest.recursive,
            media_filter: manifest.media_filter,
            completed_at: manifest.completed_at,
            scan_elapsed_ms: manifest.scan_elapsed_ms,
            first_batch_ms: manifest.first_batch_ms,
            files,
        }))
    }

    /// Stage immutable rows in bounded write transactions, then atomically
    /// swap the small manifest. Until that final commit succeeds readers keep
    /// seeing the prior generation; staging never holds the UI metadata DB.
    pub fn replace(
        &self,
        root: &Path,
        recursive: bool,
        media_filter: &str,
        files: &[String],
        scan_elapsed_ms: u64,
        first_batch_ms: Option<u64>,
    ) -> Result<MediaInventoryCommit, String> {
        self.replace_cancellable(
            root,
            recursive,
            media_filter,
            files,
            scan_elapsed_ms,
            first_batch_ms,
            || false,
        )?
        .ok_or_else(|| "inventory replacement cancelled".to_string())
    }

    /// Cancellable variant used by scan workers. Cancellation is checked
    /// between every bounded stage transaction and again after preparing but
    /// immediately before committing the manifest swap.
    pub fn replace_cancellable(
        &self,
        root: &Path,
        recursive: bool,
        media_filter: &str,
        files: &[String],
        scan_elapsed_ms: u64,
        first_batch_ms: Option<u64>,
        is_cancelled: impl FnMut() -> bool,
    ) -> Result<Option<MediaInventoryCommit>, String> {
        let root_identity = stable_media_root_identity(root);
        self.replace_cancellable_with_identity(
            root,
            &root_identity,
            recursive,
            media_filter,
            files,
            scan_elapsed_ms,
            first_batch_ms,
            is_cancelled,
        )
    }

    /// Cancellable replacement using the configured-root identity resolved by
    /// the caller once for this scan generation.
    pub fn replace_cancellable_with_identity(
        &self,
        root: &Path,
        root_identity: &str,
        recursive: bool,
        media_filter: &str,
        files: &[String],
        scan_elapsed_ms: u64,
        first_batch_ms: Option<u64>,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<Option<MediaInventoryCommit>, String> {
        const STAGE_BATCH: usize = 2_048;
        let root_display = root.to_string_lossy().to_string();
        let media_filter = media_filter.trim().to_ascii_lowercase();
        let manifest_key = inventory_key(root_identity, recursive, &media_filter);
        let item_namespace = uuid::Uuid::new_v4().simple().to_string();
        let prefix = inventory_item_prefix(&item_namespace);

        let result = (|| -> Result<Option<MediaInventoryCommit>, String> {
            if is_cancelled() {
                return Ok(None);
            }
            let marker_txn = self.db.begin_write().map_err(|error| error.to_string())?;
            {
                let mut staging = marker_txn
                    .open_table(INVENTORY_STAGING)
                    .map_err(|error| error.to_string())?;
                staging
                    .insert(item_namespace.as_str(), self.session_id.as_str())
                    .map_err(|error| error.to_string())?;
            }
            marker_txn.commit().map_err(|error| error.to_string())?;

            for (chunk_index, chunk) in files.chunks(STAGE_BATCH).enumerate() {
                if is_cancelled() {
                    return Ok(None);
                }
                let txn = self.db.begin_write().map_err(|error| error.to_string())?;
                {
                    let mut items = txn
                        .open_table(INVENTORY_ITEMS)
                        .map_err(|error| error.to_string())?;
                    let base = chunk_index * STAGE_BATCH;
                    for (offset, path) in chunk.iter().enumerate() {
                        let item_key = format!("{prefix}{:016x}", base + offset);
                        items
                            .insert(item_key.as_str(), path.as_str())
                            .map_err(|error| error.to_string())?;
                    }
                }
                txn.commit().map_err(|error| error.to_string())?;
            }
            if is_cancelled() {
                return Ok(None);
            }

            let txn = self.db.begin_write().map_err(|error| error.to_string())?;
            let (generation, superseded_namespace);
            {
                let mut manifests = txn
                    .open_table(INVENTORY_MANIFESTS)
                    .map_err(|error| error.to_string())?;
                let previous = manifests
                    .get(manifest_key.as_str())
                    .ok()
                    .flatten()
                    .and_then(|value| {
                        serde_json::from_str::<InventoryManifest>(value.value()).ok()
                    });
                generation = previous
                    .as_ref()
                    .map(|manifest| manifest.generation)
                    .unwrap_or(0)
                    .saturating_add(1);
                superseded_namespace = previous.map(|manifest| manifest.item_namespace);
                let manifest = InventoryManifest {
                    schema_version: 1,
                    generation,
                    item_namespace: item_namespace.clone(),
                    root_identity: root_identity.to_string(),
                    root_display,
                    recursive,
                    media_filter,
                    completed_at: chrono::Utc::now().to_rfc3339(),
                    scan_elapsed_ms,
                    first_batch_ms,
                    item_count: files.len(),
                };
                let encoded =
                    serde_json::to_string(&manifest).map_err(|error| error.to_string())?;
                manifests
                    .insert(manifest_key.as_str(), encoded.as_str())
                    .map_err(|error| error.to_string())?;
            }
            {
                let mut staging = txn
                    .open_table(INVENTORY_STAGING)
                    .map_err(|error| error.to_string())?;
                let _ = staging.remove(item_namespace.as_str());
            }
            // This is the last cancellable point: dropping this transaction
            // leaves the prior manifest authoritative and the stage reclaimable.
            if is_cancelled() {
                drop(txn);
                return Ok(None);
            }
            txn.commit().map_err(|error| error.to_string())?;
            Ok(Some(MediaInventoryCommit {
                generation,
                superseded_namespace,
            }))
        })();

        if !matches!(result, Ok(Some(_))) {
            self.delete_namespace(&item_namespace, true);
        }
        result
    }

    /// Best-effort bounded cleanup after the new manifest is already visible.
    /// Failure leaves only unreachable cache rows and never affects last-good.
    pub fn cleanup_superseded(&self, commit: &MediaInventoryCommit) {
        let Some(namespace) = commit.superseded_namespace.as_deref() else {
            return;
        };
        self.delete_namespace(namespace, false);
    }

    fn cleanup_abandoned_staging(&self) {
        let namespaces: Vec<String> = {
            let Ok(txn) = self.db.begin_read() else {
                return;
            };
            let Ok(staging) = txn.open_table(INVENTORY_STAGING) else {
                return;
            };
            let Ok(rows) = staging.iter() else {
                return;
            };
            rows.filter_map(|row| {
                row.ok().and_then(|(key, session)| {
                    (session.value() != self.session_id.as_str()).then(|| key.value().to_string())
                })
            })
            .collect()
        };
        for namespace in namespaces {
            self.delete_namespace(&namespace, true);
        }
    }

    fn delete_namespace(&self, namespace: &str, remove_staging_marker: bool) {
        const DELETE_BATCH: usize = 4_096;
        let prefix = inventory_item_prefix(namespace);
        let end = format!("{prefix}~");
        loop {
            let keys: Vec<String> = {
                let Ok(txn) = self.db.begin_read() else {
                    return;
                };
                let Ok(items) = txn.open_table(INVENTORY_ITEMS) else {
                    return;
                };
                let Ok(rows) = items.range(prefix.as_str()..end.as_str()) else {
                    return;
                };
                rows.take(DELETE_BATCH)
                    .filter_map(|row| row.ok().map(|(key, _)| key.value().to_string()))
                    .collect()
            };
            if keys.is_empty() {
                break;
            }
            let Ok(txn) = self.db.begin_write() else {
                return;
            };
            {
                let Ok(mut items) = txn.open_table(INVENTORY_ITEMS) else {
                    return;
                };
                for key in &keys {
                    let _ = items.remove(key.as_str());
                }
            }
            if txn.commit().is_err() {
                return;
            }
        }
        if remove_staging_marker {
            let Ok(txn) = self.db.begin_write() else {
                return;
            };
            {
                let Ok(mut staging) = txn.open_table(INVENTORY_STAGING) else {
                    return;
                };
                let _ = staging.remove(namespace);
            }
            let _ = txn.commit();
        }
    }
}

fn rebase_inventory_files(
    files: &[String],
    stored_root: &Path,
    requested_root: &Path,
) -> Option<Vec<String>> {
    let stored = slashify(stored_root.to_string_lossy().as_ref());
    let stored = stored.trim_end_matches('/');
    let stored_key = if cfg!(windows) {
        stored.to_ascii_lowercase()
    } else {
        stored.to_string()
    };
    files
        .iter()
        .map(|file| {
            let normalized = slashify(file);
            let normalized_key = if cfg!(windows) {
                normalized.to_ascii_lowercase()
            } else {
                normalized.clone()
            };
            let relative = if normalized_key == stored_key {
                ""
            } else {
                let prefix_len = stored_key.len();
                if !normalized_key.starts_with(&stored_key)
                    || normalized_key.as_bytes().get(prefix_len) != Some(&b'/')
                {
                    return None;
                }
                &normalized[prefix_len + 1..]
            };
            Some(
                requested_root
                    .join(Path::new(relative))
                    .to_string_lossy()
                    .to_string(),
            )
        })
        .collect()
}

/// Normalize a path string: strip Windows verbatim prefixes (`\\?\`,
/// `\\.\`, `\\?\UNC\`), `\` -> `/`, resolve `.`/`..` segments lexically,
/// trim trailing slashes (except bare drive roots). UNC roots keep their
/// doubled leading slash (`\\server\share` -> `//server/share`).
fn slashify(path: &str) -> String {
    let mut s = path.trim().replace('\\', "/");
    for prefix in ["//?/", "//./"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            // Verbatim UNC form: `\\?\UNC\server\share` -> `//server/share`.
            s = match rest.strip_prefix("UNC/") {
                Some(unc_rest) => format!("//{unc_rest}"),
                None => rest.to_string(),
            };
        }
    }
    let is_unc = s.starts_with("//");
    // Lexical `.` / `..` resolution (never touches the filesystem).
    let mut parts: Vec<&str> = Vec::new();
    let has_drive = s.len() >= 2 && s.as_bytes()[1] == b':';
    let leading_slash = s.starts_with('/');
    for segment in s.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Pop unless we'd pop past a drive/UNC anchor.
                let anchored = (parts.len() == 1 && has_drive) || (is_unc && parts.len() <= 2);
                match parts.last() {
                    Some(last) if *last != ".." && !anchored => {
                        parts.pop();
                    }
                    _ => parts.push(".."),
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    let mut out = if is_unc {
        format!("//{joined}")
    } else if leading_slash {
        format!("/{joined}")
    } else {
        joined
    };
    // Bare drive letter keeps its root slash ("D:" -> "D:/").
    if out.len() == 2 && out.as_bytes()[1] == b':' {
        out.push('/');
    }
    out
}

/// Public canonical-key helper for workers that must key rows WITHOUT
/// opening the (single-writer) media DB — e.g. the CLIP embedding indexer.
pub fn canonical_key(workspace_root: &Path, path: &str) -> String {
    key_for_root(workspace_root, path)
}

/// Stable identity for a configured media root. On Windows, an assigned drive
/// is converted to UNC only when the operating system's network provider
/// confirms that exact mapping. Otherwise the normalized input is retained;
/// hostname/IP guesses never merge unrelated shares.
pub fn stable_media_root_identity(root: &Path) -> String {
    stable_media_path_identity(&root.to_string_lossy())
}

/// Non-blocking lexical identity for render-time scheduling. Unlike the stable
/// cache identity, this performs no mapped-drive provider query; workers may
/// resolve a proven UNC alias after leaving the UI thread.
pub fn lexical_media_root_identity(root: &Path) -> String {
    slashify(&root.to_string_lossy()).to_lowercase()
}

/// Stable cache identity for a media path. This is public so the thumbnail
/// cache can share artifacts between a proven mapped-drive path and its UNC
/// spelling without changing the real display/open path.
pub fn stable_media_path_identity(path: &str) -> String {
    #[cfg(windows)]
    if let Some(universal) = windows_mapped_path_to_unc(path) {
        return slashify(&universal).to_lowercase();
    }
    slashify(path).to_lowercase()
}

fn inventory_key(root_identity: &str, recursive: bool, media_filter: &str) -> String {
    // JSON avoids delimiter ambiguity for UNC/share names and remains stable.
    serde_json::to_string(&(
        1u8,
        root_identity,
        recursive,
        media_filter.trim().to_ascii_lowercase(),
    ))
    .unwrap_or_else(|_| format!("v1:{recursive}:{media_filter}:{root_identity}"))
}

fn inventory_item_prefix(namespace: &str) -> String {
    format!("{namespace}:")
}

#[cfg(windows)]
fn windows_mapped_path_to_unc(path: &str) -> Option<String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct UniversalNameInfoW {
        universal_name: *mut u16,
    }

    #[link(name = "mpr")]
    extern "system" {
        fn WNetGetUniversalNameW(
            local_path: *const u16,
            info_level: u32,
            buffer: *mut c_void,
            buffer_size: *mut u32,
        ) -> u32;
    }

    const UNIVERSAL_NAME_INFO_LEVEL: u32 = 1;
    const ERROR_MORE_DATA: u32 = 234;

    let normalized = slashify(path);
    let bytes = normalized.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    // Resolve only the assigned root, then append the lexical suffix. This
    // proves alias identity without requiring the media file itself to exist.
    let drive_root = format!("{}:\\", (bytes[0] as char).to_ascii_uppercase());
    let wide: Vec<u16> = std::ffi::OsStr::new(&drive_root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buffer_size = 0u32;
    // SAFETY: the input is NUL-terminated and the null first-pass buffer is
    // the documented size-query pattern for WNetGetUniversalNameW.
    let first = unsafe {
        WNetGetUniversalNameW(
            wide.as_ptr(),
            UNIVERSAL_NAME_INFO_LEVEL,
            std::ptr::null_mut(),
            &mut buffer_size,
        )
    };
    if first != ERROR_MORE_DATA || buffer_size < std::mem::size_of::<UniversalNameInfoW>() as u32 {
        return None;
    }
    let word_count = (buffer_size as usize).div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; word_count];
    // SAFETY: `storage` is pointer-aligned and at least `buffer_size` bytes;
    // WNet writes a UNIVERSAL_NAME_INFOW plus its UTF-16 string into it.
    let result = unsafe {
        WNetGetUniversalNameW(
            wide.as_ptr(),
            UNIVERSAL_NAME_INFO_LEVEL,
            storage.as_mut_ptr().cast(),
            &mut buffer_size,
        )
    };
    if result != 0 {
        return None;
    }
    let info = unsafe { &*(storage.as_ptr().cast::<UniversalNameInfoW>()) };
    if info.universal_name.is_null() {
        return None;
    }
    let begin = storage.as_ptr() as usize;
    let end = begin.saturating_add(storage.len() * std::mem::size_of::<usize>());
    let string_start = info.universal_name as usize;
    if string_start < begin || string_start >= end {
        return None;
    }
    let max_units = (end - string_start) / std::mem::size_of::<u16>();
    let units = unsafe { std::slice::from_raw_parts(info.universal_name, max_units) };
    let length = units.iter().position(|unit| *unit == 0)?;
    let mapped_root = String::from_utf16(&units[..length]).ok()?;
    let suffix = normalized[2..].trim_start_matches('/');
    if suffix.is_empty() {
        Some(mapped_root)
    } else {
        Some(format!(
            "{}/{}",
            mapped_root.trim_end_matches(['\\', '/']),
            suffix
        ))
    }
}

/// Casefolded canonical storage key: workspace-relative when under the root,
/// absolute otherwise; always lowercase (Windows case-insensitive filesystems).
fn key_for_root(workspace_root: &Path, path: &str) -> String {
    let abs = slashify(path);
    if abs.is_empty() {
        return abs;
    }
    let root = slashify(&workspace_root.to_string_lossy());
    if root.is_empty() {
        return abs.to_lowercase();
    }
    // Drive roots already end in '/'; everything else gains exactly one.
    let root_prefix = format!("{}/", root.trim_end_matches('/'));
    if abs
        .get(..root_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&root_prefix))
    {
        // `get` above proves this byte offset is a UTF-8 character boundary.
        abs[root_prefix.len()..].to_lowercase()
    } else {
        abs.to_lowercase()
    }
}

fn decode_label_ids(raw: &str) -> Vec<String> {
    let value = raw.trim();
    if value.is_empty() {
        return Vec::new();
    }
    let candidates = if value.starts_with('[') {
        serde_json::from_str::<Vec<String>>(value).unwrap_or_default()
    } else {
        vec![value.to_string()]
    };
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect()
}

fn encode_label_ids(ids: &[String]) -> Result<String, String> {
    serde_json::to_string(ids).map_err(|error| error.to_string())
}

fn next_color_label_definition(
    existing: &[ColorLabelDefinition],
    name: &str,
    hex: &str,
) -> Result<ColorLabelDefinition, String> {
    let id = loop {
        let candidate = format!("label-{}", uuid::Uuid::new_v4().simple());
        if !existing.iter().any(|definition| definition.id == candidate) {
            break candidate;
        }
    };
    let mut proposed = existing.to_vec();
    proposed.push(ColorLabelDefinition {
        id: id.clone(),
        name: name.to_string(),
        hex: hex.to_string(),
    });
    validate_color_label_definitions(&proposed)?
        .into_iter()
        .find(|definition| definition.id == id)
        .ok_or_else(|| "new color label disappeared during validation".to_string())
}

/// Trim, drop empties, lowercase, dedupe, sort, re-join with `, `.
fn clean_tag_list(tags: &str) -> String {
    let set: BTreeSet<String> = tags
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    set.into_iter().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("facial-media-db-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp ws");
        root
    }

    #[test]
    fn key_normalization_relative_inside_workspace_absolute_outside() {
        let ws = temp_ws("keys");
        let db = MediaDb::open(&ws);
        let inside = ws.join("sub").join("a.jpg");
        let key = db.key_for(&inside.to_string_lossy());
        assert_eq!(key, "sub/a.jpg");
        let outside = "E:/Elsewhere/B.jpg";
        assert_eq!(db.key_for(outside), "e:/elsewhere/b.jpg", "casefolded");
        // Any casing of the same file yields the same key.
        let upper = inside.to_string_lossy().to_uppercase();
        assert_eq!(db.key_for(&upper), "sub/a.jpg");
        // Separator style is irrelevant.
        let fwd = inside.to_string_lossy().replace('\\', "/");
        assert_eq!(db.key_for(&fwd), "sub/a.jpg");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn key_normalization_dotdot_verbatim_and_drive_root() {
        // Lexical `..` resolution.
        assert_eq!(slashify("D:/ws/../other/x.jpg"), "D:/other/x.jpg");
        assert_eq!(slashify("D:\\ws\\.\\a\\..\\b.jpg"), "D:/ws/b.jpg");
        // Verbatim prefix stripped.
        assert_eq!(slashify("\\\\?\\D:\\ws\\a.jpg"), "D:/ws/a.jpg");
        // UNC roots keep the doubled slash and survive round-trips.
        assert_eq!(slashify("\\\\server\\share\\x.jpg"), "//server/share/x.jpg");
        assert_eq!(
            slashify("\\\\?\\UNC\\server\\share\\x.jpg"),
            "//server/share/x.jpg"
        );
        // `..` cannot pop past the UNC share anchor.
        assert_eq!(
            slashify("\\\\server\\share\\a\\..\\..\\b.jpg"),
            "//server/share/../b.jpg"
        );
        // Drive-root workspace still produces relative keys (review finding).
        let key = key_for_root(Path::new("D:\\"), "D:\\shoot\\IMG.jpg");
        assert_eq!(key, "shoot/img.jpg");
        let _ = key;
    }

    #[test]
    fn external_unicode_path_never_slices_inside_a_character() {
        // root_prefix is exactly 59 bytes. In the reported path byte 59 is
        // inside the three-byte Korean character '우'; direct byte slicing
        // used to panic before the code could establish that the roots differ.
        let workspace = PathBuf::from(format!("C:/{}", "a".repeat(55)));
        let outside = "D:/Tumblr/kpop vids/#AB6IX #박우진 선배님과 지금 우린 Sticky🦋 #KISSOFLIFE #NATTY #PARKWOOJIN #Sticky #Sticky_Challenge #Shorts.mkv";
        assert_eq!(key_for_root(&workspace, outside), outside.to_lowercase());
    }

    #[test]
    fn case_and_separator_variants_hit_the_same_row() {
        let ws = temp_ws("casefold");
        let db = MediaDb::open(&ws);
        let mixed = ws.join("Shoot").join("IMG_01.JPG");
        db.set_tags(&mixed.to_string_lossy(), "hero").unwrap();
        // Lowercase + forward-slash addressing reads the same row.
        let lower = mixed.to_string_lossy().to_lowercase().replace('\\', "/");
        assert_eq!(db.tags(&lower).as_deref(), Some("hero"));
        // Overwrite through the variant touches the same row (no duplicates).
        db.set_tags(&lower, "hero, extra").unwrap();
        assert_eq!(db.list_meta(None, None).len(), 1, "one row, not two");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn label_vocabulary_mapping() {
        assert_eq!(normalize_label("Red "), Some("red"));
        assert_eq!(normalize_label("purple"), Some("purple"));
        assert_eq!(normalize_label("chartreuse"), Some("gray"));
        assert_eq!(normalize_label("  "), None);
    }

    #[test]
    fn named_color_palette_keeps_stable_asset_ids() {
        let ws = temp_ws("named-labels");
        let db = MediaDb::open(&ws);
        let path = ws.join("asset.jpg").to_string_lossy().to_string();
        db.set_label(&path, "red").unwrap();
        let mut definitions = db.color_label_definitions();
        let red = definitions
            .iter_mut()
            .find(|item| item.id == "red")
            .unwrap();
        red.name = "Selects".to_string();
        red.hex = "#123ABC".to_string();
        db.set_color_label_definitions(&definitions).unwrap();

        assert_eq!(db.label(&path).as_deref(), Some("red"));
        assert_eq!(db.resolve_color_label_id("Selects").as_deref(), Some("red"));
        let saved = db.color_label_definitions();
        let red = saved.iter().find(|item| item.id == "red").unwrap();
        assert_eq!(red.name, "Selects");
        assert_eq!(red.hex, "#123ABC");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn color_hex_is_canonical_and_invalid_palette_is_rejected() {
        assert_eq!(
            normalize_hex_color("12abef"),
            Some(("#12ABEF".to_string(), [0x12, 0xAB, 0xEF]))
        );
        assert!(normalize_hex_color("#fff").is_none());
        let mut definitions = default_color_label_definitions();
        definitions[0].name.clear();
        assert!(validate_color_label_definitions(&definitions).is_err());
        let mut duplicate_hex = default_color_label_definitions();
        duplicate_hex[1].hex = duplicate_hex[0].hex.clone();
        assert!(validate_color_label_definitions(&duplicate_hex)
            .unwrap_err()
            .contains("duplicate color label hex"));
    }

    #[test]
    fn arbitrary_catalog_crud_enforces_unique_name_and_hex() {
        let ws = temp_ws("dynamic-label-catalog");
        let db = MediaDb::open(&ws);
        let created = db.create_color_label(" selects ", "12abef").unwrap();
        assert!(created.id.starts_with("label-"));
        assert_eq!(created.name, "selects");
        assert_eq!(created.hex, "#12ABEF");
        assert!(db.create_color_label("SELECTS", "#112233").is_err());
        assert!(db.create_color_label("another", "#12abef").is_err());

        let updated = db
            .update_color_label(&created.id, Some("Keepers"), Some("#ABCDEF"))
            .unwrap();
        assert_eq!(updated.name, "Keepers");
        assert_eq!(updated.hex, "#ABCDEF");
        assert_eq!(
            db.find_color_label_id("keepers").as_deref(),
            Some(created.id.as_str())
        );
        drop(db);
        let reopened = MediaDb::open(&ws);
        assert!(reopened
            .color_label_definitions()
            .iter()
            .any(|definition| definition == &updated));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn ordered_multi_assignments_are_deduped_and_independently_removed() {
        let ws = temp_ws("multi-labels");
        let db = MediaDb::open(&ws);
        let path = ws.join("asset.jpg").to_string_lossy().to_string();
        let custom = db.create_color_label("Selects", "#123ABC").unwrap();
        db.add_label(&path, "red").unwrap();
        db.add_label(&path, &custom.id).unwrap();
        db.add_label(&path, "RED").unwrap();
        assert_eq!(db.labels(&path), vec!["red", custom.id.as_str()]);
        assert_eq!(db.label(&path).as_deref(), Some("red"));
        assert_eq!(db.meta(&path).labels, vec!["red", custom.id.as_str()]);

        db.remove_label(&path, "Red").unwrap();
        assert_eq!(db.labels(&path), vec![custom.id.as_str()]);
        db.clear_labels(&path).unwrap();
        assert!(db.labels(&path).is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn legacy_singular_assignment_migrates_once_to_v2_json() {
        let ws = temp_ws("label-v2-migration");
        let path = ws.join("asset.jpg").to_string_lossy().to_string();
        {
            let db = MediaDb::open(&ws);
            db.write_str(LABELS, &path, Some("red")).unwrap();
            db.set_setting(COLOR_LABEL_SCHEMA_V2_MARKER, "").unwrap();
        }
        {
            let db = MediaDb::open(&ws);
            assert_eq!(db.labels(&path), vec!["red"]);
            assert_eq!(db.read_str(LABELS, &path).as_deref(), Some("[\"red\"]"));
        }
        {
            let db = MediaDb::open(&ws);
            assert_eq!(db.labels(&path), vec!["red"]);
            assert_eq!(db.read_str(LABELS, &path).as_deref(), Some("[\"red\"]"));
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn usage_aware_delete_requires_confirmation_and_cleans_all_assets() {
        let ws = temp_ws("label-delete");
        let db = MediaDb::open(&ws);
        let a = ws.join("a.jpg").to_string_lossy().to_string();
        let b = ws.join("b.jpg").to_string_lossy().to_string();
        let custom = db.create_color_label("Delete me", "#102030").unwrap();
        db.add_label(&a, &custom.id).unwrap();
        db.add_label(&a, "red").unwrap();
        db.add_label(&b, &custom.id).unwrap();
        assert_eq!(db.color_label_usage_counts()[&custom.id], 2);
        assert!(db.delete_color_label(&custom.id, false).is_err());
        assert_eq!(db.labels(&a), vec![custom.id.as_str(), "red"]);

        let deleted = db.delete_color_label(&custom.id, true).unwrap();
        assert_eq!(deleted.usage_count, 2);
        assert_eq!(deleted.assignments_removed, 2);
        assert_eq!(db.labels(&a), vec!["red"]);
        assert!(db.labels(&b).is_empty());
        assert!(db.find_color_label_id("Delete me").is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn round_trip_notes_tags_labels_favorites() {
        let ws = temp_ws("roundtrip");
        let db = MediaDb::open(&ws);
        let p = ws.join("x.png");
        let p = p.to_string_lossy().to_string();
        db.set_notes(&p, "a note").unwrap();
        db.set_tags(&p, "B, a , b,").unwrap();
        db.set_label(&p, "Blue").unwrap();
        db.add_favorite(&p).unwrap();
        let meta = db.meta(&p);
        assert_eq!(meta.notes, "a note");
        assert_eq!(meta.tags, "a, b");
        assert_eq!(meta.label, "blue");
        assert!(meta.favorite);
        assert_eq!(db.tag_vocab(), vec!["a".to_string(), "b".to_string()]);
        // Clearing removes rows.
        db.set_notes(&p, "").unwrap();
        assert!(db.notes(&p).is_none());
        db.remove_favorite(&p).unwrap();
        assert!(!db.is_favorite(&p));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn reopen_persists() {
        let ws = temp_ws("persist");
        let p = ws.join("keep.jpg").to_string_lossy().to_string();
        {
            let db = MediaDb::open(&ws);
            db.set_notes(&p, "still here").unwrap();
        }
        {
            let db = MediaDb::open(&ws);
            assert_eq!(db.notes(&p).as_deref(), Some("still here"));
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn legacy_json_migrates_once_and_archives() {
        let ws = temp_ws("migrate");
        let inside = ws.join("img").join("c.jpg");
        let legacy = serde_json::json!({
            "version": 1,
            "notes": { inside.to_string_lossy(): "legacy note" },
            "tags": { inside.to_string_lossy(): "Zed, alpha" },
            "color_labels": { inside.to_string_lossy(): "somecolor" },
            "favorites": [ inside.to_string_lossy() ],
        });
        let json_path = MediaDb::legacy_json_path(&ws);
        std::fs::create_dir_all(json_path.parent().unwrap()).unwrap();
        std::fs::write(&json_path, legacy.to_string()).unwrap();

        let db = MediaDb::open(&ws);
        let p = inside.to_string_lossy().to_string();
        assert_eq!(db.notes(&p).as_deref(), Some("legacy note"));
        assert_eq!(db.tags(&p).as_deref(), Some("alpha, zed"));
        assert_eq!(db.label(&p).as_deref(), Some("gray"));
        assert!(db.is_favorite(&p));
        assert!(!json_path.exists(), "json should be archived");
        assert!(json_path.with_extension("json.migrated").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn relocated_workspace_keeps_relative_metadata() {
        let ws_a = temp_ws("move-a");
        let file_a = ws_a.join("shoot").join("d.jpg");
        {
            let db = MediaDb::open(&ws_a);
            db.set_tags(&file_a.to_string_lossy(), "keeper").unwrap();
            db.set_labels(
                &file_a.to_string_lossy(),
                &["red".to_string(), "blue".to_string()],
            )
            .unwrap();
        }
        // Simulate relocation: move the whole .facial state to a new root.
        let ws_b = temp_ws("move-b");
        let _ = std::fs::remove_dir_all(ws_b.join(".facial"));
        std::fs::rename(ws_a.join(".facial"), ws_b.join(".facial")).unwrap();
        let db = MediaDb::open(&ws_b);
        let file_b = ws_b.join("shoot").join("d.jpg");
        assert_eq!(
            db.tags(&file_b.to_string_lossy()).as_deref(),
            Some("keeper")
        );
        assert_eq!(db.labels(&file_b.to_string_lossy()), vec!["red", "blue"]);
        let _ = std::fs::remove_dir_all(&ws_a);
        let _ = std::fs::remove_dir_all(&ws_b);
    }

    #[test]
    fn migration_marker_prevents_reimport_clobber() {
        let ws = temp_ws("marker");
        let file = ws.join("x.jpg");
        let legacy = serde_json::json!({
            "version": 1,
            "notes": { file.to_string_lossy(): "old note" },
            "tags": {}, "color_labels": {}, "favorites": [],
        });
        let json_path = MediaDb::legacy_json_path(&ws);
        std::fs::create_dir_all(json_path.parent().unwrap()).unwrap();
        std::fs::write(&json_path, legacy.to_string()).unwrap();
        let p = file.to_string_lossy().to_string();
        {
            let db = MediaDb::open(&ws);
            assert_eq!(db.notes(&p).as_deref(), Some("old note"));
            db.set_notes(&p, "newer note").unwrap();
        }
        // Adversary: the old JSON reappears (restored from backup).
        std::fs::write(&json_path, legacy.to_string()).unwrap();
        {
            let db = MediaDb::open(&ws);
            assert_eq!(
                db.notes(&p).as_deref(),
                Some("newer note"),
                "reappearing legacy JSON must not clobber newer edits"
            );
            assert!(db.status().is_some(), "stale JSON surfaced in status");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn set_meta_writes_trio_atomically_and_clears() {
        let ws = temp_ws("setmeta");
        let db = MediaDb::open(&ws);
        let p = ws.join("y.png").to_string_lossy().to_string();
        db.set_meta(&p, Some("n"), Some("T2, t1"), Some("Red"))
            .unwrap();
        let meta = db.meta(&p);
        assert_eq!(
            (meta.notes.as_str(), meta.tags.as_str(), meta.label.as_str()),
            ("n", "t1, t2", "red")
        );
        // None leaves untouched; empty clears.
        db.set_meta(&p, None, Some(""), None).unwrap();
        let meta = db.meta(&p);
        assert_eq!(meta.notes, "n");
        assert!(meta.tags.is_empty());
        assert_eq!(meta.label, "red");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn favorites_keep_display_case_and_key_casefolds() {
        let ws = temp_ws("favdisplay");
        let db = MediaDb::open(&ws);
        let pretty = "E:/Shoots/BestOf";
        db.add_favorite(pretty).unwrap();
        assert!(db.is_favorite("e:/shoots/bestof"), "casefolded membership");
        let listed = db.favorites();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].contains("BestOf"),
            "display keeps original case: {}",
            listed[0]
        );
        // Re-adding via a variant does not duplicate.
        db.add_favorite("e:\\shoots\\bestof").unwrap();
        assert_eq!(db.favorites().len(), 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn unc_paths_round_trip_through_keys() {
        let ws = temp_ws("unc");
        let db = MediaDb::open(&ws);
        let unc = "\\\\server\\share\\Shoot\\a.jpg";
        let key = db.key_for(unc);
        assert_eq!(key, "//server/share/shoot/a.jpg");
        let resolved = db.path_for_key(&key);
        assert_eq!(resolved, "\\\\server\\share\\shoot\\a.jpg");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn settings_round_trip() {
        let ws = temp_ws("settings");
        let db = MediaDb::open(&ws);
        db.set_setting("media_split_ratio", "0.58").unwrap();
        assert_eq!(db.setting("media_split_ratio").as_deref(), Some("0.58"));
        db.set_setting("media_split_ratio", "").unwrap();
        assert!(db.setting("media_split_ratio").is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn inventory_generations_replace_exactly_and_survive_cancelled_stage() {
        let ws = temp_ws("inventory-generation");
        let db = MediaDb::open(&ws);
        let store = db.inventory_store().expect("inventory store");
        let root = Path::new("Z:/library");
        let first = vec![
            "Z:/library/a.jpg".to_string(),
            "Z:/library/b.mkv".to_string(),
        ];
        let commit = store
            .replace(root, true, "all", &first, 120, Some(8))
            .unwrap();
        assert_eq!(commit.generation, 1);
        let cached = store.load(root, true, "all").unwrap().unwrap();
        assert_eq!(cached.generation, 1);
        assert_eq!(cached.files, first);

        let replacement: Vec<String> = (0..5_000)
            .map(|index| format!("Z:/library/{index:05}.jpg"))
            .collect();
        let mut checks = 0usize;
        let cancelled = store
            .replace_cancellable(root, true, "all", &replacement, 200, Some(5), || {
                checks += 1;
                checks >= 3
            })
            .unwrap();
        assert!(cancelled.is_none());
        let still_cached = store.load(root, true, "all").unwrap().unwrap();
        assert_eq!(still_cached.generation, 1);
        assert_eq!(still_cached.files, first, "cancel never swaps manifest");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn inventory_rows_rebase_to_the_requested_root_spelling() {
        let stored = vec![
            "Z:/library/a.jpg".to_string(),
            "Z:/library/nested/b.mkv".to_string(),
        ];
        let rebased = rebase_inventory_files(
            &stored,
            Path::new("Z:/library"),
            Path::new("//nas/media/library"),
        )
        .expect("all inventory rows stay beneath their stored root");
        assert_eq!(
            rebased,
            vec![
                Path::new("//nas/media/library")
                    .join("a.jpg")
                    .to_string_lossy()
                    .to_string(),
                Path::new("//nas/media/library")
                    .join("nested/b.mkv")
                    .to_string_lossy()
                    .to_string(),
            ]
        );
        assert!(rebase_inventory_files(
            &["Z:/outside.jpg".to_string()],
            Path::new("Z:/library"),
            Path::new("//nas/media/library"),
        )
        .is_none());
        #[cfg(windows)]
        assert_eq!(
            rebase_inventory_files(
                &["z:\\LIBRARY\\Nested\\Case.JPG".to_string()],
                Path::new("Z:/library"),
                Path::new("//nas/media/library"),
            )
            .expect("Windows root comparison is case/separator insensitive"),
            vec![Path::new("//nas/media/library")
                .join("Nested/Case.JPG")
                .to_string_lossy()
                .to_string()]
        );
    }

    #[test]
    fn inventory_store_is_worker_sendable_and_commits_large_row_set() {
        let ws = temp_ws("inventory-worker");
        let db = MediaDb::open(&ws);
        let store = db.inventory_store().expect("inventory store");
        let root = ws.join("remote-like-root");
        let files: Vec<String> = (0..10_000)
            .map(|index| {
                root.join(format!("{index:05}.jpg"))
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let worker_store = store.clone();
        let worker_root = root.clone();
        let worker_files = files.clone();
        let commit = std::thread::spawn(move || {
            worker_store.replace(&worker_root, true, "all", &worker_files, 50, Some(2))
        })
        .join()
        .expect("inventory worker did not panic")
        .expect("inventory worker commit");
        assert_eq!(commit.generation, 1);
        assert_eq!(
            store.load(&root, true, "all").unwrap().unwrap().files,
            files
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn abandoned_staging_marker_reclaims_orphan_rows() {
        let ws = temp_ws("inventory-orphan");
        let db = MediaDb::open(&ws);
        let store = db.inventory_store().expect("inventory store");
        let namespace = "abandoned-test-namespace";
        let prefix = inventory_item_prefix(namespace);
        let txn = store.db.begin_write().unwrap();
        {
            let mut staging = txn.open_table(INVENTORY_STAGING).unwrap();
            staging.insert(namespace, "crashed").unwrap();
            let mut items = txn.open_table(INVENTORY_ITEMS).unwrap();
            items
                .insert(format!("{prefix}0000000000000000").as_str(), "orphan.jpg")
                .unwrap();
        }
        txn.commit().unwrap();

        store.cleanup_abandoned_staging();
        let txn = store.db.begin_read().unwrap();
        let staging = txn.open_table(INVENTORY_STAGING).unwrap();
        assert!(staging.get(namespace).unwrap().is_none());
        let items = txn.open_table(INVENTORY_ITEMS).unwrap();
        let end = format!("{prefix}~");
        assert_eq!(
            items.range(prefix.as_str()..end.as_str()).unwrap().count(),
            0
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn stable_media_identity_merges_only_normalized_or_proven_aliases() {
        assert_eq!(
            stable_media_path_identity("\\\\MIR\\home\\Video\\A.MKV"),
            stable_media_path_identity("//mir/home/video/a.mkv")
        );
        assert_ne!(
            stable_media_path_identity("//mir/home/video/a.mkv"),
            stable_media_path_identity("//mir/other/video/a.mkv")
        );
        #[cfg(windows)]
        if let Some(mapped_root) = windows_mapped_path_to_unc("Z:\\") {
            let mapped = stable_media_path_identity("Z:\\Video\\alias-test.jpg");
            let unc = stable_media_path_identity(&format!(
                "{}\\Video\\alias-test.jpg",
                mapped_root.trim_end_matches(['\\', '/'])
            ));
            assert_eq!(mapped, unc, "OS-proven mapped/UNC alias identity");
        }
    }

    #[test]
    fn list_meta_filters_by_tag_and_label() {
        let ws = temp_ws("list");
        let db = MediaDb::open(&ws);
        let p1 = ws.join("one.jpg").to_string_lossy().to_string();
        let p2 = ws.join("two.jpg").to_string_lossy().to_string();
        db.set_tags(&p1, "hero, red-dress").unwrap();
        db.set_label(&p1, "red").unwrap();
        db.set_tags(&p2, "b-roll").unwrap();
        let all = db.list_meta(None, None);
        assert_eq!(all.len(), 2);
        let hero = db.list_meta(Some("hero"), None);
        assert_eq!(hero.len(), 1);
        assert!(hero[0].0.ends_with("one.jpg"));
        let red = db.list_meta(None, Some("red"));
        assert_eq!(red.len(), 1);
        let none = db.list_meta(Some("missing"), None);
        assert!(none.is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
