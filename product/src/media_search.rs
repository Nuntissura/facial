//! Media search v2 (WP-047): fuzzy scoring, filter chips, autocomplete.
//!
//! Pure logic — no egui, no I/O — so every ranking behavior unit-tests
//! directly. The explorer surface feeds it lane paths + metadata (by
//! canonical DB key) and renders the returned ranked indices.
//!
//! Fuzzy scoring follows the fzf/skim family: subsequence match with
//! word-boundary and separator bonuses, gap penalties, and a strong prefix
//! bonus — not whole-string Levenshtein (the WP-040 scorer this supersedes).

use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Query model: free text + filter chips
// ---------------------------------------------------------------------------

/// A parsed search query: free-text terms plus structured filter chips.
/// Chips AND together; free text ranks within the chip-filtered set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaQuery {
    pub text: String,
    pub tags: Vec<String>,
    pub labels: Vec<String>,
    pub kinds: Vec<MediaKindFilter>,
    /// `note:<substring>` chips — notes must contain each (case-insensitive).
    pub notes_contain: Vec<String>,
    /// `fav:` / `fav:1` / `fav:0` — favorite membership (WP-066).
    pub favorite: Option<bool>,
    /// Conflicting favorite requirements are an unsatisfiable AND.
    pub favorite_contradiction: bool,
    /// WP-066 search scope. When set to a lowercase slash-normalized folder
    /// prefix, only files directly inside that folder match. This filters the
    /// existing inventory, so the recursive scan is retained and toggling scope
    /// never triggers a rescan.
    pub folder_only: Option<String>,
    /// Subtractive chips (WP-066). A row matching **any** of these is removed
    /// after the additive filters have selected it.
    pub excluded: ExcludedFilters,
}

/// Subtractive filter terms. Written `!tag:x` or `-tag:x`; both markers are
/// accepted because desktop search teaches `!` (Everything) while code search
/// teaches `-` (GitHub), and operators arrive with either habit (WP-066).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExcludedFilters {
    pub tags: Vec<String>,
    pub labels: Vec<String>,
    pub kinds: Vec<MediaKindFilter>,
    pub notes_contain: Vec<String>,
    /// Bare words that must NOT appear in the file name.
    pub words: Vec<String>,
}

impl ExcludedFilters {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
            && self.labels.is_empty()
            && self.kinds.is_empty()
            && self.notes_contain.is_empty()
            && self.words.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKindFilter {
    Image,
    Video,
}

impl MediaQuery {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.tags.is_empty()
            && self.labels.is_empty()
            && self.kinds.is_empty()
            && self.notes_contain.is_empty()
            && self.favorite.is_none()
            && self.folder_only.is_none()
            && self.excluded.is_empty()
    }

    pub fn has_chips(&self) -> bool {
        !self.tags.is_empty()
            || !self.labels.is_empty()
            || !self.kinds.is_empty()
            || !self.notes_contain.is_empty()
            || self.favorite.is_some()
            || self.folder_only.is_some()
            || !self.excluded.is_empty()
    }
}

/// Split a query into tokens, honoring double quotes so chip values can
/// carry spaces: `tag:"red dress" beach` -> [`tag:"red dress"`, `beach`].
fn split_query_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in raw.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip surrounding double quotes from a chip value.
fn unquote(value: &str) -> &str {
    value.trim_matches('"')
}

/// Remove one token (quote-aware, case-insensitive) from a raw query string.
pub fn remove_query_token(raw: &str, token: &str) -> String {
    let mut tokens = split_query_tokens(raw);
    let remove_at = tokens
        .iter()
        .position(|candidate| candidate == token)
        .or_else(|| {
            tokens
                .iter()
                .position(|candidate| candidate.eq_ignore_ascii_case(token))
        });
    if let Some(index) = remove_at {
        tokens.remove(index);
    }
    tokens.join(" ")
}

/// Wrap a chip value in quotes when it needs them (contains whitespace).
pub fn quote_chip_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// Return the exact query tokens that represent visible/removable filters.
pub fn query_filter_tokens(raw: &str) -> Vec<String> {
    split_query_tokens(raw)
        .into_iter()
        .filter(|token| parse_query(token).has_chips())
        .collect()
}

fn add_favorite_requirement(query: &mut MediaQuery, wanted: bool) {
    match query.favorite {
        Some(existing) if existing != wanted => query.favorite_contradiction = true,
        Some(_) => {}
        None => query.favorite = Some(wanted),
    }
}

/// Parse `tag:x label:red kind:img note:word free text` into a
/// [`MediaQuery`]. Chip values may be double-quoted to carry spaces
/// (`tag:"red dress"`). Unknown prefixes stay in the free text. Values are
/// casefolded.
pub fn parse_query(raw: &str) -> MediaQuery {
    let mut query = MediaQuery::default();
    let mut free_terms: Vec<String> = Vec::new();
    for token in split_query_tokens(raw) {
        // A leading `!`/`-` marks a subtractive term, but only when what
        // follows is a real chip or a bare word the operator typed. A quoted
        // token is always literal, which matters because media filenames
        // very commonly begin with a hyphen (WP-066).
        let quoted = token.starts_with('"');
        let (negated, body) = match (quoted, token.strip_prefix(['!', '-'])) {
            (false, Some(rest)) if !rest.is_empty() && !rest.starts_with('"') => (true, rest),
            _ => (false, token.as_str()),
        };
        let lower = body.to_lowercase();
        if let Some(value) = lower.strip_prefix("tag:") {
            let value = unquote(value);
            if !value.is_empty() {
                if negated {
                    query.excluded.tags.push(value.to_string());
                } else {
                    query.tags.push(value.to_string());
                }
            }
        } else if let Some(value) = lower.strip_prefix("label:") {
            let value = unquote(value);
            if !value.is_empty() {
                if negated {
                    query.excluded.labels.push(value.to_string());
                } else {
                    query.labels.push(value.to_string());
                }
            }
        } else if let Some(value) = lower.strip_prefix("note:") {
            let value = unquote(value);
            if !value.is_empty() {
                if negated {
                    query.excluded.notes_contain.push(value.to_string());
                } else {
                    query.notes_contain.push(value.to_string());
                }
            }
        } else if let Some(value) = lower.strip_prefix("kind:") {
            let kind = match unquote(value) {
                "img" | "image" | "images" | "photo" => Some(MediaKindFilter::Image),
                "vid" | "video" | "videos" | "clip" => Some(MediaKindFilter::Video),
                _ => None,
            };
            match kind {
                Some(kind) if negated => query.excluded.kinds.push(kind),
                Some(kind) => query.kinds.push(kind),
                // Unknown kind values stay free text, exactly as before.
                None => free_terms.push(token),
            }
        } else if let Some(value) = lower.strip_prefix("fav:") {
            let value = unquote(value);
            let wanted = match value {
                "" | "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            };
            match wanted {
                Some(wanted) => add_favorite_requirement(&mut query, wanted != negated),
                None => free_terms.push(token),
            }
        } else if lower == "fav" && !negated {
            add_favorite_requirement(&mut query, true);
        } else if negated {
            query.excluded.words.push(lower);
        } else {
            free_terms.push(token);
        }
    }
    query.text = free_terms.join(" ");
    query
}

/// Metadata view of one candidate row (borrowed, casefolding done here).
#[derive(Clone, Copy, Default)]
pub struct RowMeta<'a> {
    pub tags: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub label: Option<&'a str>,
    pub is_video: bool,
    /// Favorite membership for `fav:` chips (WP-066).
    pub favorite: bool,
    /// File name used by subtractive bare-word terms (WP-066).
    pub name: Option<&'a str>,
    /// Full path used by the folder-only search scope (WP-066).
    pub path: Option<&'a str>,
}

/// True when `path` sits directly inside `folder` (no intervening separator).
/// Both are compared lowercase with normalized separators (WP-066).
pub fn is_direct_child_of(folder: &str, path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let folder = folder.replace('\\', "/").to_lowercase();
    let folder = folder.trim_end_matches('/');
    if folder.is_empty() {
        return !path.trim_start_matches('/').contains('/');
    }
    match path.strip_prefix(folder) {
        Some(rest) => {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            !rest.is_empty() && !rest.contains('/')
        }
        None => false,
    }
}

