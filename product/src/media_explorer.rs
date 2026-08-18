//! Library / Viewer media explorer state + pure logic (WP-044/WP-060).
//!
//! The Library panel is a virtualized thumbnail grid with a folder strip pinned
//! at the top of its scroll content (it scrolls away). The Viewer panel hosts
//! selected-media playback and metadata. A draggable gutter resizes them;
//! FullGrid expands the Library panel into a full-window thumbnail wall.
//! Rendering glue lives in `ui.rs` (it owns `FacialApp`);
//! everything testable without egui lives here.

use std::path::Path;
use std::{collections::HashMap, sync::Arc};

/// Filesystem roots that can be selected without walking through a parent.
/// Windows exposes assigned local, removable, and mapped drive letters through
/// `GetLogicalDrives`; this avoids probing every letter and potentially
/// blocking on an unavailable mapped drive. Other platforms expose `/`.
pub fn filesystem_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

        // SAFETY: GetLogicalDrives takes no pointers and has no preconditions.
        let mask = unsafe { GetLogicalDrives() };
        let mut roots: Vec<String> = (0..26)
            .filter(|bit| mask & (1u32 << bit) != 0)
            .map(|bit| format!("{}:\\", (b'A' + bit as u8) as char))
            .collect();
        if roots.is_empty() {
            if let Ok(system_drive) = std::env::var("SystemDrive") {
                let drive = system_drive.trim().trim_end_matches(['\\', '/']);
                if drive.len() == 2 && drive.as_bytes()[1] == b':' {
                    roots.push(format!("{drive}\\"));
                }
            }
        }
        roots
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

pub fn path_is_on_root(path: &str, root: &str) -> bool {
    let path = path.replace('/', "\\");
    let root = root.replace('/', "\\");
    path.get(..root.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&root))
}

/// View mode for the media surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaViewMode {
    /// Library panel left, Viewer panel right.
    TwoPanel,
    /// Full-window thumbnail wall (preview hidden).
    FullGrid,
}

impl MediaViewMode {
    pub fn to_setting(self) -> &'static str {
        match self {
            Self::TwoPanel => "two_panel",
            Self::FullGrid => "full_grid",
        }
    }

    pub fn from_setting(raw: &str) -> Self {
        match raw {
            "full_grid" => Self::FullGrid,
            _ => Self::TwoPanel,
        }
    }
}

/// Sort key for the grid (search ranking overrides while a query is active).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaSort {
    Name,
    Modified,
    Size,
    /// WP-068: NTFS creation time. Optional, because some volumes and policies
    /// do not record it; unknown values sort last in both directions.
    Created,
}

