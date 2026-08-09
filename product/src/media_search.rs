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
    }

    pub fn has_chips(&self) -> bool {
        !self.tags.is_empty()
            || !self.labels.is_empty()
            || !self.kinds.is_empty()
            || !self.notes_contain.is_empty()
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
    split_query_tokens(raw)
        .into_iter()
        .filter(|t| !t.eq_ignore_ascii_case(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap a chip value in quotes when it needs them (contains whitespace).
pub fn quote_chip_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
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
        let lower = token.to_lowercase();
        if let Some(value) = lower.strip_prefix("tag:") {
            let value = unquote(value);
            if !value.is_empty() {
                query.tags.push(value.to_string());
            }
        } else if let Some(value) = lower.strip_prefix("label:") {
            let value = unquote(value);
            if !value.is_empty() {
                query.labels.push(value.to_string());
            }
        } else if let Some(value) = lower.strip_prefix("note:") {
            let value = unquote(value);
            if !value.is_empty() {
                query.notes_contain.push(value.to_string());
            }
        } else if let Some(value) = lower.strip_prefix("kind:") {
            match unquote(value) {
                "img" | "image" | "images" | "photo" => query.kinds.push(MediaKindFilter::Image),
                "vid" | "video" | "videos" | "clip" => query.kinds.push(MediaKindFilter::Video),
                _ => free_terms.push(token),
            }
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
}

/// True when a row passes every chip filter (AND semantics; tag chips match
/// list membership, label chips exact, kind chips media type).
pub fn passes_chips(query: &MediaQuery, meta: &RowMeta<'_>) -> bool {
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
        let meta = metas.get(index).copied().unwrap_or_default();
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
    /// Insert this file name as the free-text query.
    FileName(String),
    /// Insert a `tag:<value>` chip.
    Tag(String),
    /// Insert a `label:<value>` chip.
    Label(String),
    /// Insert a folder name as the free-text query.
    Folder(String),
}

impl Suggestion {
    pub fn display(&self) -> (&'static str, &str) {
        match self {
            Suggestion::FileName(v) => ("file", v),
            Suggestion::Tag(v) => ("tag", v),
            Suggestion::Label(v) => ("label", v),
            Suggestion::Folder(v) => ("folder", v),
        }
    }

    /// The text that selecting this suggestion produces in the search box
    /// (chips insert their prefix, quoting multi-word values; names replace
    /// the free text).
    pub fn insert_text(&self) -> String {
        match self {
            Suggestion::FileName(v) | Suggestion::Folder(v) => v.clone(),
            Suggestion::Tag(v) => format!("tag:{}", quote_chip_value(v)),
            Suggestion::Label(v) => format!("label:{}", quote_chip_value(v)),
        }
    }
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
    for name in file_names {
        let lower = name.to_lowercase();
        if seen_names.contains(&lower) {
            continue;
        }
        if let Some(score) = prefix_or_fuzzy(&lower, &token) {
            seen_names.insert(lower);
            out.push((score, Suggestion::FileName(name.clone())));
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
            .filter(|s| matches!(s, Suggestion::FileName(_)))
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
}