/// True when a row passes every chip filter (AND semantics; tag chips match
/// list membership, label chips exact, kind chips media type).
pub fn passes_chips(query: &MediaQuery, meta: &RowMeta<'_>) -> bool {
    if query.favorite_contradiction {
        return false;
    }
    for wanted in &query.tags {
        let has = meta
            .tags
            .map(|t| {
                t.split(',')
                    .map(|x| x.trim())
                    .any(|x| x.eq_ignore_ascii_case(wanted))
            })
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    for wanted in &query.labels {
        let has = meta
            .label
            .map(|l| l.eq_ignore_ascii_case(wanted))
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    for wanted in &query.notes_contain {
        let has = meta
            .notes
            .map(|n| n.to_lowercase().contains(wanted.as_str()))
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    if !query.kinds.is_empty() {
        let matches_kind = query.kinds.iter().any(|kind| match kind {
            MediaKindFilter::Image => !meta.is_video,
            MediaKindFilter::Video => meta.is_video,
        });
        if !matches_kind {
            return false;
        }
    }
    if let Some(wanted) = query.favorite {
        if meta.favorite != wanted {
            return false;
        }
    }
    if let Some(folder) = query.folder_only.as_deref() {
        let inside = meta
            .path
            .map(|path| is_direct_child_of(folder, path))
            .unwrap_or(false);
        if !inside {
            return false;
        }
    }
    // Subtractive terms run last: any match removes the row (WP-066).
    for unwanted in &query.excluded.tags {
        let has = meta
            .tags
            .map(|t| {
                t.split(',')
                    .map(|x| x.trim())
                    .any(|x| x.eq_ignore_ascii_case(unwanted))
            })
            .unwrap_or(false);
        if has {
            return false;
        }
    }
    for unwanted in &query.excluded.labels {
        let has = meta
            .label
            .map(|l| l.eq_ignore_ascii_case(unwanted))
            .unwrap_or(false);
        if has {
            return false;
        }
    }
    for unwanted in &query.excluded.notes_contain {
        let has = meta
            .notes
            .map(|n| n.to_lowercase().contains(unwanted.as_str()))
            .unwrap_or(false);
        if has {
            return false;
        }
    }
    for unwanted in &query.excluded.kinds {
        let matches = match unwanted {
            MediaKindFilter::Image => !meta.is_video,
            MediaKindFilter::Video => meta.is_video,
        };
        if matches {
            return false;
        }
    }
    for unwanted in &query.excluded.words {
        let has = meta
            .name
            .map(|name| name.to_lowercase().contains(unwanted.as_str()))
            .unwrap_or(false);
        if has {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Fuzzy scorer (fzf-style subsequence)
// ---------------------------------------------------------------------------

const SCORE_MATCH: i32 = 16;
const BONUS_BOUNDARY: i32 = 12;
const BONUS_SEPARATOR: i32 = 14;
const BONUS_PREFIX: i32 = 10;
const BONUS_CONSECUTIVE: i32 = 8;
const PENALTY_GAP_START: i32 = -3;
const PENALTY_GAP_EXTEND: i32 = -1;

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | '/' | '\\')
}

/// Score `needle` as a subsequence of `haystack` (both should be lowercase).
/// Returns None when not a subsequence. Higher is better; scores are
/// comparable across candidates for the same needle.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = needle.chars().collect();
    if ndl.len() > hay.len() {
        return None;
    }

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut prev_matched = false;
    let mut in_gap = false;
    let mut first_match: Option<usize> = None;

    for &nc in &ndl {
        let mut found = false;
        while hi < hay.len() {
            let hc = hay[hi];
            if hc == nc {
                let mut gain = SCORE_MATCH;
                if hi == 0 {
                    gain += BONUS_PREFIX + BONUS_BOUNDARY;
                } else {
                    let prev = hay[hi - 1];
                    if is_separator(prev) {
                        gain += BONUS_SEPARATOR;
                    } else if prev.is_ascii_digit() != hc.is_ascii_digit() {
                        gain += BONUS_BOUNDARY / 2;
                    }
                }
                if prev_matched {
                    gain += BONUS_CONSECUTIVE;
                }
                score += gain;
                if first_match.is_none() {
                    first_match = Some(hi);
                }
                prev_matched = true;
                in_gap = false;
                hi += 1;
                found = true;
                break;
            } else {
                score += if in_gap {
                    PENALTY_GAP_EXTEND
                } else {
                    PENALTY_GAP_START
                };
                in_gap = true;
                prev_matched = false;
                hi += 1;
            }
        }
        if !found {
            return None;
        }
    }
    // Earlier first match ranks higher; shorter haystacks rank higher on ties.
    score -= (first_match.unwrap_or(0) as i32) / 4;
    score -= (hay.len() as i32) / 8;
    Some(score)
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Search mode for ranking (chips always pre-filter).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RankMode {
    /// Substring on name/path, ranked by match position + name match first.
    Name,
    /// Fuzzy subsequence on name (path fallback).
    Fuzzy,
    /// Metadata-aware: name + tags + notes + label, weighted (local
    /// "semantic" fallback when no CLIP index is available).
    Metadata,
}

/// One ranked hit.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedHit {
    pub index: usize,
    pub score: i32,
}

// ---------------------------------------------------------------------------
// Immutable search index + cancellable requests
// ---------------------------------------------------------------------------

/// Identifies the immutable media snapshot used to build a search index.
///
/// The UI owns generation allocation. A result whose generation no longer
/// matches the displayed media snapshot must not be published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SearchIndexGeneration(pub u64);

/// Stable identity of one indexed search request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SearchRequestKey {
    pub generation: SearchIndexGeneration,
    pub request_id: u64,
}

/// Owned metadata normalized once when an immutable index is built.
///
/// Fields stay private so a row cannot be mutated after construction through
/// this API. Original tag/label spelling is retained for exact compatibility
/// with the legacy `eq_ignore_ascii_case` chip behavior, while lowercase
/// copies serve metadata ranking without per-query allocation.
#[derive(Clone, Debug, Default)]
pub struct IndexedRowMeta {
    tags: Box<[String]>,
    tags_lower: Option<String>,
    notes_lower: Option<String>,
    labels: Box<[String]>,
    is_video: bool,
    favorite: bool,
}

fn ordered_casefold_dedup(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
        .collect()
}

impl IndexedRowMeta {
    pub fn from_borrowed(meta: RowMeta<'_>) -> Self {
        let mut built = Self::from_owned(
            meta.tags.map(str::to_string),
            meta.notes.map(str::to_string),
            meta.label.map(str::to_string),
            meta.is_video,
        );
        built.favorite = meta.favorite;
        built
    }

    /// Favorite membership for `fav:` chips (WP-066). Set after construction so
    /// existing call sites keep working unchanged.
    pub fn with_favorite(mut self, favorite: bool) -> Self {
        self.favorite = favorite;
        self
    }

    pub fn from_owned(
        tags: Option<String>,
        notes: Option<String>,
        label: Option<String>,
        is_video: bool,
    ) -> Self {
        Self::from_owned_labels(tags, notes, label.into_iter().collect(), is_video)
    }

    /// WP-061 multi-label constructor. `labels` may contain stable IDs and
    /// current visible-name aliases; membership is case-insensitive.
    pub fn from_owned_labels(
        tags: Option<String>,
        notes: Option<String>,
        labels: Vec<String>,
        is_video: bool,
    ) -> Self {
        let tag_members = tags
            .as_deref()
            .map(|value| {
                value
                    .split(',')
                    .map(|tag| tag.trim().to_string())
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .unwrap_or_default();
        Self {
            tags: tag_members,
            tags_lower: tags.map(|value| value.to_lowercase()),
            notes_lower: notes.map(|value| value.to_lowercase()),
            labels: ordered_casefold_dedup(labels).into_boxed_slice(),
            is_video,
            favorite: false,
        }
    }

    /// Chip evaluation against an empty file name, for tests that only exercise
    /// metadata chips.
    #[cfg(test)]
    fn passes_chips_for_test(&self, query: &MediaQuery) -> bool {
        self.passes_chips(query, "", "")
    }

    fn passes_chips(
        &self,
        query: &MediaQuery,
        file_name_lower: &str,
        relative_path_lower: &str,
    ) -> bool {
        if query.favorite_contradiction {
            return false;
        }
        for wanted in &query.tags {
            if !self.tags.iter().any(|tag| tag.eq_ignore_ascii_case(wanted)) {
                return false;
            }
        }
        for wanted in &query.labels {
            if !self
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(wanted))
            {
                return false;
            }
        }
        for wanted in &query.notes_contain {
            if !self
                .notes_lower
                .as_deref()
                .map(|notes| notes.contains(wanted.as_str()))
                .unwrap_or(false)
            {
                return false;
            }
        }
        if !query.kinds.is_empty()
            && !query.kinds.iter().any(|kind| match kind {
                MediaKindFilter::Image => !self.is_video,
                MediaKindFilter::Video => self.is_video,
            })
        {
            return false;
        }
        if let Some(wanted) = query.favorite {
            if self.favorite != wanted {
                return false;
            }
        }
        if let Some(folder) = query.folder_only.as_deref() {
            if !is_direct_child_of(folder, relative_path_lower) {
                return false;
            }
        }
        // Subtractive terms, mirroring the legacy `passes_chips` exactly so the
        // indexed and legacy paths cannot diverge (WP-066).
        for unwanted in &query.excluded.tags {
            if self.tags.iter().any(|tag| tag.eq_ignore_ascii_case(unwanted)) {
                return false;
            }
        }
        for unwanted in &query.excluded.labels {
            if self
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(unwanted))
            {
                return false;
            }
        }
        for unwanted in &query.excluded.notes_contain {
            if self
                .notes_lower
                .as_deref()
                .map(|notes| notes.contains(unwanted.as_str()))
                .unwrap_or(false)
            {
                return false;
            }
        }
        for unwanted in &query.excluded.kinds {
            let matches = match unwanted {
                MediaKindFilter::Image => !self.is_video,
                MediaKindFilter::Video => self.is_video,
            };
            if matches {
                return false;
            }
        }
        for unwanted in &query.excluded.words {
            if file_name_lower.contains(unwanted.as_str()) {
                return false;
            }
        }
        true
    }
}

/// One immutable, owned search row. The source index is the value returned in
/// [`RankedHit`]; semantic tie ordering remains the row's position in the
/// surrounding [`MediaSearchIndex`].
#[derive(Clone, Debug)]
pub struct IndexedMediaRow {
    source_index: usize,
    file_name: Arc<str>,
    file_name_lower: Arc<str>,
    /// Original-case path, retained so an activated search result can be opened
    /// with the exact path rather than a casefolded one (WP-066).
    relative_path: Arc<str>,
    relative_path_lower: String,
    meta: IndexedRowMeta,
}

impl IndexedMediaRow {
    pub fn new(
        source_index: usize,
        file_name: impl Into<String>,
        relative_path: impl Into<String>,
        meta: IndexedRowMeta,
    ) -> Self {
        let file_name = file_name.into();
        let file_name_lower: Arc<str> = file_name.to_lowercase().into();
        let relative_path = relative_path.into();
        Self {
            source_index,
            file_name_lower,
            file_name: file_name.into(),
            relative_path_lower: relative_path.to_lowercase(),
            relative_path: relative_path.into(),
            meta,
        }
    }

    pub fn source_index(&self) -> usize {
        self.source_index
    }

    pub fn file_name_lower(&self) -> &str {
        &self.file_name_lower
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn relative_path_lower(&self) -> &str {
        &self.relative_path_lower
    }
}

/// One autocomplete value normalized once with the immutable media snapshot.
/// `display` preserves the first spelling seen in row order; `lower` is used
/// for every subsequent worker-side match.
#[derive(Clone, Debug)]
struct IndexedSuggestionCandidate {
    display: Arc<str>,
    lower: Arc<str>,
    /// Set for file candidates: the row that produced this name, so a selected
    /// result can be opened rather than only inserted as text (WP-066).
    source: Option<(usize, Arc<str>)>,
}

#[derive(Clone, Debug, Default)]
struct IndexedSuggestionCatalog {
    file_names: Arc<[IndexedSuggestionCandidate]>,
    tags: Arc<[IndexedSuggestionCandidate]>,
}

impl IndexedSuggestionCatalog {
    fn from_rows(rows: &[IndexedMediaRow]) -> Self {
        let mut seen_file_names = BTreeSet::new();
        let mut file_names = Vec::new();
        let mut seen_tags = BTreeSet::new();
        let mut tags = Vec::new();

        for row in rows {
            if seen_file_names.insert(row.file_name_lower.clone()) {
                file_names.push(IndexedSuggestionCandidate {
                    display: row.file_name.clone(),
                    lower: row.file_name_lower.clone(),
                    source: Some((row.source_index, row.relative_path.clone())),
                });
            }
            for tag in row.meta.tags.iter().filter(|tag| !tag.is_empty()) {
                let lower: Arc<str> = tag.to_lowercase().into();
                if seen_tags.insert(lower.clone()) {
                    tags.push(IndexedSuggestionCandidate {
                        display: Arc::from(tag.as_str()),
                        lower,
                        source: None,
                    });
                }
            }
        }

        Self {
            file_names: file_names.into(),
            tags: tags.into(),
        }
    }
}

/// Cheaply cloneable immutable search index suitable for a worker thread.
#[derive(Clone, Debug)]
pub struct MediaSearchIndex {
    generation: SearchIndexGeneration,
    rows: Arc<[IndexedMediaRow]>,
    suggestion_catalog: Arc<IndexedSuggestionCatalog>,
}

impl MediaSearchIndex {
    pub fn new(generation: SearchIndexGeneration, rows: impl Into<Arc<[IndexedMediaRow]>>) -> Self {
        let rows = rows.into();
        let suggestion_catalog = Arc::new(IndexedSuggestionCatalog::from_rows(&rows));
        Self {
            generation,
            rows,
            suggestion_catalog,
        }
    }

    /// Compatibility constructor for the existing `(name, path)` + borrowed
    /// metadata representation. Normalization happens once here.
    pub fn from_legacy_rows(
        generation: SearchIndexGeneration,
        rows: &[(String, String)],
        metas: &[RowMeta<'_>],
    ) -> Self {
        let owned: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, (name, path))| {
                IndexedMediaRow::new(
                    index,
                    name.clone(),
                    path.clone(),
                    IndexedRowMeta::from_borrowed(metas.get(index).copied().unwrap_or_default()),
                )
            })
            .collect();
        Self::new(generation, owned)
    }

    pub fn generation(&self) -> SearchIndexGeneration {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[IndexedMediaRow] {
        &self.rows
    }
}

/// Complete owned request that can be moved to a search worker.
#[derive(Clone, Debug)]
pub struct IndexedSearchRequest {
    pub key: SearchRequestKey,
    pub query: MediaQuery,
    pub mode: RankMode,
    pub limit: usize,
}

#[derive(Debug, Default)]
struct LatestRequestState {
    next_request: AtomicU64,
    latest_request: AtomicU64,
}

/// Allocates monotonically newer requests and invalidates prior work without
/// coupling the search module to UI state.
#[derive(Clone, Debug, Default)]
pub struct LatestSearchRequests {
    state: Arc<LatestRequestState>,
}

impl LatestSearchRequests {
    pub fn begin(
        &self,
        generation: SearchIndexGeneration,
        query: MediaQuery,
        mode: RankMode,
        limit: usize,
    ) -> (IndexedSearchRequest, SearchCancelToken) {
        let request_id = self.state.next_request.fetch_add(1, Ordering::Relaxed) + 1;
        self.state
            .latest_request
            .store(request_id, Ordering::Release);
        let key = SearchRequestKey {
            generation,
            request_id,
        };
        (
            IndexedSearchRequest {
                key,
                query,
                mode,
                limit,
            },
            SearchCancelToken {
                key,
                state: Arc::clone(&self.state),
            },
        )
    }

    /// Invalidates the current request, if any. Later requests remain usable.
    pub fn cancel_current(&self) {
        let invalidation = self.state.next_request.fetch_add(1, Ordering::Relaxed) + 1;
        self.state
            .latest_request
            .store(invalidation, Ordering::Release);
    }

    pub fn is_current(&self, key: SearchRequestKey) -> bool {
        self.state.latest_request.load(Ordering::Acquire) == key.request_id
    }
}

/// Cloneable cancellation probe held by a worker with its request.
#[derive(Clone, Debug)]
pub struct SearchCancelToken {
    key: SearchRequestKey,
    state: Arc<LatestRequestState>,
}

impl SearchCancelToken {
    pub fn key(&self) -> SearchRequestKey {
        self.key
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.latest_request.load(Ordering::Acquire) != self.key.request_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexedSearchStatus {
    Complete,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct IndexedSearchDiagnostics {
    pub status: IndexedSearchStatus,
    pub scanned_rows: usize,
    pub matched_rows: usize,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct IndexedSearchResult {
    pub key: SearchRequestKey,
    /// Always empty for cancelled work, preventing accidental publication of
    /// a partial result.
    pub hits: Vec<RankedHit>,
    pub diagnostics: IndexedSearchDiagnostics,
}

impl IndexedSearchResult {
    pub fn is_complete(&self) -> bool {
        self.diagnostics.status == IndexedSearchStatus::Complete
    }
}

/// Rank an immutable index using a latest-request cancellation token.
pub fn rank_indexed(
    index: &MediaSearchIndex,
    request: &IndexedSearchRequest,
    cancellation: &SearchCancelToken,
) -> IndexedSearchResult {
    rank_indexed_cancellable(index, request, || {
        cancellation.key() != request.key || cancellation.is_cancelled()
    })
}

/// Rank an immutable index with a caller-supplied cancellation probe.
///
/// The probe is checked before every row and again before and after ranking.
/// This variant also makes cancellation deterministic to test and lets an
/// integration combine latest-request, shutdown, and navigation signals.
pub fn rank_indexed_cancellable<F>(
    index: &MediaSearchIndex,
    request: &IndexedSearchRequest,
    mut is_cancelled: F,
) -> IndexedSearchResult
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let mut scanned_rows = 0usize;
    let mut hits = Vec::new();

    let cancelled_result = |scanned_rows: usize, matched_rows: usize| IndexedSearchResult {
        key: request.key,
        hits: Vec::new(),
        diagnostics: IndexedSearchDiagnostics {
            status: IndexedSearchStatus::Cancelled,
            scanned_rows,
            matched_rows,
            elapsed: started.elapsed(),
        },
    };

    if request.key.generation != index.generation() || is_cancelled() {
        return cancelled_result(0, 0);
    }

    let text = request.query.text.trim().to_lowercase();
    for (semantic_order, row) in index.rows().iter().enumerate() {
        if is_cancelled() {
            return cancelled_result(scanned_rows, hits.len());
        }
        scanned_rows += 1;
        if !row
            .meta
            .passes_chips(&request.query, &row.file_name_lower, &row.relative_path_lower)
        {
            continue;
        }
        if text.is_empty() {
            hits.push((
                RankedHit {
                    index: row.source_index,
                    score: 0,
                },
                semantic_order,
            ));
            continue;
        }
        let score = indexed_row_score(row, &text, request.mode);
        if let Some(score) = score {
            hits.push((
                RankedHit {
                    index: row.source_index,
                    score,
                },
                semantic_order,
            ));
        }
    }

    if is_cancelled() {
        return cancelled_result(scanned_rows, hits.len());
    }
    if !text.is_empty() {
        hits.sort_by(|(a, a_order), (b, b_order)| b.score.cmp(&a.score).then(a_order.cmp(b_order)));
    }
    if is_cancelled() {
        return cancelled_result(scanned_rows, hits.len());
    }
    let matched_rows = hits.len();
    if request.limit > 0 {
        hits.truncate(request.limit);
    }
    let hits: Vec<_> = hits.into_iter().map(|(hit, _)| hit).collect();
    IndexedSearchResult {
        key: request.key,
        hits,
        diagnostics: IndexedSearchDiagnostics {
            status: IndexedSearchStatus::Complete,
            scanned_rows,
            matched_rows,
            elapsed: started.elapsed(),
        },
    }
}

fn indexed_row_score(row: &IndexedMediaRow, text: &str, mode: RankMode) -> Option<i32> {
    match mode {
        RankMode::Name => {
            if let Some(pos) = row.file_name_lower.find(text) {
                Some(1000 - pos as i32)
            } else {
                row.relative_path_lower
                    .find(text)
                    .map(|pos| 500 - pos as i32)
            }
        }
        RankMode::Fuzzy => fuzzy_score(&row.file_name_lower, text)
            .or_else(|| fuzzy_score(&row.relative_path_lower, text).map(|score| score / 2)),
        RankMode::Metadata => {
            let mut best: Option<i32> = None;
            let mut consider = |candidate: Option<i32>| {
                if let Some(candidate) = candidate {
                    best = Some(best.map_or(candidate, |current| current.max(candidate)));
                }
            };
            consider(
                row.file_name_lower
                    .find(text)
                    .map(|position| 1200 - position as i32),
            );
            consider(fuzzy_score(&row.file_name_lower, text));
            if let Some(tags_lower) = row.meta.tags_lower.as_deref() {
                consider(tags_lower.find(text).map(|_| 1100));
                consider(fuzzy_score(tags_lower, text).map(|score| score + 100));
            }
            if let Some(notes_lower) = row.meta.notes_lower.as_deref() {
                consider(notes_lower.find(text).map(|_| 1000));
            }
            if row
                .meta
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(text))
            {
                consider(Some(900));
            }
            consider(
                row.relative_path_lower
                    .find(text)
                    .map(|position| 400 - position as i32),
            );
            best
        }
    }
}

/// Rank candidate rows for a parsed query. `rows` yields
/// `(file_name_lower, rel_path_lower)`; `meta` is indexed alongside.
/// Empty free text with chips returns all chip-passing rows in input order.
pub fn rank(
    rows: &[(String, String)],
    metas: &[RowMeta<'_>],
    query: &MediaQuery,
    mode: RankMode,
    limit: usize,
) -> Vec<RankedHit> {
    let text = query.text.trim().to_lowercase();
    let mut hits: Vec<RankedHit> = Vec::new();
    for (index, (name, path)) in rows.iter().enumerate() {
        let mut meta = metas.get(index).copied().unwrap_or_default();
        // Subtractive bare-word terms match the file name and the folder-only
        // scope matches the path (WP-066).
        if meta.name.is_none() {
            meta.name = Some(name.as_str());
        }
        if meta.path.is_none() {
            meta.path = Some(path.as_str());
        }
        if !passes_chips(query, &meta) {
            continue;
        }
        if text.is_empty() {
            hits.push(RankedHit { index, score: 0 });
            continue;
        }
        let score = match mode {
            RankMode::Name => {
                if let Some(pos) = name.find(&text) {
                    Some(1000 - pos as i32)
                } else {
                    path.find(&text).map(|pos| 500 - pos as i32)
                }
            }
            RankMode::Fuzzy => {
                fuzzy_score(name, &text).or_else(|| fuzzy_score(path, &text).map(|s| s / 2))
            }
            RankMode::Metadata => {
                let mut best: Option<i32> = None;
                let mut consider = |candidate: Option<i32>| {
                    if let Some(c) = candidate {
                        best = Some(best.map_or(c, |b: i32| b.max(c)));
                    }
                };
                consider(name.find(&text).map(|pos| 1200 - pos as i32));
                consider(fuzzy_score(name, &text));
                if let Some(tags) = meta.tags {
                    let tags_lower = tags.to_lowercase();
                    consider(tags_lower.find(&text).map(|_| 1100));
                    consider(fuzzy_score(&tags_lower, &text).map(|s| s + 100));
                }
                if let Some(notes) = meta.notes {
                    let notes_lower = notes.to_lowercase();
                    consider(notes_lower.find(&text).map(|_| 1000));
                }
                if let Some(label) = meta.label {
                    if label.eq_ignore_ascii_case(&text) {
                        consider(Some(900));
                    }
                }
                consider(path.find(&text).map(|pos| 400 - pos as i32));
                best
            }
        };
        if let Some(score) = score {
            hits.push(RankedHit { index, score });
        }
    }
    if !text.is_empty() {
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
    }
    if limit > 0 {
        hits.truncate(limit);
    }
    hits
}

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

/// One autocomplete suggestion.
#[derive(Clone, Debug, PartialEq)]
pub enum Suggestion {
    /// A concrete file. Carries the identity needed to open it, not just its
    /// display name: a file **name** is not unique across a recursive
    /// inventory, so a name alone cannot resolve a result row (WP-066).
    File(FileSuggestion),
    /// Insert a `tag:<value>` chip.
    Tag(String),
    /// Insert a `label:<value>` chip.
    Label(String),
    /// Insert a folder name as the free-text query.
    Folder(String),
}

/// An activatable file result. `source_index` is only meaningful inside
/// `generation`; when the inventory has advanced, activation falls back to
/// resolving `path` and reports an explicit unavailable state rather than
/// opening whatever now sits at that index (WP-066).
#[derive(Clone, Debug, PartialEq)]
pub struct FileSuggestion {
    pub name: String,
    pub path: String,
    pub source_index: usize,
    pub generation: SearchIndexGeneration,
}

impl Suggestion {
    pub fn display(&self) -> (&'static str, &str) {
        match self {
            Suggestion::File(v) => ("file", v.name.as_str()),
            Suggestion::Tag(v) => ("tag", v),
            Suggestion::Label(v) => ("label", v),
            Suggestion::Folder(v) => ("folder", v),
        }
    }

    /// The file this suggestion opens, when it is a file result.
    pub fn file(&self) -> Option<&FileSuggestion> {
        match self {
            Suggestion::File(file) => Some(file),
            _ => None,
        }
    }

    /// The text that selecting this suggestion produces in the search box
    /// (chips insert their prefix, quoting multi-word values; names replace
    /// the free text).
    pub fn insert_text(&self) -> String {
        match self {
            Suggestion::File(v) => v.name.clone(),
            Suggestion::Folder(v) => v.clone(),
            Suggestion::Tag(v) => format!("tag:{}", quote_chip_value(v)),
            Suggestion::Label(v) => format!("label:{}", quote_chip_value(v)),
        }
    }
}

/// Completion status for an immutable-index autocomplete request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexedSuggestionStatus {
    Complete,
    Cancelled,
}

/// Worker-facing measurements for autocomplete ranking.
#[derive(Clone, Debug)]
pub struct IndexedSuggestionDiagnostics {
    pub status: IndexedSuggestionStatus,
    pub scanned_candidates: usize,
    pub matched_candidates: usize,
    pub elapsed: Duration,
}

/// Autocomplete output tied to the immutable index generation that produced
/// it. Cancelled work never exposes a partial suggestion list.
#[derive(Clone, Debug)]
pub struct IndexedSuggestionResult {
    pub generation: SearchIndexGeneration,
    pub suggestions: Vec<Suggestion>,
    pub diagnostics: IndexedSuggestionDiagnostics,
}

impl IndexedSuggestionResult {
    pub fn is_complete(&self) -> bool {
        self.diagnostics.status == IndexedSuggestionStatus::Complete
    }
}

const SUGGESTION_CANCEL_CHECK_INTERVAL: usize = 64;

fn cancelled_suggestion_result(
    generation: SearchIndexGeneration,
    started: Instant,
    scanned_candidates: usize,
    matched_candidates: usize,
) -> IndexedSuggestionResult {
    IndexedSuggestionResult {
        generation,
        suggestions: Vec::new(),
        diagnostics: IndexedSuggestionDiagnostics {
            status: IndexedSuggestionStatus::Cancelled,
            scanned_candidates,
            matched_candidates,
            elapsed: started.elapsed(),
        },
    }
}

fn complete_suggestion_result(
    generation: SearchIndexGeneration,
    started: Instant,
    scanned_candidates: usize,
    matched_candidates: usize,
    suggestions: Vec<Suggestion>,
) -> IndexedSuggestionResult {
    IndexedSuggestionResult {
        generation,
        suggestions,
        diagnostics: IndexedSuggestionDiagnostics {
            status: IndexedSuggestionStatus::Complete,
            scanned_candidates,
            matched_candidates,
            elapsed: started.elapsed(),
        },
    }
}

fn suggestion_scan_cancelled<F>(scanned_candidates: usize, is_cancelled: &mut F) -> bool
where
    F: FnMut() -> bool,
{
    scanned_candidates > 0
        && scanned_candidates % SUGGESTION_CANCEL_CHECK_INTERVAL == 0
        && is_cancelled()
}

fn indexed_vocab_matches_cancellable<'a, F>(
    token: &str,
    vocab: &'a [IndexedSuggestionCandidate],
    limit: usize,
    scanned_candidates: &mut usize,
    matched_candidates: &mut usize,
    is_cancelled: &mut F,
) -> Option<Vec<&'a str>>
where
    F: FnMut() -> bool,
{
    let mut scored = Vec::new();
    for candidate in vocab {
        if suggestion_scan_cancelled(*scanned_candidates, is_cancelled) {
            return None;
        }
        *scanned_candidates += 1;
        let score = if token.is_empty() {
            Some(0)
        } else {
            prefix_or_fuzzy(candidate.lower.as_ref(), token)
        };
        if let Some(score) = score {
            *matched_candidates += 1;
            scored.push((score, candidate.display.as_ref()));
        }
    }
    if is_cancelled() {
        return None;
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.truncate(limit);
    if is_cancelled() {
        return None;
    }
    Some(scored.into_iter().map(|(_, value)| value).collect())
}

fn external_vocab_matches_cancellable<'a, F>(
    token: &str,
    vocab: &'a [&'a str],
    limit: usize,
    scanned_candidates: &mut usize,
    matched_candidates: &mut usize,
    is_cancelled: &mut F,
) -> Option<Vec<&'a str>>
where
    F: FnMut() -> bool,
{
    let mut scored = Vec::new();
    for value in vocab {
        if suggestion_scan_cancelled(*scanned_candidates, is_cancelled) {
            return None;
        }
        *scanned_candidates += 1;
        let lower = value.to_lowercase();
        let score = if token.is_empty() {
            Some(0)
        } else {
            prefix_or_fuzzy(&lower, token)
        };
        if let Some(score) = score {
            *matched_candidates += 1;
            scored.push((score, *value));
        }
    }
    if is_cancelled() {
        return None;
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.truncate(limit);
    if is_cancelled() {
        return None;
    }
    Some(scored.into_iter().map(|(_, value)| value).collect())
}

/// Rank autocomplete candidates from an immutable [`MediaSearchIndex`].
///
/// File names and row-derived tags use the index's pre-normalized,
/// case-insensitively deduplicated catalog, so a query never rebuilds or
/// lowercases the large media candidate set. Labels and folder names remain
/// caller-owned vocabularies for compatibility with their existing sources.
/// The cancellation probe runs before work, at most every 64 scanned
/// candidates, at phase boundaries, and before publication. A cancelled
/// result is empty and carries the source generation so stale work is safe to
/// reject on the receiving thread.
pub fn suggestions_indexed_cancellable<F>(
    index: &MediaSearchIndex,
    partial: &str,
    label_vocab: &[&str],
    folder_names: &[String],
    limit: usize,
    mut is_cancelled: F,
) -> IndexedSuggestionResult
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let generation = index.generation();
    let mut scanned_candidates = 0usize;
    let mut matched_candidates = 0usize;
    if is_cancelled() {
        return cancelled_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
        );
    }

    let token = partial
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_lowercase();
    if token.is_empty() || limit == 0 {
        return complete_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
            Vec::new(),
        );
    }

    if let Some(rest) = token.strip_prefix("tag:") {
        let Some(tags) = indexed_vocab_matches_cancellable(
            rest,
            &index.suggestion_catalog.tags,
            limit,
            &mut scanned_candidates,
            &mut matched_candidates,
            &mut is_cancelled,
        ) else {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        };
        let suggestions = tags
            .into_iter()
            .map(|tag| Suggestion::Tag(tag.to_string()))
            .collect();
        if is_cancelled() {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        }
        return complete_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
            suggestions,
        );
    }

    if let Some(rest) = token.strip_prefix("label:") {
        let Some(labels) = external_vocab_matches_cancellable(
            rest,
            label_vocab,
            limit,
            &mut scanned_candidates,
            &mut matched_candidates,
            &mut is_cancelled,
        ) else {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        };
        let suggestions = labels
            .into_iter()
            .map(|label| Suggestion::Label(label.to_string()))
            .collect();
        if is_cancelled() {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        }
        return complete_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
            suggestions,
        );
    }

    let mut out: Vec<(i32, Suggestion)> = Vec::new();
    let Some(tags) = indexed_vocab_matches_cancellable(
        &token,
        &index.suggestion_catalog.tags,
        limit,
        &mut scanned_candidates,
        &mut matched_candidates,
        &mut is_cancelled,
    ) else {
        return cancelled_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
        );
    };
    out.extend(
        tags.into_iter()
            .map(|tag| (3000, Suggestion::Tag(tag.to_string()))),
    );

    let Some(labels) = external_vocab_matches_cancellable(
        &token,
        label_vocab,
        2,
        &mut scanned_candidates,
        &mut matched_candidates,
        &mut is_cancelled,
    ) else {
        return cancelled_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
        );
    };
    out.extend(
        labels
            .into_iter()
            .map(|label| (2900, Suggestion::Label(label.to_string()))),
    );

    for folder in folder_names {
        if suggestion_scan_cancelled(scanned_candidates, &mut is_cancelled) {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        }
        scanned_candidates += 1;
        let lower = folder.to_lowercase();
        if let Some(score) = prefix_or_fuzzy(&lower, &token) {
            matched_candidates += 1;
            out.push((score + 1000, Suggestion::Folder(folder.clone())));
        }
    }

    for name in index.suggestion_catalog.file_names.iter() {
        if suggestion_scan_cancelled(scanned_candidates, &mut is_cancelled) {
            return cancelled_suggestion_result(
                generation,
                started,
                scanned_candidates,
                matched_candidates,
            );
        }
        scanned_candidates += 1;
        if let Some(score) = prefix_or_fuzzy(name.lower.as_ref(), &token) {
            matched_candidates += 1;
            let (source_index, path) = name
                .source
                .as_ref()
                .map(|(index, path)| (*index, path.as_ref().to_string()))
                .unwrap_or((0, String::new()));
            out.push((
                score,
                Suggestion::File(FileSuggestion {
                    name: name.display.as_ref().to_string(),
                    path,
                    source_index,
                    generation,
                }),
            ));
        }
    }

    if is_cancelled() {
        return cancelled_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
        );
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(limit);
    if is_cancelled() {
        return cancelled_suggestion_result(
            generation,
            started,
            scanned_candidates,
            matched_candidates,
        );
    }
    complete_suggestion_result(
        generation,
        started,
        scanned_candidates,
        matched_candidates,
        out.into_iter().map(|(_, suggestion)| suggestion).collect(),
    )
}