impl MediaSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Modified",
            Self::Size => "Size",
            Self::Created => "Created",
        }
    }

    pub fn to_setting(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Size => "size",
            Self::Created => "created",
        }
    }

    pub fn from_setting(raw: &str) -> Self {
        match raw {
            "modified" => Self::Modified,
            "size" => Self::Size,
            "created" => Self::Created,
            _ => Self::Name,
        }
    }

    /// Whether this key needs the stat sidecar rather than just the path.
    pub fn needs_stat(self) -> bool {
        !matches!(self, Self::Name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatFailure {
    NotFound,
    PermissionDenied,
    Unavailable,
    Other,
}

/// Per-file stat sidecar for Modified/Size sorting (filled off-thread).
/// Unknown/error values stay distinct from a real zero-byte file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStat {
    Unknown,
    Known {
        mtime: Option<u64>,
        size: u64,
        /// WP-068: creation time from the same metadata call that yields
        /// `mtime` and `size`. `None` where the platform or volume does not
        /// record it.
        created: Option<u64>,
    },
    Error(StatFailure),
}

impl Default for FileStat {
    fn default() -> Self {
        Self::Unknown
    }
}

impl FileStat {
    pub fn mtime(self) -> Option<u64> {
        match self {
            Self::Known { mtime, .. } => mtime,
            Self::Unknown | Self::Error(_) => None,
        }
    }

    pub fn size(self) -> Option<u64> {
        match self {
            Self::Known { size, .. } => Some(size),
            Self::Unknown | Self::Error(_) => None,
        }
    }

    pub fn created(self) -> Option<u64> {
        match self {
            Self::Known { created, .. } => created,
            Self::Unknown | Self::Error(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaFolderEntry {
    pub path: String,
    pub label: String,
    pub is_parent: bool,
    pub is_drive: bool,
}

/// Immutable, reusable folder-navigation rows. Build once when enumeration
/// completes; immediate-mode paint borrows visible rows without rebuilding
/// or cloning the complete collection.
#[derive(Clone, Debug, Default)]
pub struct PreparedFolderEntries {
    entries: Vec<MediaFolderEntry>,
    drive_count: usize,
}

impl PreparedFolderEntries {
    pub fn all(&self) -> &[MediaFolderEntry] {
        &self.entries
    }

    pub fn get(&self, index: usize) -> Option<&MediaFolderEntry> {
        self.entries.get(index)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn drive_count(&self) -> usize {
        self.drive_count
    }
}

impl std::ops::Deref for PreparedFolderEntries {
    type Target = [MediaFolderEntry];

    fn deref(&self) -> &Self::Target {
        self.all()
    }
}

pub fn prepare_folder_entries(folder: &str, child_names: &[String]) -> PreparedFolderEntries {
    let mut entries = Vec::with_capacity(child_names.len().saturating_add(27));
    for root in filesystem_roots() {
        entries.push(MediaFolderEntry {
            label: format!("{} drive", root.trim_end_matches(['\\', '/'])),
            path: root,
            is_parent: false,
            is_drive: true,
        });
    }
    let drive_count = entries.len();
    if !folder.is_empty() {
        if let Some(parent) = Path::new(folder).parent().and_then(|path| path.to_str()) {
            entries.push(MediaFolderEntry {
                path: parent.to_string(),
                label: "Parent folder".to_string(),
                is_parent: true,
                is_drive: false,
            });
        }
        entries.extend(child_names.iter().map(|name| MediaFolderEntry {
            path: Path::new(folder).join(name).to_string_lossy().to_string(),
            label: name.clone(),
            is_parent: false,
            is_drive: false,
        }));
    }
    PreparedFolderEntries {
        entries,
        drive_count,
    }
}

/// Explorer surface state persisted via the media DB settings table.
pub struct MediaExplorerState {
    pub view_mode: MediaViewMode,
    /// Left-page width as a fraction of the book width (TwoPanel only).
    pub split_ratio: f32,
    /// Displayed tile edge in points (grid recomputes columns from this).
    pub tile_edge: f32,
    /// Whether thumbnail filenames are painted below tiles.
    pub show_names: bool,
    /// Max height of the folder strip's child list before it scrolls.
    pub strip_height: f32,
    /// Operator-set height of the Viewer metadata band (WP-072). The default
    /// matches the previous hard 142pt cap so the shipped look is unchanged
    /// until the operator drags the divider; the render path clamps it to the
    /// panel through `viewer_meta_band_height`.
    pub viewer_meta_height: f32,
    pub sort: MediaSort,
    pub sort_desc: bool,
    /// Grid keyboard cursor in display-index space.
    pub cursor: Option<usize>,
    /// Chrome (header/status/toolbar) hidden for immersive browsing.
    pub chrome_hidden: bool,
    pub show_favorites: bool,
    pub show_settings: bool,
    /// Selected category in the unified in-app Settings window:
    /// Media | Playback | Controls | App.
    pub settings_category: u8,
    /// Transient distance-reading mode for Settings. This never persists: it
    /// uses a separate egui window identity and temporarily requests native
    /// fullscreen without changing the operator's saved font preference.
    pub settings_couch_fullscreen: bool,
    /// Exact native-fullscreen state to restore when the transient Settings
    /// couch mode exits. In Facial that state is owned by `chrome_hidden`.
    pub settings_couch_prior_fullscreen: bool,
    /// Loop selected videos by default. Persisted with the existing media
    /// layout settings and applied by the optional LibVLC player.
    pub video_loop: bool,
    /// Direct local, mapped-drive, or UNC location entry for the in-app folder
    /// navigator. This is deliberately transient: it is navigation input, not
    /// a machine-specific path saved into project configuration.
    pub folder_location_input: String,
    /// Transient folder currently being browsed by the large navigator.
    /// This is deliberately separate from the active Media lane folder:
    /// entering/parenting inside the modal must not change the visible
    /// library or start a media scan until an explicit Open action commits it.
    pub folder_navigator_location: String,
    /// Large couch-distance folder navigator (WP-051).
    pub show_folder_navigator: bool,
    /// Cursor in the navigator entry list (`..` when present, then children).
    pub folder_cursor: Option<usize>,
    /// One-shot request to reveal the controller-focused folder row.
    pub folder_scroll_to_cursor: bool,
    /// Stat sidecar for the current folder (Modified/Size sort).
    pub stats: Arc<HashMap<String, FileStat>>,
    /// True while a stat sweep for the current folder is in flight.
    pub stats_loading: bool,
    /// Unsaved settings changes pending the debounced flush.
    pub settings_dirty: bool,
    /// Last grid scroll offset (stale-thumbnail-job cancellation; not persisted).
    pub last_scroll_top: f32,
    /// Selection anchor in display-index space for Shift-range selection.
    pub sel_anchor_display: Option<usize>,
    /// Column count the grid ACTUALLY rendered with last frame — keyboard
    /// navigation must use this, never a recomputed width guess (the grid
    /// page is narrower than the tab in TwoPanel mode).
    pub last_grid_columns: usize,
    /// When chrome was hidden (drives the transient on-book restore hint).
    pub chrome_hidden_at: Option<std::time::Instant>,
}

impl Default for MediaExplorerState {
    fn default() -> Self {
        Self {
            view_mode: MediaViewMode::TwoPanel,
            split_ratio: 0.62,
            tile_edge: 500.0,
            show_names: false,
            strip_height: 132.0,
            viewer_meta_height: META_DEFAULT,
            sort: MediaSort::Name,
            sort_desc: false,
            cursor: None,
            chrome_hidden: false,
            show_favorites: false,
            show_settings: false,
            settings_category: 0,
            settings_couch_fullscreen: false,
            settings_couch_prior_fullscreen: false,
            video_loop: true,
            folder_location_input: String::new(),
            folder_navigator_location: String::new(),
            show_folder_navigator: false,
            folder_cursor: None,
            folder_scroll_to_cursor: false,
            stats: Arc::new(HashMap::new()),
            stats_loading: false,
            settings_dirty: false,
            last_scroll_top: 0.0,
            sel_anchor_display: None,
            last_grid_columns: 1,
            chrome_hidden_at: None,
        }
    }
}

pub const SPLIT_MIN: f32 = 0.25;
pub const SPLIT_MAX: f32 = 0.80;
pub const TILE_MIN: f32 = 64.0;
pub const TILE_MAX: f32 = 512.0;
pub const STRIP_MIN: f32 = 56.0;
pub const STRIP_MAX: f32 = 400.0;
/// WP-072 Viewer metadata band bounds. META_DEFAULT is the previous hard cap,
/// so untouched layouts stay byte-identical at standard window sizes.
pub const META_DEFAULT: f32 = 142.0;
pub const META_MIN: f32 = 96.0;
/// The band may take at most this fraction of the Viewer panel, so a usable
/// image viewport always remains at maximum band height.
pub const META_MAX_FRACTION: f32 = 0.60;
/// Static persistence bound: values beyond any plausible panel are rejected at
/// load; the per-frame panel clamp below is the live authority.
pub const META_STATIC_MAX: f32 = 1200.0;
const MEDIA_LAYOUT_SETTINGS_VERSION: u32 = 3;

/// WP-072: clamp the stored Viewer metadata band height to the current panel.
/// The band never grows past `META_MAX_FRACTION` of the panel (and always
/// leaves the existing 60pt image floor), and never shrinks below `META_MIN`
/// except on panels too short to honor it, where the upper bound wins so the
/// image floor survives.
pub fn viewer_meta_band_height(panel_h: f32, stored: f32) -> f32 {
    let upper = (panel_h * META_MAX_FRACTION)
        .min((panel_h - 60.0).max(0.0))
        .max(0.0);
    let lower = META_MIN.min(upper);
    stored.clamp(lower, upper.max(lower))
}

impl MediaExplorerState {
    /// Enter transient Settings couch mode and remember the exact native
    /// fullscreen state owned by the Media surface.
    pub fn enter_settings_couch(&mut self) {
        if !self.settings_couch_fullscreen {
            self.settings_couch_prior_fullscreen = self.chrome_hidden;
            self.settings_couch_fullscreen = true;
        }
    }

    /// Leave transient Settings couch mode and return the native fullscreen
    /// state the caller must restore. Repeated exits are no-ops.
    pub fn exit_settings_couch(&mut self) -> Option<bool> {
        if !self.settings_couch_fullscreen {
            return None;
        }
        self.settings_couch_fullscreen = false;
        Some(self.settings_couch_prior_fullscreen)
    }

    /// Load persisted layout settings (missing/invalid values keep defaults).
    pub fn load(db: &crate::media_db::MediaDb) -> Self {
        let mut state = Self::default();
        let stored_version = db
            .setting("media_layout_settings_version")
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(1);
        if let Some(raw) = db.setting("media_view_mode") {
            state.view_mode = MediaViewMode::from_setting(&raw);
        }
        if let Some(v) = db.setting("media_split_ratio").and_then(|r| r.parse().ok()) {
            state.split_ratio = clampf(v, SPLIT_MIN, SPLIT_MAX);
        }
        if let Some(v) = db
            .setting("media_tile_edge")
            .and_then(|r| r.parse::<f32>().ok())
        {
            // The prior release persisted its 168-point default. Migrate only
            // that exact legacy default; preserve intentional custom sizes.
            state.tile_edge = if stored_version < 2 && (v - 168.0).abs() < 0.1 {
                500.0
            } else {
                clampf(v, TILE_MIN, TILE_MAX)
            };
        }
        state.show_names = db.setting("media_show_names").as_deref() == Some("1");
        state.video_loop = db
            .setting("media_video_loop")
            .as_deref()
            .map(|raw| raw != "0")
            .unwrap_or(true);
        if let Some(v) = db
            .setting("media_strip_height")
            .and_then(|r| r.parse().ok())
        {
            state.strip_height = clampf(v, STRIP_MIN, STRIP_MAX);
        }
        if let Some(v) = db
            .setting("media_viewer_meta_height")
            .and_then(|r| r.parse::<f32>().ok())
        {
            if v.is_finite() {
                state.viewer_meta_height = clampf(v, META_MIN, META_STATIC_MAX);
            }
        }
        if let Some(raw) = db.setting("media_sort") {
            state.sort = MediaSort::from_setting(&raw);
        }
        state.sort_desc = db.setting("media_sort_desc").as_deref() == Some("1");
        if stored_version < MEDIA_LAYOUT_SETTINGS_VERSION {
            let _ = db.set_setting("media_tile_edge", &format!("{:.1}", state.tile_edge));
            let _ = db.set_setting(
                "media_layout_settings_version",
                &MEDIA_LAYOUT_SETTINGS_VERSION.to_string(),
            );
        }
        state
    }

    /// Persist layout settings (call through the debounced flush).
    pub fn save(&self, db: &crate::media_db::MediaDb) -> Result<(), String> {
        db.set_setting("media_view_mode", self.view_mode.to_setting())?;
        db.set_setting("media_split_ratio", &format!("{:.4}", self.split_ratio))?;
        db.set_setting("media_tile_edge", &format!("{:.1}", self.tile_edge))?;
        db.set_setting("media_show_names", if self.show_names { "1" } else { "0" })?;
        db.set_setting("media_video_loop", if self.video_loop { "1" } else { "0" })?;
        db.set_setting("media_strip_height", &format!("{:.1}", self.strip_height))?;
        db.set_setting(
            "media_viewer_meta_height",
            &format!("{:.1}", self.viewer_meta_height),
        )?;
        db.set_setting("media_sort", self.sort.to_setting())?;
        db.set_setting("media_sort_desc", if self.sort_desc { "1" } else { "0" })?;
        db.set_setting(
            "media_layout_settings_version",
            &MEDIA_LAYOUT_SETTINGS_VERSION.to_string(),
        )?;
        Ok(())
    }
}

fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    v.clamp(lo, hi)
}

// ---------------------------------------------------------------------------
// Grid math (virtualization)
// ---------------------------------------------------------------------------

/// Computed grid geometry for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridLayout {
    pub columns: usize,
    pub rows: usize,
    pub tile_w: f32,
    pub tile_h: f32,
    /// Full virtual height of the grid content.
    pub content_height: f32,
}

/// Vertical space reserved under each thumbnail for the name line.
pub const TILE_CAPTION_H: f32 = 18.0;
pub const TILE_GAP: f32 = 8.0;

/// Compute grid geometry: columns from available width and tile edge,
/// tiles stretched evenly so the grid always fills the row width.
pub fn grid_layout(
    avail_width: f32,
    tile_edge: f32,
    item_count: usize,
    show_names: bool,
) -> GridLayout {
    let edge = tile_edge.clamp(TILE_MIN, TILE_MAX);
    let usable = (avail_width - TILE_GAP).max(edge);
    let columns = ((usable / (edge + TILE_GAP)).floor() as usize).max(1);
    let tile_w = (usable / columns as f32) - TILE_GAP;
    let tile_h = tile_w + if show_names { TILE_CAPTION_H } else { 0.0 };
    let rows = item_count.div_ceil(columns);
    let content_height = rows as f32 * (tile_h + TILE_GAP);
    GridLayout {
        columns,
        rows,
        tile_w,
        tile_h,
        content_height,
    }
}

/// Visible display-index range for a scroll viewport, with one overscan row
/// each side so scrolling never pops blank tiles.
pub fn visible_range(
    layout: &GridLayout,
    viewport_top: f32,
    viewport_height: f32,
    item_count: usize,
) -> std::ops::Range<usize> {
    if item_count == 0 || layout.columns == 0 {
        return 0..0;
    }
    let row_h = layout.tile_h + TILE_GAP;
    let first_row = ((viewport_top / row_h).floor() as isize - 1).max(0) as usize;
    let last_row = (((viewport_top + viewport_height) / row_h).ceil() as usize) + 1;
    let start = (first_row * layout.columns).min(item_count);
    let end = ((last_row + 1) * layout.columns).min(item_count);
    start..end
}

/// Move a linear controller cursor with clamped ends. Empty lists have no
/// cursor; an unset cursor starts at the first item for forward movement and
/// the last item for backward movement.
pub fn move_list_cursor(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(start) = current.filter(|index| *index < len) else {
        return Some(if delta < 0 { len - 1 } else { 0 });
    };
    Some((start as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize)
}

/// Move a grid cursor by arrow keys in display-index space.
/// `dx` is -1/0/+1 columns, `dy` is -1/0/+1 rows (or larger for page jumps).
pub fn move_cursor(
    cursor: Option<usize>,
    dx: isize,
    dy: isize,
    columns: usize,
    item_count: usize,
) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    // No cursor yet: any navigation lands on the first tile.
    let Some(current) = cursor else {
        return Some(0);
    };
    let columns = columns.max(1) as isize;
    let next = (current as isize + dx + dy * columns).clamp(0, item_count as isize - 1);
    Some(next as usize)
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

/// Produce display order (indices into `files`) for the current sort.
/// Name sorts case-insensitively by file name then full path; Modified/Size
/// use the stat sidecar with name fallback for missing entries.
pub fn sorted_indices(
    files: &[String],
    sort: MediaSort,
    descending: bool,
    stats: &HashMap<String, FileStat>,
) -> Vec<usize> {
    sorted_indices_cancellable(files, sort, descending, stats, || false).unwrap_or_default()
}

/// Cancellation-aware complete-set sort. Lower-cased filename keys are
/// decorated exactly once per row (rather than allocated inside every sort
/// comparison), and bounded sorted runs make cancellation latency independent
/// of the full collection size.
pub fn sorted_indices_cancellable(
    files: &[String],
    sort: MediaSort,
    descending: bool,
    stats: &HashMap<String, FileStat>,
    mut is_cancelled: impl FnMut() -> bool,
) -> Option<Vec<usize>> {
    const RUN: usize = 2_048;
    let mut names = Vec::with_capacity(files.len());
    for path in files {
        if is_cancelled() {
            return None;
        }
        names.push(
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_ascii_lowercase(),
        );
    }

    let compare = |a: usize, b: usize| {
        // WP-068: the final tiebreak is the full path, so runs of equal size or
        // equal timestamp keep a byte-identical order across runs instead of
        // depending on enumeration order.
        let name_order = names[a]
            .cmp(&names[b])
            .then_with(|| files[a].cmp(&files[b]));
        match sort {
            MediaSort::Name if descending => name_order.reverse(),
            MediaSort::Name => name_order,
            MediaSort::Modified => compare_optional_stat(
                stats.get(&files[a]).copied().and_then(FileStat::mtime),
                stats.get(&files[b]).copied().and_then(FileStat::mtime),
                descending,
            )
            .then(name_order),
            MediaSort::Size => compare_optional_stat(
                stats.get(&files[a]).copied().and_then(FileStat::size),
                stats.get(&files[b]).copied().and_then(FileStat::size),
                descending,
            )
            .then(name_order),
            MediaSort::Created => compare_optional_stat(
                stats.get(&files[a]).copied().and_then(FileStat::created),
                stats.get(&files[b]).copied().and_then(FileStat::created),
                descending,
            )
            .then(name_order),
        }
    };

    let mut runs = Vec::with_capacity(files.len().div_ceil(RUN));
    for start in (0..files.len()).step_by(RUN) {
        if is_cancelled() {
            return None;
        }
        let mut run: Vec<usize> = (start..(start + RUN).min(files.len())).collect();
        run.sort_by(|a, b| compare(*a, *b));
        runs.push(run);
    }
    while runs.len() > 1 {
        let mut merged = Vec::with_capacity(runs.len().div_ceil(2));
        let mut iter = runs.into_iter();
        while let Some(left) = iter.next() {
            if is_cancelled() {
                return None;
            }
            let Some(right) = iter.next() else {
                merged.push(left);
                break;
            };
            let mut output = Vec::with_capacity(left.len() + right.len());
            let (mut left_index, mut right_index) = (0usize, 0usize);
            while left_index < left.len() && right_index < right.len() {
                if (output.len() & 0x3ff) == 0 && is_cancelled() {
                    return None;
                }
                if compare(left[left_index], right[right_index]).is_le() {
                    output.push(left[left_index]);
                    left_index += 1;
                } else {
                    output.push(right[right_index]);
                    right_index += 1;
                }
            }
            output.extend_from_slice(&left[left_index..]);
            output.extend_from_slice(&right[right_index..]);
            merged.push(output);
        }
        runs = merged;
    }
    Some(runs.pop().unwrap_or_default())
}

fn compare_optional_stat<T: Ord>(
    left: Option<T>,
    right: Option<T>,
    descending: bool,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) if descending => right.cmp(&left),
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Breadcrumbs
// ---------------------------------------------------------------------------

/// Split an absolute folder path into clickable breadcrumb segments:
/// `(label, full_path_up_to_segment)`, root first. UNC paths anchor on
/// `\\server\share` as one crumb (a bare `\\server` is not navigable).
pub fn breadcrumbs(folder: &str) -> Vec<(String, String)> {
    let normalized = folder.trim().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut acc: String;
    let rest: &str;
    if let Some(unc) = trimmed.strip_prefix("//") {
        // Anchor crumb: \\server\share (both segments required).
        let mut segments = unc.splitn(3, '/');
        let (Some(server), Some(share)) = (segments.next(), segments.next()) else {
            return vec![(format!("\\\\{unc}"), trimmed.to_string())];
        };
        acc = format!("//{server}/{share}");
        out.push((format!("\\\\{server}\\{share}"), acc.clone()));
        rest = segments.next().unwrap_or("");
    } else {
        let mut segments = trimmed.splitn(2, '/');
        let first = segments.next().unwrap_or("");
        if first.is_empty() {
            acc = String::new();
            out.push(("/".to_string(), "/".to_string()));
        } else {
            // Drive root ("D:").
            acc = first.to_string();
            out.push((first.to_string(), format!("{first}/")));
        }
        rest = segments.next().unwrap_or("");
    }
    for segment in rest.split('/').filter(|s| !s.is_empty()) {
        if !acc.ends_with('/') {
            acc.push('/');
        }
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// True if the path has a video extension (icon tile instead of a decode).
pub fn is_video_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".mp4", ".mov", ".mkv", ".avi", ".webm", ".m4v", ".wmv", ".flv", ".mpg", ".mpeg", ".ts",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_meta_band_height_clamps_to_panel_and_bounds() {
        // WP-072. Default on a normal panel is exactly the previous hard cap,
        // so untouched layouts render byte-identical at standard sizes.
        assert_eq!(viewer_meta_band_height(700.0, META_DEFAULT), 142.0);
        // A grown band clamps to the panel fraction so the image viewport
        // stays dominant.
        assert_eq!(viewer_meta_band_height(700.0, 5000.0), 700.0 * 0.60);
        // Below the minimum the band snaps up to stay usable.
        assert_eq!(viewer_meta_band_height(700.0, 10.0), META_MIN);
        // On panels too short for the minimum, the upper bound wins so the
        // 60pt image floor survives; the result is finite and non-negative.
        let tiny = viewer_meta_band_height(120.0, META_DEFAULT);
        assert!(tiny >= 0.0 && tiny <= 120.0 * 0.60 + f32::EPSILON);
        assert!(viewer_meta_band_height(0.0, META_DEFAULT).abs() < f32::EPSILON);
    }

    #[test]
    fn filesystem_roots_are_unique_and_root_shaped() {
        let roots = filesystem_roots();
        assert!(!roots.is_empty());
        let unique: std::collections::HashSet<_> = roots.iter().collect();
        assert_eq!(unique.len(), roots.len());
        #[cfg(windows)]
        for root in roots {
            assert_eq!(root.len(), 3);
            assert_eq!(root.as_bytes()[1], b':');
            assert!(root.ends_with('\\'));
        }
        #[cfg(not(windows))]
        assert_eq!(roots, vec!["/"]);
    }

    #[test]
    fn root_membership_is_boundary_safe_and_separator_agnostic() {
        assert!(path_is_on_root("D:/Movies/K-pop", "D:\\"));
        assert!(path_is_on_root("d:\\Movies", "D:\\"));
        assert!(!path_is_on_root("C:/Movies", "D:\\"));
        assert!(!path_is_on_root("우/video", "D:\\"));
    }

    #[test]
    fn grid_layout_columns_scale_with_width_and_edge() {
        let narrow = grid_layout(200.0, 168.0, 10, false);
        assert_eq!(narrow.columns, 1);
        let wide = grid_layout(900.0, 168.0, 10, false);
        assert!(wide.columns >= 4, "columns {}", wide.columns);
        // Tiles stretch to fill: width * cols + gaps ~ usable width.
        let total = wide.tile_w * wide.columns as f32 + TILE_GAP * wide.columns as f32;
        assert!(
            (total - (900.0 - TILE_GAP)).abs() < 1.0,
            "fills row: {total}"
        );
        assert_eq!(wide.rows, 10usize.div_ceil(wide.columns));
    }

    #[test]
    fn media_front_defaults_to_large_captionless_tiles() {
        let state = MediaExplorerState::default();
        assert_eq!(state.tile_edge, 500.0);
        assert!(!state.show_names);
        let hidden = grid_layout(1100.0, state.tile_edge, 10, false);
        let shown = grid_layout(1100.0, state.tile_edge, 10, true);
        assert_eq!(shown.tile_h - hidden.tile_h, TILE_CAPTION_H);
    }

    #[test]
    fn visible_range_covers_viewport_with_overscan() {
        let layout = grid_layout(900.0, 168.0, 1000, false);
        let row_h = layout.tile_h + TILE_GAP;
        let range = visible_range(&layout, row_h * 10.0, row_h * 4.0, 1000);
        // Rows 9..=16 (one overscan row each side) times columns.
        assert!(range.start <= 9 * layout.columns);
        assert!(range.end >= 15 * layout.columns);
        assert!(range.end <= 1000);
        // Empty grid.
        assert_eq!(visible_range(&layout, 0.0, 100.0, 0), 0..0);
    }

    #[test]
    fn linear_folder_cursor_clamps_and_recovers() {
        assert_eq!(move_list_cursor(None, 1, 4), Some(0));
        assert_eq!(move_list_cursor(None, -1, 4), Some(3));
        assert_eq!(move_list_cursor(Some(0), -1, 4), Some(0));
        assert_eq!(move_list_cursor(Some(3), 1, 4), Some(3));
        assert_eq!(move_list_cursor(Some(99), 0, 4), Some(0));
        assert_eq!(move_list_cursor(Some(0), 1, 0), None);
    }

    #[test]
    fn move_cursor_navigates_grid_and_clamps() {
        // 4 columns, 10 items (rows: 0-3, 4-7, 8-9).
        assert_eq!(move_cursor(None, 0, 0, 4, 10), Some(0));
        assert_eq!(move_cursor(Some(0), 1, 0, 4, 10), Some(1));
        assert_eq!(move_cursor(Some(3), 0, 1, 4, 10), Some(7));
        assert_eq!(move_cursor(Some(7), 0, 1, 4, 10), Some(9), "clamps to last");
        assert_eq!(move_cursor(Some(0), -1, 0, 4, 10), Some(0), "no wrap left");
        assert_eq!(move_cursor(Some(1), 0, -1, 4, 10), Some(0), "clamps up");
        assert_eq!(move_cursor(Some(5), 0, 0, 4, 0), None, "empty grid");
    }

    #[test]
    fn sorted_indices_by_name_modified_size() {
        let files = vec![
            "D:/x/bbb.jpg".to_string(),
            "D:/x/AAA.jpg".to_string(),
            "D:/x/ccc.jpg".to_string(),
        ];
        let mut stats = HashMap::new();
        stats.insert(
            files[0].clone(),
            FileStat::Known {
                mtime: Some(30),
                size: 5,
                created: None,
            },
        );
        stats.insert(
            files[1].clone(),
            FileStat::Known {
                mtime: Some(10),
                size: 50,
                created: None,
            },
        );
        stats.insert(
            files[2].clone(),
            FileStat::Known {
                mtime: Some(20),
                size: 500,
                created: None,
            },
        );

        // WP-068: created-time ordering, and unknown values sorting last in
        // BOTH directions so a volume that does not record creation time never
        // silently reorders the grid.
        let mut created_stats = stats.clone();
        created_stats.insert(
            files[0].clone(),
            FileStat::Known {
                mtime: Some(30),
                size: 5,
                created: Some(300),
            },
        );
        created_stats.insert(
            files[1].clone(),
            FileStat::Known {
                mtime: Some(10),
                size: 50,
                created: Some(100),
            },
        );
        created_stats.insert(
            files[2].clone(),
            FileStat::Known {
                mtime: Some(20),
                size: 500,
                created: None,
            },
        );
        let by_created = sorted_indices(&files, MediaSort::Created, false, &created_stats);
        assert_eq!(by_created, vec![1, 0, 2], "unknown created sorts last");
        let by_created_desc = sorted_indices(&files, MediaSort::Created, true, &created_stats);
        assert_eq!(
            by_created_desc,
            vec![0, 1, 2],
            "unknown created still sorts last when descending"
        );
        // Equal stat values must produce a byte-identical order every run,
        // regardless of how the rows arrived from enumeration (WP-068).
        let tied: Vec<String> = vec![
            "D:/media/zeta.jpg".to_string(),
            "D:/media/alpha.jpg".to_string(),
            "D:/media/Mid.jpg".to_string(),
        ];
        let mut tied_stats = HashMap::new();
        for path in &tied {
            tied_stats.insert(
                path.clone(),
                FileStat::Known {
                    mtime: Some(7),
                    size: 42,
                    created: Some(7),
                },
            );
        }
        for key in [MediaSort::Size, MediaSort::Modified, MediaSort::Created] {
            let first = sorted_indices(&tied, key, false, &tied_stats);
            let again = sorted_indices(&tied, key, false, &tied_stats);
            assert_eq!(first, again, "{key:?} must be deterministic for equal keys");
            assert_eq!(
                first,
                vec![1, 2, 0],
                "{key:?} ties must fall back to case-insensitive name then path"
            );
        }

        assert!(MediaSort::Created.needs_stat());
        assert!(!MediaSort::Name.needs_stat());
        assert_eq!(MediaSort::from_setting("created"), MediaSort::Created);
        assert_eq!(MediaSort::from_setting("nonsense"), MediaSort::Name);

        let by_name = sorted_indices(&files, MediaSort::Name, false, &stats);
        assert_eq!(by_name, vec![1, 0, 2], "case-insensitive name order");
        let by_name_desc = sorted_indices(&files, MediaSort::Name, true, &stats);
        assert_eq!(by_name_desc, vec![2, 0, 1]);
        let by_mtime = sorted_indices(&files, MediaSort::Modified, false, &stats);
        assert_eq!(by_mtime, vec![1, 2, 0]);
        let by_size = sorted_indices(&files, MediaSort::Size, true, &stats);
        assert_eq!(by_size, vec![2, 1, 0]);
        // Missing stats fall back to name order among themselves.
        let no_stats = HashMap::new();
        let fallback = sorted_indices(&files, MediaSort::Modified, false, &no_stats);
        assert_eq!(fallback, vec![1, 0, 2]);
        let mut partial = stats.clone();
        partial.insert(files[2].clone(), FileStat::Error(StatFailure::Unavailable));
        assert_eq!(
            sorted_indices(&files, MediaSort::Size, true, &partial),
            vec![1, 0, 2],
            "unknown/error values stay last even for descending sorts"
        );
    }

    #[test]
    fn complete_sort_is_cancellable_between_bounded_runs() {
        let files: Vec<String> = (0..10_000)
            .rev()
            .map(|index| format!("D:/media/image-{index:05}.jpg"))
            .collect();
        let mut checks = 0usize;
        let result =
            sorted_indices_cancellable(&files, MediaSort::Name, false, &HashMap::new(), || {
                checks += 1;
                checks > 64
            });
        assert!(result.is_none());
    }

    #[test]
    fn complete_name_sort_handles_141400_rows_with_one_key_per_row() {
        let files: Vec<String> = (0..141_400)
            .rev()
            .map(|index| format!("D:/media/image-{index:06}.jpg"))
            .collect();
        let indices =
            sorted_indices_cancellable(&files, MediaSort::Name, false, &HashMap::new(), || false)
                .expect("fixture sort completes");
        assert_eq!(indices.len(), files.len());
        assert_eq!(files[indices[0]], "D:/media/image-000000.jpg");
        assert_eq!(files[*indices.last().unwrap()], "D:/media/image-141399.jpg");
    }

    #[test]
    fn prepared_folder_entries_are_reusable_and_ordered() {
        let children = vec!["alpha".to_string(), "βeta".to_string()];
        let prepared = prepare_folder_entries("D:/media", &children);
        assert!(prepared.drive_count() > 0);
        assert!(prepared
            .all()
            .iter()
            .any(|entry| entry.is_parent && entry.label == "Parent folder"));
        let tail: Vec<&str> = prepared
            .all()
            .iter()
            .filter(|entry| !entry.is_drive && !entry.is_parent)
            .map(|entry| entry.label.as_str())
            .collect();
        assert_eq!(tail, vec!["alpha", "βeta"]);
    }

    #[test]
    fn breadcrumbs_split_windows_paths() {
        let crumbs = breadcrumbs("D:\\Projects\\LLM projects\\shoot");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["D:", "Projects", "LLM projects", "shoot"]);
        assert_eq!(crumbs[0].1, "D:/");
        assert_eq!(crumbs[2].1, "D:/Projects/LLM projects");
        assert!(breadcrumbs("  ").is_empty());
    }

    #[test]
    fn breadcrumbs_anchor_unc_shares() {
        let crumbs = breadcrumbs("\\\\nas\\media\\shoots\\summer");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["\\\\nas\\media", "shoots", "summer"]);
        assert_eq!(crumbs[0].1, "//nas/media");
        assert_eq!(crumbs[1].1, "//nas/media/shoots");
        // Bare server with no share: one non-navigable crumb, no panic.
        let bare = breadcrumbs("\\\\nas");
        assert_eq!(bare.len(), 1);
    }

    #[test]
    fn view_mode_and_sort_settings_round_trip() {
        assert_eq!(
            MediaViewMode::from_setting(MediaViewMode::FullGrid.to_setting()),
            MediaViewMode::FullGrid
        );
        assert_eq!(MediaSort::from_setting("size"), MediaSort::Size);
        assert_eq!(MediaSort::from_setting("garbage"), MediaSort::Name);
    }

    #[test]
    fn video_extension_detection() {
        assert!(is_video_path("a/b/clip.MP4"));
        assert!(is_video_path("clip.webm"));
        assert!(!is_video_path("photo.jpg"));
    }

    #[test]
    fn explorer_state_persists_through_media_db() {
        let ws =
            std::env::temp_dir().join(format!("facial-explorer-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        let db = crate::media_db::MediaDb::open(&ws);
        let mut state = MediaExplorerState::default();
        state.view_mode = MediaViewMode::FullGrid;
        state.split_ratio = 0.44;
        state.tile_edge = 220.0;
        state.show_names = true;
        state.strip_height = 200.0;
        state.sort = MediaSort::Size;
        state.sort_desc = true;
        state.save(&db).unwrap();
        let loaded = MediaExplorerState::load(&db);
        assert_eq!(loaded.view_mode, MediaViewMode::FullGrid);
        assert!((loaded.split_ratio - 0.44).abs() < 1e-3);
        assert!((loaded.tile_edge - 220.0).abs() < 0.5);
        assert!(loaded.show_names);
        assert!((loaded.strip_height - 200.0).abs() < 0.5);
        assert_eq!(loaded.sort, MediaSort::Size);
        assert!(loaded.sort_desc);
        // Out-of-range persisted values clamp on load.
        db.set_setting("media_split_ratio", "9.0").unwrap();
        let clamped = MediaExplorerState::load(&db);
        assert!(clamped.split_ratio <= SPLIT_MAX);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn legacy_default_tile_size_migrates_but_custom_size_is_preserved() {
        let ws = std::env::temp_dir().join(format!(
            "facial-explorer-migration-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let db = crate::media_db::MediaDb::open(&ws);
        db.set_setting("media_tile_edge", "168.0").unwrap();
        let migrated = MediaExplorerState::load(&db);
        assert_eq!(migrated.tile_edge, 500.0);
        assert_eq!(db.setting("media_tile_edge").as_deref(), Some("500.0"));

        db.set_setting("media_layout_settings_version", "1")
            .unwrap();
        db.set_setting("media_tile_edge", "220.0").unwrap();
        let preserved = MediaExplorerState::load(&db);
        assert_eq!(preserved.tile_edge, 220.0);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn settings_couch_restores_exact_prior_fullscreen_state() {
        for prior in [false, true] {
            let mut state = MediaExplorerState::default();
            state.chrome_hidden = prior;
            state.enter_settings_couch();
            assert!(state.settings_couch_fullscreen);
            assert_eq!(state.settings_couch_prior_fullscreen, prior);
            assert_eq!(state.exit_settings_couch(), Some(prior));
            assert!(!state.settings_couch_fullscreen);
            assert_eq!(state.exit_settings_couch(), None);
        }
    }
}
