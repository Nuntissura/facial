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

const NOTES: TableDefinition<&str, &str> = TableDefinition::new("notes");
const TAGS: TableDefinition<&str, &str> = TableDefinition::new("tags");
const LABELS: TableDefinition<&str, &str> = TableDefinition::new("color_labels");
/// Key = casefolded canonical key; value = original-case path for display.
const FAVORITES: TableDefinition<&str, &str> = TableDefinition::new("favorites");
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");

/// Settings marker that makes the legacy-JSON migration one-shot even when
/// the archive rename fails or an old JSON reappears later.
const MIGRATED_MARKER: &str = "legacy_json_migrated";

/// Fixed color-label vocabulary (WP-042). Writes outside this set are mapped
/// via [`normalize_label`]; the UI renders these as swatches.
pub const COLOR_LABELS: [&str; 7] = ["red", "orange", "yellow", "green", "blue", "purple", "gray"];

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
    pub label: String,
    pub favorite: bool,
}

enum Handle {
    ReadWrite(Database),
    ReadOnly(ReadOnlyDatabase),
    Unavailable,
}

pub struct MediaDb {
    handle: Handle,
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
                workspace_root: workspace_root.to_path_buf(),
                status: Some(format!(
                    "media db unavailable: cannot create {}: {err}",
                    dir.display()
                )),
                tag_vocab_cache: std::cell::RefCell::new(None),
            };
        }
        let path = Self::db_path(workspace_root);
        match Database::create(&path) {
            Ok(db) => {
                let mut me = Self {
                    handle: Handle::ReadWrite(db),
                    workspace_root: workspace_root.to_path_buf(),
                    status: None,
                    tag_vocab_cache: std::cell::RefCell::new(None),
                };
                me.migrate_legacy_json();
                me
            }
            Err(create_err) => match ReadOnlyDatabase::open(&path) {
                Ok(db) => Self {
                    handle: Handle::ReadOnly(db),
                    workspace_root: workspace_root.to_path_buf(),
                    status: Some(
                        "media db is locked by another instance — metadata is read-only here"
                            .to_string(),
                    ),
                    tag_vocab_cache: std::cell::RefCell::new(None),
                },
                Err(_) => Self {
                    handle: Handle::Unavailable,
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
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut write_field =
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
                label.map(|l| normalize_label(l).unwrap_or("").to_string()),
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
        self.read_str(LABELS, path)
    }

    /// Set (or clear, with empty input) the color label; input is normalized
    /// onto [`COLOR_LABELS`].
    pub fn set_label(&self, path: &str, label: &str) -> Result<(), String> {
        match normalize_label(label) {
            Some(l) => self.write_str(LABELS, path, Some(l)),
            None => self.write_str(LABELS, path, None),
        }
    }

    /// Combined row for one path.
    pub fn meta(&self, path: &str) -> MediaMeta {
        MediaMeta {
            notes: self.notes(path).unwrap_or_default(),
            tags: self.tags(path).unwrap_or_default(),
            label: self.label(path).unwrap_or_default(),
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

        keys.into_iter()
            .filter_map(|key| {
                let meta = MediaMeta {
                    notes: notes.get(&key).cloned().unwrap_or_default(),
                    tags: tags.get(&key).cloned().unwrap_or_default(),
                    label: labels.get(&key).cloned().unwrap_or_default(),
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
                if let Some(l) = &label_filter {
                    if !meta.label.eq_ignore_ascii_case(l) {
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

    // ------------------------------------------------------------------
    // Legacy migration (WP-038 JSON -> redb), one shot
    // ------------------------------------------------------------------

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