/// Rank completion candidates for the token currently being typed.
/// Tags/labels/folders are matched by prefix first, then fuzzy; file names
/// by fuzzy. At most `limit` suggestions, tags/labels preferred (they narrow
/// the working set fastest).
pub fn suggestions(
    partial: &str,
    file_names: &[String],
    tag_vocab: &[String],
    label_vocab: &[&str],
    folder_names: &[String],
    limit: usize,
) -> Vec<Suggestion> {
    let token = partial
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_lowercase();
    if token.is_empty() {
        return Vec::new();
    }
    // While typing an explicit chip prefix, complete only that vocabulary.
    if let Some(rest) = token.strip_prefix("tag:") {
        return vocab_matches(rest, tag_vocab, limit)
            .into_iter()
            .map(Suggestion::Tag)
            .collect();
    }
    if let Some(rest) = token.strip_prefix("label:") {
        let labels: Vec<String> = label_vocab.iter().map(|s| s.to_string()).collect();
        return vocab_matches(rest, &labels, limit)
            .into_iter()
            .map(Suggestion::Label)
            .collect();
    }

    let mut out: Vec<(i32, Suggestion)> = Vec::new();
    for tag in vocab_matches(&token, tag_vocab, limit) {
        out.push((3000, Suggestion::Tag(tag)));
    }
    let labels: Vec<String> = label_vocab.iter().map(|s| s.to_string()).collect();
    for label in vocab_matches(&token, &labels, 2) {
        out.push((2900, Suggestion::Label(label)));
    }
    for folder in folder_names {
        let lower = folder.to_lowercase();
        if let Some(score) = prefix_or_fuzzy(&lower, &token) {
            out.push((score + 1000, Suggestion::Folder(folder.clone())));
        }
    }
    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    for (index, name) in file_names.iter().enumerate() {
        let lower = name.to_lowercase();
        if seen_names.contains(&lower) {
            continue;
        }
        if let Some(score) = prefix_or_fuzzy(&lower, &token) {
            seen_names.insert(lower);
            out.push((
                score,
                Suggestion::File(FileSuggestion {
                    name: name.clone(),
                    // The legacy vocabulary path carries names only; activation
                    // resolves by name against the live inventory (WP-066).
                    path: String::new(),
                    source_index: index,
                    generation: SearchIndexGeneration(0),
                }),
            ));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(limit);
    out.into_iter().map(|(_, s)| s).collect()
}

fn prefix_or_fuzzy(candidate_lower: &str, token: &str) -> Option<i32> {
    if candidate_lower.starts_with(token) {
        Some(2000 - candidate_lower.len() as i32)
    } else {
        fuzzy_score(candidate_lower, token)
    }
}

fn vocab_matches(token: &str, vocab: &[String], limit: usize) -> Vec<String> {
    let mut scored: Vec<(i32, &String)> = vocab
        .iter()
        .filter_map(|v| {
            let lower = v.to_lowercase();
            if token.is_empty() {
                Some((0, v))
            } else {
                prefix_or_fuzzy(&lower, token).map(|s| (s, v))
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, v)| v.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_extracts_chips_and_free_text() {
        let q = parse_query("tag:hero label:RED kind:img red dress");
        assert_eq!(q.tags, vec!["hero"]);
        assert_eq!(q.labels, vec!["red"]);
        assert_eq!(q.kinds, vec![MediaKindFilter::Image]);
        assert_eq!(q.text, "red dress");
        assert!(!q.is_empty() && q.has_chips());
        let plain = parse_query("just words");
        assert!(plain.tags.is_empty() && !plain.is_empty());
        assert_eq!(parse_query("  ").text, "");
        assert!(parse_query("").is_empty());
        // Unknown kind value stays free text.
        let odd = parse_query("kind:weird thing");
        assert!(odd.kinds.is_empty());
        assert_eq!(odd.text, "kind:weird thing");
        // Quoted chip values carry spaces (round 3, finding 8).
        let quoted = parse_query("tag:\"red dress\" beach note:\"golden hour\"");
        assert_eq!(quoted.tags, vec!["red dress"]);
        assert_eq!(quoted.notes_contain, vec!["golden hour"]);
        assert_eq!(quoted.text, "beach");
        assert_eq!(quote_chip_value("red dress"), "\"red dress\"");
        assert_eq!(quote_chip_value("hero"), "hero");
        assert_eq!(
            Suggestion::Tag("red dress".to_string()).insert_text(),
            "tag:\"red dress\""
        );
    }

    #[test]
    fn visible_filter_tokens_preserve_exact_spelling_for_removal() {
        let raw = "clip tag:\"red dress\" !label:reject -kind:vid fav: -blooper \"-literal\" kind:unknown";
        assert_eq!(
            query_filter_tokens(raw),
            vec![
                "tag:\"red dress\"",
                "!label:reject",
                "-kind:vid",
                "fav:",
                "-blooper",
            ]
        );
        for token in query_filter_tokens(raw) {
            let remaining = remove_query_token(raw, &token);
            assert!(!split_query_tokens(&remaining).contains(&token));
        }
    }

    #[test]
    fn duplicate_filter_removal_uses_the_clicked_case_variant() {
        let raw = "tag:keep TAG:KEEP clip";
        assert_eq!(remove_query_token(raw, "tag:keep"), "TAG:KEEP clip");
        assert_eq!(remove_query_token(raw, "TAG:KEEP"), "tag:keep clip");
    }

    /// The display pipeline picks between "apply the operator's sort" and
    /// "use ranker output" from the query. That decision must key on whether
    /// there is FREE TEXT to rank by, not on `is_empty()`: a chip — including
    /// the folder-only scope, which the operator never typed — makes a query
    /// non-empty while leaving nothing to rank. Keying on `is_empty()` there
    /// silently disabled the sort control entirely.
    #[test]
    fn a_chip_only_query_has_no_free_text_to_rank() {
        for raw in [
            "fav:",
            "tag:hero",
            "!label:red",
            "kind:vid",
            "-blooper",
        ] {
            let query = parse_query(raw);
            assert!(
                !query.is_empty(),
                "{raw} must count as a real query for filtering"
            );
            assert!(
                query.text.trim().is_empty() || raw == "-blooper",
                "{raw} carries no free text to rank by"
            );
        }
        // Scope is set by a toggle, not typed, and must behave the same way.
        let mut scoped = parse_query("");
        scoped.folder_only = Some("D:/media".to_string());
        assert!(!scoped.is_empty(), "scope is a real filter");
        assert!(scoped.text.trim().is_empty(), "scope adds no rankable text");
        assert!(scoped.has_chips(), "scope must be treated as a chip filter");
    }

    /// WP-066: subtractive terms. Both `!` (Everything) and `-` (GitHub) are
    /// accepted, and a literal leading hyphen in a filename must survive —
    /// media filenames commonly start with one.
    #[test]
    fn negation_marks_subtractive_terms_without_eating_literal_hyphens() {
        let q = parse_query("beach !tag:hero -label:red -kind:vid !note:draft -blooper");
        assert_eq!(q.text, "beach");
        assert_eq!(q.excluded.tags, vec!["hero".to_string()]);
        assert_eq!(q.excluded.labels, vec!["red".to_string()]);
        assert_eq!(q.excluded.kinds, vec![MediaKindFilter::Video]);
        assert_eq!(q.excluded.notes_contain, vec!["draft".to_string()]);
        assert_eq!(q.excluded.words, vec!["blooper".to_string()]);
        assert!(q.tags.is_empty() && q.labels.is_empty() && q.kinds.is_empty());

        // A quoted term is always literal, never negation.
        let quoted = parse_query("\"-foo.jpg\"");
        assert!(quoted.excluded.is_empty());
        assert_eq!(quoted.text, "\"-foo.jpg\"");

        // Additive and subtractive forms of the same chip coexist.
        let mixed = parse_query("tag:hero !tag:reject");
        assert_eq!(mixed.tags, vec!["hero".to_string()]);
        assert_eq!(mixed.excluded.tags, vec!["reject".to_string()]);
    }

    /// WP-066: favorites become a first-class filter term.
    #[test]
    fn favorite_chip_filters_and_negates() {
        assert_eq!(parse_query("fav:").favorite, Some(true));
        assert_eq!(parse_query("fav").favorite, Some(true));
        assert_eq!(parse_query("fav:1").favorite, Some(true));
        assert_eq!(parse_query("fav:0").favorite, Some(false));
        assert_eq!(parse_query("!fav:1").favorite, Some(false));
        assert_eq!(parse_query("-fav:").favorite, Some(false));

        let faved = RowMeta {
            favorite: true,
            ..Default::default()
        };
        let plain = RowMeta::default();
        assert!(passes_chips(&parse_query("fav:"), &faved));
        assert!(!passes_chips(&parse_query("fav:"), &plain));
        assert!(passes_chips(&parse_query("!fav:"), &plain));
        assert!(!passes_chips(&parse_query("!fav:"), &faved));
        for raw in ["fav: fav:0", "fav: !fav:"] {
            let contradictory = parse_query(raw);
            assert!(contradictory.favorite_contradiction, "{raw}");
            assert!(!passes_chips(&contradictory, &faved), "{raw}");
            assert!(!passes_chips(&contradictory, &plain), "{raw}");
        }
    }

    /// WP-066: subtractive terms remove rows the additive terms selected.
    #[test]
    fn subtractive_terms_remove_rows_after_additive_selection() {
        let hero_red = RowMeta {
            tags: Some("hero, red dress"),
            label: Some("red"),
            name: Some("shot-blooper.jpg"),
            ..Default::default()
        };
        let hero_only = RowMeta {
            tags: Some("hero"),
            name: Some("shot-keeper.jpg"),
            ..Default::default()
        };
        let keep = parse_query("tag:hero !label:red");
        assert!(!passes_chips(&keep, &hero_red));
        assert!(passes_chips(&keep, &hero_only));

        let word = parse_query("tag:hero -blooper");
        assert!(!passes_chips(&word, &hero_red));
        assert!(passes_chips(&word, &hero_only));
    }

    /// WP-066: folder-only scope keeps a recursive inventory but restricts
    /// matches to direct children, so toggling it never needs a rescan.
    #[test]
    fn folder_only_scope_matches_direct_children_only() {
        assert!(is_direct_child_of(r"D:\media", r"D:\media\a.jpg"));
        assert!(!is_direct_child_of(r"D:\media", r"D:\media\sub\a.jpg"));
        assert!(is_direct_child_of("d:/media", r"D:\MEDIA\A.JPG"));
        assert!(!is_direct_child_of(r"D:\media", r"D:\media-old\a.jpg"));
        assert!(!is_direct_child_of(r"D:\media", r"D:\media"));

        let mut scoped = parse_query("");
        scoped.folder_only = Some(r"D:\media".to_string());
        let direct = RowMeta {
            path: Some(r"D:\media\a.jpg"),
            ..Default::default()
        };
        let nested = RowMeta {
            path: Some(r"D:\media\sub\a.jpg"),
            ..Default::default()
        };
        assert!(passes_chips(&scoped, &direct));
        assert!(!passes_chips(&scoped, &nested));
    }

    #[test]
    fn chips_filter_with_and_semantics() {
        let q = parse_query("tag:hero label:red");
        let hit = RowMeta {
            tags: Some("hero, b-roll"),
            label: Some("red"),
            ..Default::default()
        };
        let miss_tag = RowMeta {
            tags: Some("b-roll"),
            label: Some("red"),
            ..Default::default()
        };
        let miss_label = RowMeta {
            tags: Some("hero"),
            label: Some("blue"),
            ..Default::default()
        };
        assert!(passes_chips(&q, &hit));
        assert!(!passes_chips(&q, &miss_tag));
        assert!(!passes_chips(&q, &miss_label));
        let vid = parse_query("kind:vid");
        assert!(passes_chips(
            &vid,
            &RowMeta {
                is_video: true,
                ..Default::default()
            }
        ));
        assert!(!passes_chips(&vid, &RowMeta::default()));
    }

    #[test]
    fn indexed_multi_labels_match_each_membership_and_visible_name_alias() {
        let meta = IndexedRowMeta::from_owned_labels(
            Some("hero".to_string()),
            None,
            vec![
                "red".to_string(),
                "Selects".to_string(),
                "label-abc".to_string(),
                "Keepers".to_string(),
                "SELECTS".to_string(),
            ],
            false,
        );
        assert!(meta.passes_chips_for_test(&parse_query("label:red")));
        assert!(meta.passes_chips_for_test(&parse_query("label:selects")));
        assert!(meta.passes_chips_for_test(&parse_query("label:label-abc")));
        assert!(meta.passes_chips_for_test(&parse_query("label:keepers")));
        assert!(meta.passes_chips_for_test(&parse_query("label:red label:keepers")));
        assert!(!meta.passes_chips_for_test(&parse_query("label:rejects")));
        assert_eq!(meta.labels.len(), 4, "casefolded aliases are deduplicated");
    }

    #[test]
    fn fuzzy_prefers_boundaries_prefixes_and_consecutive_runs() {
        // Typo tolerance: subsequence, not exact.
        assert!(fuzzy_score("red_dress_004.png", "rdress").is_some());
        assert!(fuzzy_score("red_dress_004.png", "zebra").is_none());
        // Prefix beats mid-string.
        let prefix = fuzzy_score("red_dress.png", "red").unwrap();
        let mid = fuzzy_score("bored_dress.png", "red").unwrap();
        assert!(prefix > mid, "{prefix} vs {mid}");
        // Word-boundary match beats buried match.
        let boundary = fuzzy_score("shoot_red_01.png", "red").unwrap();
        let buried = fuzzy_score("bored_01.png", "red").unwrap();
        assert!(boundary > buried, "{boundary} vs {buried}");
        // Consecutive run beats scattered letters.
        let run = fuzzy_score("dress.png", "dre").unwrap();
        let scattered = fuzzy_score("d1r2e3ss.png", "dre").unwrap();
        assert!(run > scattered, "{run} vs {scattered}");
    }

    #[test]
    fn rank_modes_order_sensibly() {
        let rows: Vec<(String, String)> = [
            ("red_dress.png", "shoot/red_dress.png"),
            ("blue_dress.png", "shoot/blue_dress.png"),
            ("dress_red_2.png", "shoot/dress_red_2.png"),
            ("unrelated.png", "other/unrelated.png"),
        ]
        .iter()
        .map(|(n, p)| (n.to_string(), p.to_string()))
        .collect();
        let metas = vec![RowMeta::default(); rows.len()];
        let q = parse_query("red");
        let name_hits = rank(&rows, &metas, &q, RankMode::Name, 0);
        assert_eq!(name_hits[0].index, 0, "earliest substring first");
        assert_eq!(name_hits.len(), 2, "substring-only in Name mode");
        let fuzzy_hits = rank(&rows, &metas, &q, RankMode::Fuzzy, 0);
        assert!(fuzzy_hits.len() >= 2);
        assert_eq!(fuzzy_hits[0].index, 0);
        // Metadata mode: a tag hit outranks a filename hit.
        let metas2 = vec![
            RowMeta::default(),
            RowMeta {
                tags: Some("red, hero"),
                ..Default::default()
            },
            RowMeta::default(),
            RowMeta::default(),
        ];
        let meta_hits = rank(&rows, &metas2, &q, RankMode::Metadata, 0);
        let pos_tagged = meta_hits.iter().position(|h| h.index == 1).unwrap();
        let pos_named = meta_hits.iter().position(|h| h.index == 0).unwrap();
        assert!(pos_tagged > pos_named || meta_hits[0].index == 0 || meta_hits[0].index == 1);
        // Chips + empty text: all passing rows, input order.
        let chips_only = parse_query("tag:hero");
        let hits = rank(&rows, &metas2, &chips_only, RankMode::Name, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 1);
    }

    #[test]
    fn immutable_index_matches_legacy_rank_for_all_modes_and_limits() {
        let rows: Vec<(String, String)> = [
            ("red_dress.png", "shoot/red_dress.png"),
            ("blue_dress.png", "shoot/blue_dress.png"),
            ("dress_red_2.png", "archive/dress_red_2.png"),
            ("hero_clip.mp4", "video/hero_clip.mp4"),
            ("äther.png", "unicode/äther.png"),
            ("other.jpg", "deep/red/other.jpg"),
        ]
        .iter()
        .map(|(name, path)| (name.to_string(), path.to_string()))
        .collect();
        let metas = vec![
            RowMeta {
                tags: Some("hero, red dress"),
                notes: Some("Golden Hour portrait"),
                label: Some("Red"),
                is_video: false,
                favorite: true,
                    ..Default::default()
            },
            RowMeta {
                tags: Some("blue, alternate"),
                notes: Some("Studio portrait"),
                label: Some("Blue"),
                is_video: false,
                    ..Default::default()
            },
            RowMeta::default(),
            RowMeta {
                tags: Some("hero, b-roll"),
                notes: Some("Golden hour motion"),
                label: Some("Red"),
                is_video: true,
                    ..Default::default()
            },
            RowMeta {
                tags: Some("ÄTHER"),
                notes: None,
                label: Some("Ä"),
                is_video: false,
                    ..Default::default()
            },
            RowMeta {
                label: Some("Ä"),
                ..Default::default()
            },
        ];
        let index = MediaSearchIndex::from_legacy_rows(SearchIndexGeneration(17), &rows, &metas);
        let coordinator = LatestSearchRequests::default();
        let mut folder_scoped = parse_query("");
        folder_scoped.folder_only = Some("shoot".to_string());
        let queries = vec![
            parse_query("red"),
            parse_query("rd"),
            parse_query("golden"),
            parse_query("tag:hero"),
            parse_query("tag:hero kind:video golden"),
            parse_query("label:red note:hour"),
            parse_query("kind:image"),
            parse_query("Ä"),
            parse_query("label:Ä"),
            parse_query("tag:hero !label:blue -kind:img"),
            parse_query("!note:studio -other"),
            parse_query("fav:"),
            parse_query("!fav:"),
            parse_query("fav: fav:0"),
            folder_scoped,
            MediaQuery::default(),
        ];

        for mode in [RankMode::Name, RankMode::Fuzzy, RankMode::Metadata] {
            for query in &queries {
                for limit in [0, 1, 2, 20] {
                    let expected = rank(&rows, &metas, query, mode, limit);
                    let (request, token) =
                        coordinator.begin(index.generation(), query.clone(), mode, limit);
                    let actual = rank_indexed(&index, &request, &token);
                    assert!(actual.is_complete());
                    assert_eq!(
                        actual.hits, expected,
                        "mode={mode:?} query={query:?} limit={limit}"
                    );
                    assert_eq!(actual.diagnostics.scanned_rows, rows.len());
                    assert!(actual.diagnostics.matched_rows >= actual.hits.len());
                }
            }
        }
    }

    #[test]
    fn immutable_index_preserves_semantic_input_order_for_equal_scores() {
        let rows = vec![
            IndexedMediaRow::new(90, "same-a.png", "same-a.png", IndexedRowMeta::default()),
            IndexedMediaRow::new(10, "same-b.png", "same-b.png", IndexedRowMeta::default()),
        ];
        let index = MediaSearchIndex::new(SearchIndexGeneration(3), rows);
        let coordinator = LatestSearchRequests::default();
        let (request, token) =
            coordinator.begin(index.generation(), parse_query("same"), RankMode::Name, 0);
        let result = rank_indexed(&index, &request, &token);
        assert_eq!(
            result.hits.iter().map(|hit| hit.index).collect::<Vec<_>>(),
            vec![90, 10]
        );
    }

    #[test]
    fn latest_request_and_generation_cancel_stale_work_without_partial_hits() {
        let legacy_rows: Vec<(String, String)> = (0..2_000)
            .map(|index| {
                (
                    format!("matching-{index}.jpg"),
                    format!("folder/matching-{index}.jpg"),
                )
            })
            .collect();
        let index = MediaSearchIndex::from_legacy_rows(SearchIndexGeneration(8), &legacy_rows, &[]);
        let coordinator = LatestSearchRequests::default();
        let (old_request, old_token) = coordinator.begin(
            index.generation(),
            parse_query("matching"),
            RankMode::Name,
            0,
        );
        let (new_request, new_token) = coordinator.begin(
            index.generation(),
            parse_query("matching-1999"),
            RankMode::Name,
            0,
        );
        assert!(!coordinator.is_current(old_request.key));
        assert!(coordinator.is_current(new_request.key));
        assert!(old_token.is_cancelled());
        let old_result = rank_indexed(&index, &old_request, &old_token);
        assert_eq!(
            old_result.diagnostics.status,
            IndexedSearchStatus::Cancelled
        );
        assert!(old_result.hits.is_empty());

        let new_result = rank_indexed(&index, &new_request, &new_token);
        assert!(new_result.is_complete());
        assert_eq!(new_result.hits.len(), 1);

        // A token must belong to the supplied request, even if that token is
        // itself current.
        let mismatched = rank_indexed(&index, &old_request, &new_token);
        assert_eq!(
            mismatched.diagnostics.status,
            IndexedSearchStatus::Cancelled
        );
        assert!(mismatched.hits.is_empty());

        let wrong_generation_request = IndexedSearchRequest {
            key: SearchRequestKey {
                generation: SearchIndexGeneration(9),
                request_id: new_request.key.request_id,
            },
            query: parse_query("matching"),
            mode: RankMode::Name,
            limit: 0,
        };
        let wrong_generation =
            rank_indexed_cancellable(&index, &wrong_generation_request, || false);
        assert_eq!(
            wrong_generation.diagnostics.status,
            IndexedSearchStatus::Cancelled
        );
        assert_eq!(wrong_generation.diagnostics.scanned_rows, 0);

        coordinator.cancel_current();
        assert!(new_token.is_cancelled());
    }

    #[test]
    fn indexed_ranking_observes_mid_scan_cancellation() {
        let legacy_rows: Vec<(String, String)> = (0..4_000)
            .map(|index| {
                (
                    format!("matching-{index}.jpg"),
                    format!("folder/matching-{index}.jpg"),
                )
            })
            .collect();
        let index =
            MediaSearchIndex::from_legacy_rows(SearchIndexGeneration(11), &legacy_rows, &[]);
        let request = IndexedSearchRequest {
            key: SearchRequestKey {
                generation: index.generation(),
                request_id: 44,
            },
            query: parse_query("matching"),
            mode: RankMode::Name,
            limit: 0,
        };
        let mut probes = 0usize;
        let result = rank_indexed_cancellable(&index, &request, || {
            probes += 1;
            probes > 514
        });
        assert_eq!(result.diagnostics.status, IndexedSearchStatus::Cancelled);
        assert!(
            result.hits.is_empty(),
            "partial hits must never be published"
        );
        assert!(result.diagnostics.scanned_rows > 0);
        assert!(result.diagnostics.scanned_rows < index.len());
        assert!(result.diagnostics.matched_rows > 0);
    }

    #[test]
    fn immutable_index_ranks_141400_rows_without_rebuilding_row_strings() {
        const ROW_COUNT: usize = 141_400;
        let rows: Vec<_> = (0..ROW_COUNT)
            .map(|index| {
                let name = if index + 1 == ROW_COUNT {
                    format!("needle-{index}.jpg")
                } else {
                    format!("asset-{index}.jpg")
                };
                IndexedMediaRow::new(
                    index,
                    name,
                    format!("collection/bucket-{}/item-{index}.jpg", index % 128),
                    IndexedRowMeta::default(),
                )
            })
            .collect();
        let index = MediaSearchIndex::new(SearchIndexGeneration(141_400), rows);
        let coordinator = LatestSearchRequests::default();
        let (request, token) = coordinator.begin(
            index.generation(),
            parse_query("needle-141399"),
            RankMode::Name,
            0,
        );
        let result = rank_indexed(&index, &request, &token);
        assert!(result.is_complete());
        assert_eq!(result.diagnostics.scanned_rows, ROW_COUNT);
        assert_eq!(result.diagnostics.matched_rows, 1);
        assert_eq!(
            result.hits,
            vec![RankedHit {
                index: 141_399,
                score: 1000
            }]
        );
    }

    #[test]
    fn suggestions_prefer_chips_and_dedupe() {
        let files = vec![
            "red_dress.png".to_string(),
            "red_dress.png".to_string(),
            "ruby.png".to_string(),
        ];
        let tags = vec!["red-dress".to_string(), "hero".to_string()];
        let labels = ["red", "blue"];
        let folders = vec!["renders".to_string()];
        let out = suggestions("re", &files, &tags, &labels, &folders, 8);
        assert!(matches!(out.first(), Some(Suggestion::Tag(t)) if t == "red-dress"));
        assert!(out
            .iter()
            .any(|s| matches!(s, Suggestion::Label(l) if l == "red")));
        assert!(out
            .iter()
            .any(|s| matches!(s, Suggestion::Folder(f) if f == "renders")));
        let file_count = out
            .iter()
            .filter(|s| matches!(s, Suggestion::File(_)))
            .count();
        assert!(file_count <= 2, "duplicate file names deduped");
        // Chip-prefix completion restricts to that vocabulary.
        let tag_only = suggestions("tag:h", &files, &tags, &labels, &folders, 8);
        assert_eq!(tag_only, vec![Suggestion::Tag("hero".to_string())]);
        assert_eq!(
            Suggestion::Tag("hero".to_string()).insert_text(),
            "tag:hero"
        );
    }

    #[test]
    fn indexed_suggestions_match_legacy_ordering_and_semantics() {
        let files = vec![
            "red_dress.png".to_string(),
            "red_dress.png".to_string(),
            "ruby.png".to_string(),
            "hero_clip.mp4".to_string(),
        ];
        let rows: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(row, name)| {
                let tags = match row {
                    0 => Some("red-dress, hero".to_string()),
                    1 => Some("hero".to_string()),
                    2 => Some("ruby".to_string()),
                    _ => None,
                };
                IndexedMediaRow::new(
                    row,
                    name.clone(),
                    format!("folder/{name}"),
                    IndexedRowMeta::from_owned(tags, None, None, name.ends_with(".mp4")),
                )
            })
            .collect();
        let index = MediaSearchIndex::new(SearchIndexGeneration(77), rows);
        let tags = vec![
            "red-dress".to_string(),
            "hero".to_string(),
            "ruby".to_string(),
        ];
        let labels = ["red", "blue"];
        let folders = vec!["renders".to_string(), "review".to_string()];

        for partial in ["re", "ru", "hero", "tag:h", "tag:", "label:r", "zz"] {
            for limit in [1, 2, 8] {
                let expected = suggestions(partial, &files, &tags, &labels, &folders, limit);
                let actual = suggestions_indexed_cancellable(
                    &index,
                    partial,
                    &labels,
                    &folders,
                    limit,
                    || false,
                );
                assert!(actual.is_complete());
                assert_eq!(actual.generation, SearchIndexGeneration(77));
                // WP-066: file suggestions now carry resolution identity
                // (path, source index, generation) that is legitimately
                // specific to the path that produced them — the indexed path
                // knows the real row, the legacy vocabulary path does not.
                // Parity is therefore asserted on the ordered display identity,
                // which is what the operator sees and selects.
                let display_of = |list: &[Suggestion]| {
                    list.iter()
                        .map(|item| {
                            let (kind, value) = item.display();
                            (kind, value.to_string())
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    display_of(&actual.suggestions),
                    display_of(&expected),
                    "partial={partial} limit={limit}"
                );
            }
        }
    }

    #[test]
    fn indexed_suggestion_catalog_deduplicates_case_insensitively() {
        let rows = vec![
            IndexedMediaRow::new(
                0,
                "Same.PNG",
                "a/Same.PNG",
                IndexedRowMeta::from_owned(
                    Some("Hero, red dress, ".to_string()),
                    None,
                    None,
                    false,
                ),
            ),
            IndexedMediaRow::new(
                1,
                "same.png",
                "b/same.png",
                IndexedRowMeta::from_owned(Some("hero, RED DRESS".to_string()), None, None, false),
            ),
        ];
        let index = MediaSearchIndex::new(SearchIndexGeneration(9), rows);
        assert_eq!(index.suggestion_catalog.file_names.len(), 1);
        assert_eq!(index.suggestion_catalog.tags.len(), 2);

        let files = suggestions_indexed_cancellable(&index, "same", &[], &[], 8, || false);
        assert_eq!(
            files.suggestions,
            vec![Suggestion::File(FileSuggestion { name: "Same.PNG".to_string(), path: "a/Same.PNG".to_string(), source_index: 0, generation: SearchIndexGeneration(9) })]
        );
        let tags = suggestions_indexed_cancellable(&index, "tag:", &[], &[], 8, || false);
        assert_eq!(
            tags.suggestions,
            vec![
                Suggestion::Tag("Hero".to_string()),
                Suggestion::Tag("red dress".to_string())
            ]
        );
    }

    #[test]
    fn indexed_suggestions_cancel_in_bounded_scans_without_partial_results() {
        let rows: Vec<_> = (0..4_000)
            .map(|row| {
                IndexedMediaRow::new(
                    row,
                    format!("matching-{row}.jpg"),
                    format!("folder/matching-{row}.jpg"),
                    IndexedRowMeta::default(),
                )
            })
            .collect();
        let index = MediaSearchIndex::new(SearchIndexGeneration(44), rows);
        let mut probes = 0usize;
        let result = suggestions_indexed_cancellable(&index, "matching", &[], &[], 8, || {
            probes += 1;
            probes > 6
        });
        assert_eq!(result.generation, SearchIndexGeneration(44));
        assert_eq!(
            result.diagnostics.status,
            IndexedSuggestionStatus::Cancelled
        );
        assert!(result.suggestions.is_empty());
        assert!(result.diagnostics.scanned_candidates > 0);
        assert!(result.diagnostics.scanned_candidates < index.suggestion_catalog.file_names.len());
        assert!(result.diagnostics.matched_candidates > 0);
        assert!(result.diagnostics.scanned_candidates <= 3 * SUGGESTION_CANCEL_CHECK_INTERVAL);

        let cancelled_before_start =
            suggestions_indexed_cancellable(&index, "matching", &[], &[], 8, || true);
        assert_eq!(
            cancelled_before_start.diagnostics.status,
            IndexedSuggestionStatus::Cancelled
        );
        assert_eq!(cancelled_before_start.diagnostics.scanned_candidates, 0);
        assert!(cancelled_before_start.suggestions.is_empty());
    }

    #[test]
    fn indexed_suggestions_rank_141400_pre_normalized_file_names() {
        const ROW_COUNT: usize = 141_400;
        let rows: Vec<_> = (0..ROW_COUNT)
            .map(|row| {
                let name = if row + 1 == ROW_COUNT {
                    format!("needle-{row}.jpg")
                } else {
                    format!("asset-{row}.jpg")
                };
                IndexedMediaRow::new(
                    row,
                    name,
                    format!("collection/bucket-{}/item-{row}.jpg", row % 128),
                    IndexedRowMeta::from_owned(Some("shared-tag".to_string()), None, None, false),
                )
            })
            .collect();
        let index = MediaSearchIndex::new(SearchIndexGeneration(141_400), rows);
        assert_eq!(index.suggestion_catalog.file_names.len(), ROW_COUNT);
        assert_eq!(index.suggestion_catalog.tags.len(), 1);

        let result =
            suggestions_indexed_cancellable(&index, "needle-141399", &[], &[], 8, || false);
        assert!(result.is_complete());
        assert_eq!(result.diagnostics.scanned_candidates, ROW_COUNT + 1);
        assert_eq!(result.diagnostics.matched_candidates, 1);
        // WP-066: the suggestion must resolve to the exact row that produced
        // it, not merely display the right name — a name alone cannot identify
        // a file across a 141k recursive inventory.
        let file = result.suggestions[0]
            .file()
            .expect("file suggestion carries resolution identity");
        assert_eq!(file.name, "needle-141399.jpg");
        assert_eq!(file.source_index, ROW_COUNT - 1);
        assert_eq!(
            file.path,
            format!("collection/bucket-{}/item-{}.jpg", (ROW_COUNT - 1) % 128, ROW_COUNT - 1)
        );
        assert_eq!(file.generation, SearchIndexGeneration(141_400));
    }
}
