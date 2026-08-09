//! Review queue with persisted decisions + dataset lineage (WP-016, slice 1).
//!
//! A review *session* is a machine-readable store a fleet of agents can work
//! against in parallel without positional inference or chat-tracked state:
//!
//! ```text
//! <review_root>/<session_id>/
//!   session.json    immutable init metadata (source, shard plan, schema)
//!   ledger.jsonl    append-only events: init | claim | steal | decide
//!   claims/         shard_<k>.json — existence == claimed (atomic create_new)
//! ```
//!
//! Image identity is content-addressed: `id` = sha256 of the file bytes,
//! `short_id` = first 16 hex chars. Decisions reference IDs, never tile or row
//! positions. The manifest view is always *derived* by replaying the ledger,
//! so the store cannot drift from its history and every count in
//! `review_status` is reproducible by a no-context agent.
//!
//! Slice 1 = init / claim / decide / status. Montage serving and the kohya
//! export tail land in later slices (see WP-016).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::AppConfig;

pub const REVIEW_SCHEMA_VERSION: u32 = 1;

/// Decisions a reviewer can record for an image.
pub const DECISIONS: [&str; 3] = ["accept", "reject", "hold"];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Root for review sessions: beside run outputs when a copy location is set,
/// otherwise under the workspace state dir.
pub fn review_root(config: &AppConfig) -> PathBuf {
    match &config.copy_location {
        Some(copy) => copy.join("review"),
        None => config.workspace_root.join(".facial").join("review"),
    }
}

fn session_dir(config: &AppConfig, session: &str) -> PathBuf {
    review_root(config).join(session)
}

fn ledger_path(dir: &Path) -> PathBuf {
    dir.join("ledger.jsonl")
}

// ---------------------------------------------------------------------------
// Stored records
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
struct SessionMeta {
    schema_version: u32,
    session_id: String,
    created_at: String,
    source_kind: String, // "dir" (slice 1) | "csv" | "run" (later)
    source: String,
    shards: usize,
    image_count: usize,
}

#[derive(Serialize, Deserialize, Clone)]
struct ImageRow {
    id: String,       // full sha256 hex
    short_id: String, // first 16 hex chars
    path: String,
    file_size: u64,
    shard: usize,
    /// Per-image curation metadata joined from a gate manifest (WP-019):
    /// verdict, framing, face_box, sharpness, yaw, hair flag, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
    /// Near-duplicate cluster id joined from identity_dedup (WP-018).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cluster_id: Option<String>,
}

/// One append-only ledger line.
#[derive(Serialize, Deserialize, Clone)]
struct LedgerEvent {
    schema_version: u32,
    ts: String,
    event: String, // init | claim | steal | decide
    actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shard: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Exact invocation args for lineage reproduction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    args: Option<Value>,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn append_event(dir: &Path, event: &LedgerEvent) -> Result<(), String> {
    let line = serde_json::to_string(event).map_err(|e| format!("encode event: {e}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path(dir))
        .map_err(|e| format!("open ledger: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("append ledger: {e}"))
}

fn read_events(dir: &Path) -> Result<Vec<LedgerEvent>, String> {
    let raw = fs::read_to_string(ledger_path(dir)).map_err(|e| format!("read ledger: {e}"))?;
    let mut events = Vec::new();
    for (no, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: LedgerEvent =
            serde_json::from_str(line).map_err(|e| format!("ledger line {}: {e}", no + 1))?;
        events.push(event);
    }
    Ok(events)
}

fn load_meta(dir: &Path) -> Result<SessionMeta, String> {
    let raw = fs::read_to_string(dir.join("session.json"))
        .map_err(|e| format!("read session.json: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse session.json: {e}"))
}

fn load_rows(dir: &Path) -> Result<Vec<ImageRow>, String> {
    let raw = fs::read_to_string(dir.join("images.json"))
        .map_err(|e| format!("read images.json: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse images.json: {e}"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "gif"
            )
        })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Resolve an operator/agent-supplied id (full or short, case-insensitive)
/// against the session rows. Errors on unknown AND on ambiguous short ids.
fn resolve_id<'a>(rows: &'a [ImageRow], given: &str) -> Result<&'a ImageRow, String> {
    let needle = given.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Err("empty id".to_string());
    }
    let matches: Vec<&ImageRow> = rows
        .iter()
        .filter(|r| r.id == needle || r.short_id == needle || r.id.starts_with(&needle))
        .collect();
    match matches.len() {
        0 => Err(format!("unknown image id: {given}")),
        1 => Ok(matches[0]),
        n => Err(format!(
            "ambiguous id {given}: {n} matches; use the full sha256"
        )),
    }
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

/// Initialize a review session over the images in `dir` (recursive walk).
/// `gate_manifest` joins per-image identity-gate rows as row metadata (WP-019);
/// `clusters` joins identity_dedup near-dup cluster ids (WP-018).
pub fn init_session(
    config: &AppConfig,
    dir: &str,
    shards: usize,
    gate_manifest: Option<&str>,
    clusters: Option<&str>,
) -> Result<Value, String> {
    let source = Path::new(dir);
    if !source.is_dir() {
        return Err(format!("not a directory: {dir}"));
    }
    let shards = shards.clamp(1, 64);

    // Deterministic candidate list: recursive walk, sorted paths.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut queue = vec![source.to_path_buf()];
    while let Some(current) = queue.pop() {
        let entries =
            fs::read_dir(&current).map_err(|e| format!("read dir {}: {e}", current.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else if is_supported_image(&path) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no supported images under {dir}"));
    }

    let session_id = format!("review_{}", chrono::Utc::now().format("%Y%m%d_%H%M%S"));
    let sdir = session_dir(config, &session_id);
    if sdir.exists() {
        return Err(format!("session dir already exists: {}", sdir.display()));
    }
    fs::create_dir_all(sdir.join("claims")).map_err(|e| format!("create session dir: {e}"))?;

    let mut rows = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let id = sha256_file(path)?;
        let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        rows.push(ImageRow {
            short_id: id[..16].to_string(),
            id,
            path: path.to_string_lossy().to_string(),
            file_size,
            shard: index % shards,
            metadata: None,
            cluster_id: None,
        });
    }

    // Optional joins before rows are frozen to disk.
    let mut joined_metadata = 0usize;
    let mut joined_clusters = 0usize;
    if let Some(manifest_path) = gate_manifest {
        joined_metadata = join_gate_manifest(&mut rows, manifest_path)?;
    }
    if let Some(clusters_path) = clusters {
        joined_clusters = join_clusters(&mut rows, clusters_path)?;
    }

    let meta = SessionMeta {
        schema_version: REVIEW_SCHEMA_VERSION,
        session_id: session_id.clone(),
        created_at: now_rfc3339(),
        source_kind: "dir".to_string(),
        source: dir.to_string(),
        shards,
        image_count: rows.len(),
    };
    fs::write(
        sdir.join("session.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("write session.json: {e}"))?;
    fs::write(
        sdir.join("images.json"),
        serde_json::to_string_pretty(&rows).unwrap_or_default(),
    )
    .map_err(|e| format!("write images.json: {e}"))?;

    append_event(
        &sdir,
        &LedgerEvent {
            schema_version: REVIEW_SCHEMA_VERSION,
            ts: now_rfc3339(),
            event: "init".to_string(),
            actor: "init".to_string(),
            shard: None,
            id: None,
            decision: None,
            reason: None,
            args: Some(json!({
                "dir": dir, "shards": shards, "images": rows.len(),
                "gate_manifest": gate_manifest, "clusters": clusters,
                "joined_metadata": joined_metadata, "joined_clusters": joined_clusters,
            })),
        },
    )?;

    Ok(json!({
        "session_id": session_id,
        "session_dir": sdir.to_string_lossy(),
        "image_count": rows.len(),
        "shards": shards,
        "joined_metadata": joined_metadata,
        "joined_clusters": joined_clusters,
        "schema_version": REVIEW_SCHEMA_VERSION,
    }))
}

/// Claim a shard for an actor. Claiming is atomic (create_new); a claimed
/// shard stays claimed until stolen explicitly with `steal = true`.
/// With `shard = None`, claims the lowest unclaimed shard.
pub fn claim_shard(
    config: &AppConfig,
    session: &str,
    shard: Option<usize>,
    actor: &str,
    steal: bool,
) -> Result<Value, String> {
    let dir = session_dir(config, session);
    let meta = load_meta(&dir)?;
    let actor = if actor.trim().is_empty() {
        "anonymous"
    } else {
        actor.trim()
    };

    let try_claim = |k: usize| -> Result<Option<Value>, String> {
        if k >= meta.shards {
            return Err(format!("shard {k} out of range (shards: {})", meta.shards));
        }
        let claim_path = dir.join("claims").join(format!("shard_{k}.json"));
        let body = json!({
            "shard": k, "actor": actor, "claimed_at": now_rfc3339(), "stolen": steal,
        });
        if steal {
            // Explicit takeover: rewrite + ledger the steal.
            fs::write(
                &claim_path,
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
            .map_err(|e| format!("steal shard {k}: {e}"))?;
            return Ok(Some(body));
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claim_path)
        {
            Ok(mut file) => {
                let text = serde_json::to_string_pretty(&body).unwrap_or_default();
                file.write_all(text.as_bytes())
                    .map_err(|e| format!("write claim: {e}"))?;
                Ok(Some(body))
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(err) => Err(format!("claim shard {k}: {err}")),
        }
    };

    let claimed = match shard {
        Some(k) => try_claim(k)?,
        None => {
            let mut found = None;
            for k in 0..meta.shards {
                if let Some(value) = try_claim(k)? {
                    found = Some(value);
                    break;
                }
            }
            found
        }
    };

    let Some(claim) = claimed else {
        return Err(match shard {
            Some(k) => format!("shard {k} is already claimed (use steal to take it over)"),
            None => "no unclaimed shards remain".to_string(),
        });
    };

    let claimed_shard = claim["shard"].as_u64().map(|v| v as usize);
    append_event(
        &dir,
        &LedgerEvent {
            schema_version: REVIEW_SCHEMA_VERSION,
            ts: now_rfc3339(),
            event: if steal { "steal" } else { "claim" }.to_string(),
            actor: actor.to_string(),
            shard: claimed_shard,
            id: None,
            decision: None,
            reason: None,
            args: None,
        },
    )?;

    // The actor's worklist rides along so no second roundtrip is needed.
    let rows = load_rows(&dir)?;
    let worklist: Vec<Value> = rows
        .iter()
        .filter(|r| Some(r.shard) == claimed_shard)
        .map(|r| json!({ "id": r.short_id, "path": r.path }))
        .collect();

    Ok(json!({
        "session_id": meta.session_id,
        "claim": claim,
        "worklist_len": worklist.len(),
        "worklist": worklist,
    }))
}

/// Record a decision for one image. Appends to the ledger; conflicts are
/// surfaced by `status`, never silently merged.
pub fn decide(
    config: &AppConfig,
    session: &str,
    id: &str,
    decision: &str,
    reason: &str,
    actor: &str,
) -> Result<Value, String> {
    let dir = session_dir(config, session);
    let decision = decision.trim().to_ascii_lowercase();
    if !DECISIONS.contains(&decision.as_str()) {
        return Err(format!(
            "invalid decision '{decision}' (expected accept|reject|hold)"
        ));
    }
    let rows = load_rows(&dir)?;
    let row = resolve_id(&rows, id)?;
    let actor = if actor.trim().is_empty() {
        "anonymous"
    } else {
        actor.trim()
    };

    append_event(
        &dir,
        &LedgerEvent {
            schema_version: REVIEW_SCHEMA_VERSION,
            ts: now_rfc3339(),
            event: "decide".to_string(),
            actor: actor.to_string(),
            shard: Some(row.shard),
            id: Some(row.id.clone()),
            decision: Some(decision.clone()),
            reason: if reason.trim().is_empty() {
                None
            } else {
                Some(reason.trim().to_string())
            },
            args: None,
        },
    )?;

    Ok(json!({
        "session_id": session,
        "id": row.short_id,
        "path": row.path,
        "decision": decision,
        "actor": actor,
    }))
}

/// Replay the ledger into the live state: per-decision counts, per-shard and
/// per-actor progress, conflicts, claims, and the lineage funnel.
pub fn status(config: &AppConfig, session: &str) -> Result<Value, String> {
    let dir = session_dir(config, session);
    let meta = load_meta(&dir)?;
    let rows = load_rows(&dir)?;
    let events = read_events(&dir)?;

    // Effective decision per image = last decide event; all decide events kept
    // for conflict detection (different decisions from different actors).
    #[derive(Default, Clone)]
    struct DecisionState {
        history: Vec<(String, String)>, // (actor, decision)
        effective: Option<String>,
    }
    let mut by_id: BTreeMap<String, DecisionState> = BTreeMap::new();
    let mut per_actor: BTreeMap<String, usize> = BTreeMap::new();
    for event in &events {
        if event.event != "decide" {
            continue;
        }
        let (Some(id), Some(decision)) = (&event.id, &event.decision) else {
            continue;
        };
        let state = by_id.entry(id.clone()).or_default();
        state.history.push((event.actor.clone(), decision.clone()));
        state.effective = Some(decision.clone());
        *per_actor.entry(event.actor.clone()).or_default() += 1;
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for d in DECISIONS {
        counts.insert(d, 0);
    }
    let mut per_shard: BTreeMap<usize, (usize, usize)> = BTreeMap::new(); // (decided, total)
    let mut conflicts: Vec<Value> = Vec::new();
    for row in &rows {
        let entry = per_shard.entry(row.shard).or_insert((0, 0));
        entry.1 += 1;
        if let Some(state) = by_id.get(&row.id) {
            if let Some(effective) = &state.effective {
                entry.0 += 1;
                if let Some(slot) = counts.get_mut(effective.as_str()) {
                    *slot += 1;
                }
            }
            let distinct: std::collections::BTreeSet<&String> =
                state.history.iter().map(|(_, d)| d).collect();
            if distinct.len() > 1 {
                conflicts.push(json!({
                    "id": row.short_id,
                    "path": row.path,
                    "history": state.history.iter()
                        .map(|(a, d)| json!({ "actor": a, "decision": d }))
                        .collect::<Vec<_>>(),
                    "effective": state.effective,
                }));
            }
        }
    }
    let decided: usize = counts.values().sum();
    let undecided = rows.len().saturating_sub(decided);

    // Claims snapshot.
    let mut claims = Vec::new();
    for k in 0..meta.shards {
        let path = dir.join("claims").join(format!("shard_{k}.json"));
        match fs::read_to_string(&path) {
            Ok(raw) => {
                let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                claims.push(json!({ "shard": k, "claimed": true, "claim": value }));
            }
            Err(_) => claims.push(json!({ "shard": k, "claimed": false })),
        }
    }

    let per_shard_json: Vec<Value> = per_shard
        .iter()
        .map(|(shard, (done, total))| json!({ "shard": shard, "decided": done, "total": total }))
        .collect();

    // Near-dup cluster progress (rows joined via identity_dedup, WP-018).
    let mut per_cluster: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for row in &rows {
        if let Some(cluster) = &row.cluster_id {
            let entry = per_cluster.entry(cluster.clone()).or_insert((0, 0));
            entry.1 += 1;
            if by_id
                .get(&row.id)
                .and_then(|s| s.effective.as_ref())
                .is_some()
            {
                entry.0 += 1;
            }
        }
    }
    let clusters_json: Vec<Value> = per_cluster
        .iter()
        .map(|(cluster, (done, total))| {
            json!({ "cluster_id": cluster, "decided": done, "total": total })
        })
        .collect();

    Ok(json!({
        "schema_version": REVIEW_SCHEMA_VERSION,
        "session_id": meta.session_id,
        "source": { "kind": meta.source_kind, "value": meta.source },
        "funnel": {
            "candidates": rows.len(),
            "decided": decided,
            "accepted": counts.get("accept").copied().unwrap_or(0),
            "rejected": counts.get("reject").copied().unwrap_or(0),
            "hold": counts.get("hold").copied().unwrap_or(0),
            "undecided": undecided,
        },
        "per_shard": per_shard_json,
        "per_actor": per_actor,
        "clusters": clusters_json,
        "claims": claims,
        "decision_conflicts": conflicts,
        "ledger_events": events.len(),
    }))
}

// ---------------------------------------------------------------------------
// Decisions + filters (shared by status / montage / export)
// ---------------------------------------------------------------------------

/// Effective (last-write-wins) decision per image id, from the ledger.
fn effective_decisions(events: &[LedgerEvent]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for event in events {
        if event.event != "decide" {
            continue;
        }
        if let (Some(id), Some(decision)) = (&event.id, &event.decision) {
            map.insert(id.clone(), decision.clone());
        }
    }
    map
}

/// One parsed `--filter` term. Forms:
///   `key=value`     string equality against a metadata field (or `decision`,
///                   `cluster`, `shard` pseudo-fields)
///   `key_min=x`     numeric `metadata[key] >= x`
///   `key_max=x`     numeric `metadata[key] <= x`
struct FilterTerm {
    key: String,
    value: String,
    op: FilterOp,
}

enum FilterOp {
    Eq,
    Min,
    Max,
}

fn parse_filters(specs: &[String]) -> Result<Vec<FilterTerm>, String> {
    let mut terms = Vec::new();
    for spec in specs {
        let Some((raw_key, value)) = spec.split_once('=') else {
            return Err(format!("bad filter '{spec}' (expected key=value)"));
        };
        let raw_key = raw_key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(format!("bad filter '{spec}' (empty value)"));
        }
        let (key, op) = if let Some(base) = raw_key.strip_suffix("_min") {
            (base.to_string(), FilterOp::Min)
        } else if let Some(base) = raw_key.strip_suffix("_max") {
            (base.to_string(), FilterOp::Max)
        } else {
            (raw_key, FilterOp::Eq)
        };
        if matches!(op, FilterOp::Min | FilterOp::Max) && value.parse::<f64>().is_err() {
            return Err(format!("filter '{spec}' needs a numeric value"));
        }
        terms.push(FilterTerm { key, value, op });
    }
    Ok(terms)
}

/// Does a row pass every filter term? `decision` comes from the ledger replay.
fn row_matches(row: &ImageRow, decision: Option<&str>, terms: &[FilterTerm]) -> bool {
    for term in terms {
        let pass = match term.key.as_str() {
            "decision" => {
                let effective = decision.unwrap_or("undecided");
                effective.eq_ignore_ascii_case(&term.value)
            }
            "cluster" => row
                .cluster_id
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(&term.value)),
            "shard" => term.value.parse::<usize>() == Ok(row.shard),
            key => {
                let field = row.metadata.as_ref().and_then(|m| m.get(key));
                match (&term.op, field) {
                    (FilterOp::Eq, Some(v)) => match v {
                        Value::String(s) => s.eq_ignore_ascii_case(&term.value),
                        other => other.to_string() == term.value,
                    },
                    (FilterOp::Min, Some(v)) => v
                        .as_f64()
                        .zip(term.value.parse::<f64>().ok())
                        .is_some_and(|(a, b)| a >= b),
                    (FilterOp::Max, Some(v)) => v
                        .as_f64()
                        .zip(term.value.parse::<f64>().ok())
                        .is_some_and(|(a, b)| a <= b),
                    (_, None) => false,
                }
            }
        };
        if !pass {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Joins: gate-manifest metadata (WP-019) + dedup clusters (WP-018)
// ---------------------------------------------------------------------------

/// Join per-image rows from an identity-gate `manifest.json` onto session rows
/// (matched by path string, falling back to file name when unique). Returns
/// how many rows gained metadata.
fn join_gate_manifest(rows: &mut [ImageRow], manifest_path: &str) -> Result<usize, String> {
    let raw = fs::read_to_string(manifest_path)
        .map_err(|e| format!("read gate manifest {manifest_path}: {e}"))?;
    let manifest: Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse gate manifest: {e}"))?;
    let gate_rows = manifest
        .get("rows")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "gate manifest has no rows[]".to_string())?;

    // Index by full path and by file name (only when the name is unique).
    // Gate rows key the path as "image" (CSV column is named "path"); accept
    // both so hand-built manifests also join.
    let mut by_path: BTreeMap<String, &Value> = BTreeMap::new();
    let mut by_name: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for gate_row in gate_rows {
        let path_field = gate_row
            .get("image")
            .or_else(|| gate_row.get("path"))
            .and_then(|v| v.as_str());
        if let Some(p) = path_field {
            by_path.insert(p.to_string(), gate_row);
            if let Some(name) = Path::new(p).file_name().and_then(|n| n.to_str()) {
                by_name.entry(name.to_string()).or_default().push(gate_row);
            }
        }
    }

    let mut joined = 0usize;
    for row in rows.iter_mut() {
        let found = by_path.get(row.path.as_str()).copied().or_else(|| {
            Path::new(&row.path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|name| by_name.get(name))
                .and_then(|hits| if hits.len() == 1 { Some(hits[0]) } else { None })
        });
        if let Some(gate_row) = found {
            row.metadata = Some(gate_row.clone());
            joined += 1;
        }
    }
    Ok(joined)
}

/// Join cluster ids from an `identity_dedup` clusters JSON (`groups[]` with
/// `cluster_id` + `members[].path`). Returns how many rows gained a cluster.
fn join_clusters(rows: &mut [ImageRow], clusters_path: &str) -> Result<usize, String> {
    let raw = fs::read_to_string(clusters_path)
        .map_err(|e| format!("read clusters {clusters_path}: {e}"))?;
    let doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse clusters: {e}"))?;
    let groups = doc
        .get("groups")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "clusters file has no groups[]".to_string())?;

    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    for group in groups {
        let Some(cluster_id) = group.get("cluster_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(members) = group.get("members").and_then(|v| v.as_array()) {
            for member in members {
                if let Some(p) = member.get("path").and_then(|v| v.as_str()) {
                    by_path.insert(p.to_string(), cluster_id.to_string());
                }
            }
        }
    }
    let mut joined = 0usize;
    for row in rows.iter_mut() {
        if let Some(cluster) = by_path.get(row.path.as_str()) {
            row.cluster_id = Some(cluster.clone());
            joined += 1;
        }
    }
    Ok(joined)
}

// ---------------------------------------------------------------------------
// Montage (WP-016 slice 2)
// ---------------------------------------------------------------------------

const MONTAGE_COLS: usize = 6;
const MONTAGE_ROWS: usize = 5;
const MONTAGE_TILE: u32 = 256;
const MONTAGE_GAP: u32 = 6;
const MONTAGE_MARGIN: u32 = 8;

/// Render one montage page for a session as `montages/montage_<scope>_<page>.png`
/// plus a tile map (`.map.json`) keyed by image ID — never by position alone.
/// Tiles honor `--filter` terms; `face_crop` uses joined gate face boxes and
/// falls back (flagged in the map) when a row has no face geometry.
pub fn montage(
    config: &AppConfig,
    session: &str,
    shard: Option<usize>,
    page: usize,
    face_crop: bool,
    filter_specs: &[String],
) -> Result<Value, String> {
    let dir = session_dir(config, session);
    let _meta = load_meta(&dir)?;
    let rows = load_rows(&dir)?;
    let events = read_events(&dir)?;
    let decisions = effective_decisions(&events);
    let terms = parse_filters(filter_specs)?;

    // Scope: shard subset (if given) -> filters -> stable order -> page.
    let mut scoped: Vec<&ImageRow> = rows
        .iter()
        .filter(|r| shard.is_none_or(|k| r.shard == k))
        .filter(|r| row_matches(r, decisions.get(&r.id).map(|s| s.as_str()), &terms))
        .collect();
    // Near-dup clusters tile together ("these N are the same moment — pick
    // one"); unclustered rows follow in stable path order.
    scoped.sort_by(|a, b| match (&a.cluster_id, &b.cluster_id) {
        (Some(ca), Some(cb)) => ca.cmp(cb).then_with(|| a.path.cmp(&b.path)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.path.cmp(&b.path),
    });
    if scoped.is_empty() {
        return Err("no images match the requested shard/filters".to_string());
    }
    let per_page = MONTAGE_COLS * MONTAGE_ROWS;
    let pages = scoped.len().div_ceil(per_page);
    if page >= pages {
        return Err(format!("page {page} out of range (pages: {pages})"));
    }
    let slice = &scoped[page * per_page..(page * per_page + per_page).min(scoped.len())];

    let canvas_w =
        MONTAGE_MARGIN * 2 + (MONTAGE_TILE + MONTAGE_GAP) * MONTAGE_COLS as u32 - MONTAGE_GAP;
    let canvas_h =
        MONTAGE_MARGIN * 2 + (MONTAGE_TILE + MONTAGE_GAP) * MONTAGE_ROWS as u32 - MONTAGE_GAP;
    let mut canvas =
        image::RgbaImage::from_pixel(canvas_w, canvas_h, image::Rgba([235, 232, 222, 255]));

    let mut tiles_json = Vec::new();
    for (i, row) in slice.iter().enumerate() {
        let col = (i % MONTAGE_COLS) as u32;
        let grid_row = (i / MONTAGE_COLS) as u32;
        let cell_x = MONTAGE_MARGIN + col * (MONTAGE_TILE + MONTAGE_GAP);
        let cell_y = MONTAGE_MARGIN + grid_row * (MONTAGE_TILE + MONTAGE_GAP);

        let mut tile_error: Option<String> = None;
        let mut face_cropped = false;
        match image::open(&row.path) {
            Ok(img) => {
                // Optional face crop from joined gate geometry (box + 30% margin).
                let img = if face_crop {
                    match face_box_of(row) {
                        Some([bx, by, bw, bh]) => {
                            let margin_w = bw * 0.3;
                            let margin_h = bh * 0.3;
                            let x = (bx - margin_w).max(0.0) as u32;
                            let y = (by - margin_h).max(0.0) as u32;
                            let w = ((bw + 2.0 * margin_w) as u32)
                                .max(1)
                                .min(img.width() - x.min(img.width() - 1));
                            let h = ((bh + 2.0 * margin_h) as u32)
                                .max(1)
                                .min(img.height() - y.min(img.height() - 1));
                            face_cropped = true;
                            img.crop_imm(x, y, w.max(1), h.max(1))
                        }
                        None => img,
                    }
                } else {
                    img
                };
                let thumb = img.thumbnail(MONTAGE_TILE, MONTAGE_TILE).to_rgba8();
                let off_x = cell_x + (MONTAGE_TILE - thumb.width()) / 2;
                let off_y = cell_y + (MONTAGE_TILE - thumb.height()) / 2;
                image::imageops::overlay(&mut canvas, &thumb, off_x as i64, off_y as i64);
            }
            Err(err) => {
                // Honest failure tile: brick fill, error recorded in the map.
                for y in cell_y..cell_y + MONTAGE_TILE {
                    for x in cell_x..cell_x + MONTAGE_TILE {
                        canvas.put_pixel(x, y, image::Rgba([140, 58, 58, 255]));
                    }
                }
                tile_error = Some(format!("{err}"));
            }
        }

        tiles_json.push(json!({
            "tile": i,
            "row": grid_row,
            "col": col,
            "x": cell_x, "y": cell_y, "w": MONTAGE_TILE, "h": MONTAGE_TILE,
            "id": row.short_id,
            "full_id": row.id,
            "path": row.path,
            "shard": row.shard,
            "decision": decisions.get(&row.id),
            "cluster_id": row.cluster_id,
            "face_crop": face_cropped,
            "error": tile_error,
        }));
    }

    let montage_dir = dir.join("montages");
    fs::create_dir_all(&montage_dir).map_err(|e| format!("create montages dir: {e}"))?;
    let scope = match shard {
        Some(k) => format!("shard{k}"),
        None => "all".to_string(),
    };
    let png_path = montage_dir.join(format!("montage_{scope}_p{page}.png"));
    let map_path = montage_dir.join(format!("montage_{scope}_p{page}.map.json"));
    canvas
        .save(&png_path)
        .map_err(|e| format!("save montage png: {e}"))?;
    let map = json!({
        "schema_version": REVIEW_SCHEMA_VERSION,
        "session_id": session,
        "scope": scope,
        "page": page,
        "pages": pages,
        "grid": { "cols": MONTAGE_COLS, "rows": MONTAGE_ROWS, "tile": MONTAGE_TILE },
        "face_crop_requested": face_crop,
        "filters": filter_specs,
        "tiles": tiles_json,
    });
    fs::write(
        &map_path,
        serde_json::to_string_pretty(&map).unwrap_or_default(),
    )
    .map_err(|e| format!("write montage map: {e}"))?;

    Ok(json!({
        "session_id": session,
        "png": png_path.to_string_lossy(),
        "map": map_path.to_string_lossy(),
        "page": page,
        "pages": pages,
        "tiles": slice.len(),
        "matched": scoped.len(),
    }))
}

fn face_box_of(row: &ImageRow) -> Option<[f32; 4]> {
    let face_box = row.metadata.as_ref()?.get("face_box")?;
    let get = |k: &str| face_box.get(k).and_then(|v| v.as_f64()).map(|v| v as f32);
    Some([get("x")?, get("y")?, get("w")?, get("h")?])
}

// ---------------------------------------------------------------------------
// Export (WP-016 slice 3)
// ---------------------------------------------------------------------------

/// Export accepted images as a kohya-style training folder:
/// `<out>/<repeats>_<name>/<image files>` plus `dataset_manifest.json` with the
/// full lineage funnel. Hashes are verified before copy; mismatches/missing
/// files are reported explicitly and never copied. Undecided images block the
/// export unless `allow_partial`.
pub fn export_kohya(
    config: &AppConfig,
    session: &str,
    out: &str,
    repeats: usize,
    name: &str,
    allow_partial: bool,
) -> Result<Value, String> {
    let dir = session_dir(config, session);
    let meta = load_meta(&dir)?;
    let rows = load_rows(&dir)?;
    let events = read_events(&dir)?;
    let decisions = effective_decisions(&events);

    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\', ' ']) {
        return Err("export requires --name (no spaces or slashes)".to_string());
    }
    let repeats = repeats.clamp(1, 10_000);

    let undecided: Vec<&ImageRow> = rows
        .iter()
        .filter(|r| !decisions.contains_key(&r.id))
        .collect();
    if !undecided.is_empty() && !allow_partial {
        return Err(format!(
            "{} images are undecided; decide them or pass allow_partial",
            undecided.len()
        ));
    }

    let accepted: Vec<&ImageRow> = rows
        .iter()
        .filter(|r| decisions.get(&r.id).map(|d| d.as_str()) == Some("accept"))
        .collect();
    if accepted.is_empty() {
        return Err("no accepted images to export".to_string());
    }

    let dataset_dir = Path::new(out).join(format!("{repeats}_{name}"));
    fs::create_dir_all(&dataset_dir).map_err(|e| format!("create dataset dir: {e}"))?;

    let mut exported = Vec::new();
    let mut problems = Vec::new();
    let mut used_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in &accepted {
        let src = Path::new(&row.path);
        // Verify content identity before copying: never export wrong bytes.
        match sha256_file(src) {
            Ok(hash) if hash == row.id => {
                let base = src
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image")
                    .to_string();
                let dest_name = if used_names.contains(&base) {
                    format!("{}_{base}", row.short_id)
                } else {
                    base.clone()
                };
                used_names.insert(dest_name.clone());
                let dest = dataset_dir.join(&dest_name);
                match fs::copy(src, &dest) {
                    Ok(_) => exported.push(json!({
                        "id": row.short_id,
                        "src": row.path,
                        "dest": dest.to_string_lossy(),
                        "sha256": row.id,
                    })),
                    Err(err) => problems.push(json!({
                        "id": row.short_id, "path": row.path,
                        "problem": format!("copy failed: {err}"),
                    })),
                }
            }
            Ok(_) => problems.push(json!({
                "id": row.short_id, "path": row.path,
                "problem": "content changed since session init (sha256 mismatch)",
            })),
            Err(err) => problems.push(json!({
                "id": row.short_id, "path": row.path,
                "problem": format!("unreadable: {err}"),
            })),
        }
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for d in DECISIONS {
        counts.insert(d, 0);
    }
    for decision in decisions.values() {
        if let Some(slot) = counts.get_mut(decision.as_str()) {
            *slot += 1;
        }
    }

    let funnel = json!({
        "source": { "kind": meta.source_kind, "value": meta.source },
        "candidates": rows.len(),
        "accepted": counts.get("accept").copied().unwrap_or(0),
        "rejected": counts.get("reject").copied().unwrap_or(0),
        "hold": counts.get("hold").copied().unwrap_or(0),
        "undecided": undecided.len(),
        "exported": exported.len(),
        "export_problems": problems.len(),
    });
    let manifest = json!({
        "schema_version": REVIEW_SCHEMA_VERSION,
        "layout_version": "kohya_imagedir_v1",
        "session_id": meta.session_id,
        "exported_at": now_rfc3339(),
        "dataset_dir": dataset_dir.to_string_lossy(),
        "repeats": repeats,
        "name": name,
        "allow_partial": allow_partial,
        "funnel": funnel,
        "files": exported,
        "problems": problems,
    });
    let manifest_path = Path::new(out).join("dataset_manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("write dataset manifest: {e}"))?;

    append_event(
        &dir,
        &LedgerEvent {
            schema_version: REVIEW_SCHEMA_VERSION,
            ts: now_rfc3339(),
            event: "export".to_string(),
            actor: "export".to_string(),
            shard: None,
            id: None,
            decision: None,
            reason: None,
            args: Some(json!({
                "out": out, "repeats": repeats, "name": name,
                "allow_partial": allow_partial,
                "exported": manifest["funnel"]["exported"],
                "problems": manifest["funnel"]["export_problems"],
                "manifest": manifest_path.to_string_lossy(),
            })),
        },
    )?;

    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_config(root: &Path) -> AppConfig {
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
            copy_location: None,
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

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "facial_review_{label}_{}",
            Uuid::new_v4().to_string().replace('-', "_")
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn seed_images(dir: &Path, count: usize) {
        fs::create_dir_all(dir).unwrap();
        for i in 0..count {
            // Distinct bytes -> distinct sha256 ids; extension makes them "images".
            fs::write(
                dir.join(format!("img_{i:03}.png")),
                format!("fake-image-{i}"),
            )
            .unwrap();
        }
    }

    fn session_id_of(init: &Value) -> String {
        init["session_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn init_decide_status_funnel() {
        let root = temp_root("funnel");
        let config = test_config(&root);
        let src = root.join("src_images");
        seed_images(&src, 5);

        let init = init_session(&config, src.to_str().unwrap(), 2, None, None).unwrap();
        assert_eq!(init["image_count"], 5);
        let session = session_id_of(&init);

        // Decide three images via the claim worklist (stable short ids).
        let claim = claim_shard(&config, &session, Some(0), "agent-a", false).unwrap();
        let worklist = claim["worklist"].as_array().unwrap();
        assert!(!worklist.is_empty());
        let first = worklist[0]["id"].as_str().unwrap();
        decide(
            &config,
            &session,
            first,
            "accept",
            "sharp, on-identity",
            "agent-a",
        )
        .unwrap();

        let st = status(&config, &session).unwrap();
        assert_eq!(st["funnel"]["candidates"], 5);
        assert_eq!(st["funnel"]["accepted"], 1);
        assert_eq!(st["funnel"]["undecided"], 4);
        assert_eq!(st["decision_conflicts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ids_are_stable_across_sessions() {
        let root = temp_root("stable_ids");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_images(&src, 3);

        let a = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        // Different session, same files -> same ids.
        std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct session_id stamp
        let b = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let dir_a = session_dir(&config, a["session_id"].as_str().unwrap());
        let dir_b = session_dir(&config, b["session_id"].as_str().unwrap());
        let rows_a = load_rows(&dir_a).unwrap();
        let rows_b = load_rows(&dir_b).unwrap();
        let ids_a: Vec<_> = rows_a.iter().map(|r| r.id.clone()).collect();
        let ids_b: Vec<_> = rows_b.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn claims_are_exclusive_until_stolen() {
        let root = temp_root("claims");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_images(&src, 4);
        let init = init_session(&config, src.to_str().unwrap(), 2, None, None).unwrap();
        let session = session_id_of(&init);

        claim_shard(&config, &session, Some(0), "agent-a", false).unwrap();
        let err = claim_shard(&config, &session, Some(0), "agent-b", false).unwrap_err();
        assert!(err.contains("already claimed"));

        // Auto-claim picks the remaining shard.
        let auto = claim_shard(&config, &session, None, "agent-b", false).unwrap();
        assert_eq!(auto["claim"]["shard"], 1);
        // Nothing left.
        let none = claim_shard(&config, &session, None, "agent-c", false).unwrap_err();
        assert!(none.contains("no unclaimed"));

        // Steal is explicit and logged.
        let stolen = claim_shard(&config, &session, Some(0), "agent-c", true).unwrap();
        assert_eq!(stolen["claim"]["actor"], "agent-c");
        let st = status(&config, &session).unwrap();
        let claims = st["claims"].as_array().unwrap();
        assert!(claims.iter().all(|c| c["claimed"] == true));
    }

    #[test]
    fn concurrent_claims_one_winner() {
        let root = temp_root("race");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_images(&src, 2);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);

        let mut handles = Vec::new();
        for i in 0..8 {
            let config = config.clone();
            let session = session.clone();
            handles.push(std::thread::spawn(move || {
                claim_shard(&config, &session, Some(0), &format!("agent-{i}"), false).is_ok()
            }));
        }
        let wins: usize = handles
            .into_iter()
            .map(|h| h.join().unwrap() as usize)
            .sum();
        assert_eq!(wins, 1, "exactly one concurrent claimant may win");
    }

    #[test]
    fn conflicting_decisions_are_surfaced_not_lost() {
        let root = temp_root("conflict");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_images(&src, 2);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);
        let claim = claim_shard(&config, &session, Some(0), "agent-a", false).unwrap();
        let id = claim["worklist"][0]["id"].as_str().unwrap().to_string();

        decide(&config, &session, &id, "accept", "good", "agent-a").unwrap();
        decide(&config, &session, &id, "reject", "pink wig", "agent-b").unwrap();

        let st = status(&config, &session).unwrap();
        let conflicts = st["decision_conflicts"].as_array().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["effective"], "reject");
        assert_eq!(conflicts[0]["history"].as_array().unwrap().len(), 2);
        // Effective count reflects last-write-wins.
        assert_eq!(st["funnel"]["rejected"], 1);
        assert_eq!(st["funnel"]["accepted"], 0);
    }

    /// Seed REAL decodable PNGs (distinct solid colors -> distinct hashes).
    fn seed_real_images(dir: &Path, count: usize) {
        fs::create_dir_all(dir).unwrap();
        for i in 0..count {
            let shade = 30 + (i as u8 * 13);
            let img = image::RgbImage::from_pixel(64, 48, image::Rgb([shade, 90, 160]));
            img.save(dir.join(format!("real_{i:02}.png"))).unwrap();
        }
    }

    #[test]
    fn montage_renders_png_with_id_keyed_map() {
        let root = temp_root("montage");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_real_images(&src, 7);
        let init = init_session(&config, src.to_str().unwrap(), 2, None, None).unwrap();
        let session = session_id_of(&init);

        let out = montage(&config, &session, None, 0, false, &[]).unwrap();
        assert_eq!(out["tiles"], 7);
        assert_eq!(out["pages"], 1);
        let png = PathBuf::from(out["png"].as_str().unwrap());
        let map_path = PathBuf::from(out["map"].as_str().unwrap());
        // The PNG is a real, decodable image of the expected canvas size.
        let decoded = image::open(&png).unwrap();
        assert!(decoded.width() > 1000 && decoded.height() > 1000);
        // The map keys tiles by image id, with grid coordinates.
        let map: Value = serde_json::from_str(&fs::read_to_string(&map_path).unwrap()).unwrap();
        let tiles = map["tiles"].as_array().unwrap();
        assert_eq!(tiles.len(), 7);
        assert!(tiles.iter().all(|t| t["id"].as_str().unwrap().len() == 16));
        assert_eq!(tiles[6]["row"], 1); // 7th tile wraps to second grid row
        assert_eq!(tiles[6]["col"], 0);
    }

    #[test]
    fn montage_decision_filter_and_paging() {
        let root = temp_root("montage_filter");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_real_images(&src, 4);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);
        let claim = claim_shard(&config, &session, Some(0), "a", false).unwrap();
        let id0 = claim["worklist"][0]["id"].as_str().unwrap().to_string();
        decide(&config, &session, &id0, "accept", "", "a").unwrap();

        // Only undecided images.
        let out = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["decision=undecided".to_string()],
        )
        .unwrap();
        assert_eq!(out["matched"], 3);
        // Only the accepted one.
        let out = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["decision=accept".to_string()],
        )
        .unwrap();
        assert_eq!(out["matched"], 1);
        // No matches errors honestly.
        let err = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["decision=hold".to_string()],
        )
        .unwrap_err();
        assert!(err.contains("no images match"));
        // Page out of range errors.
        let err = montage(&config, &session, None, 9, false, &[]).unwrap_err();
        assert!(err.contains("out of range"));
    }

    #[test]
    fn export_kohya_layout_and_guards() {
        let root = temp_root("export");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_real_images(&src, 3);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);
        let claim = claim_shard(&config, &session, Some(0), "a", false).unwrap();
        let ids: Vec<String> = claim["worklist"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap().to_string())
            .collect();

        // Undecided images block the export.
        let out_dir = root.join("dataset");
        let err = export_kohya(
            &config,
            &session,
            out_dir.to_str().unwrap(),
            10,
            "leeseo",
            false,
        )
        .unwrap_err();
        assert!(err.contains("undecided"));

        decide(&config, &session, &ids[0], "accept", "", "a").unwrap();
        decide(&config, &session, &ids[1], "reject", "dupe", "a").unwrap();
        decide(&config, &session, &ids[2], "accept", "", "a").unwrap();

        let manifest = export_kohya(
            &config,
            &session,
            out_dir.to_str().unwrap(),
            10,
            "leeseo",
            false,
        )
        .unwrap();
        assert_eq!(manifest["funnel"]["exported"], 2);
        assert_eq!(manifest["funnel"]["rejected"], 1);
        assert_eq!(manifest["layout_version"], "kohya_imagedir_v1");
        // kohya folder layout: <out>/<repeats>_<name>/ with exactly the accepted files.
        let dataset_dir = out_dir.join("10_leeseo");
        let files: Vec<_> = fs::read_dir(&dataset_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 2);
        assert!(out_dir.join("dataset_manifest.json").exists());
        // Name guard.
        let err = export_kohya(
            &config,
            &session,
            out_dir.to_str().unwrap(),
            10,
            "bad name",
            false,
        )
        .unwrap_err();
        assert!(err.contains("no spaces"));
    }

    #[test]
    fn export_detects_content_drift() {
        let root = temp_root("export_drift");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_real_images(&src, 2);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);
        let claim = claim_shard(&config, &session, Some(0), "a", false).unwrap();
        let ids: Vec<String> = claim["worklist"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap().to_string())
            .collect();
        decide(&config, &session, &ids[0], "accept", "", "a").unwrap();
        decide(&config, &session, &ids[1], "accept", "", "a").unwrap();

        // Mutate one source file after init: its sha no longer matches.
        let mutated = claim["worklist"][0]["path"].as_str().unwrap();
        image::RgbImage::from_pixel(64, 48, image::Rgb([1, 2, 3]))
            .save(mutated)
            .unwrap();

        let out_dir = root.join("dataset");
        let manifest =
            export_kohya(&config, &session, out_dir.to_str().unwrap(), 5, "x", false).unwrap();
        assert_eq!(manifest["funnel"]["exported"], 1);
        assert_eq!(manifest["funnel"]["export_problems"], 1);
        let problem = &manifest["problems"][0];
        assert!(problem["problem"]
            .as_str()
            .unwrap()
            .contains("content changed"));
    }

    #[test]
    fn gate_manifest_and_cluster_joins() {
        let root = temp_root("joins");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_real_images(&src, 2);
        let paths: Vec<String> = {
            let mut p: Vec<_> = fs::read_dir(&src)
                .unwrap()
                .flatten()
                .map(|e| e.path().to_string_lossy().to_string())
                .collect();
            p.sort();
            p
        };

        // Fake gate manifest with face geometry + WP-019 columns for row 0.
        let gate_manifest = root.join("gate_manifest.json");
        fs::write(
            &gate_manifest,
            serde_json::to_string_pretty(&json!({
                "rows": [{
                    "image": paths[0],
                    "verdict": "match",
                    "framing": "close-up",
                    "face_box": { "x": 8.0, "y": 6.0, "w": 30.0, "h": 30.0 },
                    "face_crop_sharpness": 123.5,
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        // Fake dedup clusters covering both rows.
        let clusters = root.join("clusters.json");
        fs::write(
            &clusters,
            serde_json::to_string_pretty(&json!({
                "groups": [{
                    "cluster_id": "c0",
                    "members": [ { "path": paths[0] }, { "path": paths[1] } ]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let init = init_session(
            &config,
            src.to_str().unwrap(),
            1,
            Some(gate_manifest.to_str().unwrap()),
            Some(clusters.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(init["joined_metadata"], 1);
        assert_eq!(init["joined_clusters"], 2);
        let session = session_id_of(&init);

        // Metadata filters work end to end.
        let out = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["framing=close-up".to_string()],
        )
        .unwrap();
        assert_eq!(out["matched"], 1);
        let out = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["face_crop_sharpness_min=100".to_string()],
        )
        .unwrap();
        assert_eq!(out["matched"], 1);
        let err = montage(
            &config,
            &session,
            None,
            0,
            false,
            &["face_crop_sharpness_min=999".to_string()],
        )
        .unwrap_err();
        assert!(err.contains("no images match"));

        // Face-crop montage: row 0 has geometry (cropped), cluster filter hits both.
        let out = montage(
            &config,
            &session,
            None,
            0,
            true,
            &["cluster=c0".to_string()],
        )
        .unwrap();
        assert_eq!(out["tiles"], 2);
        let map: Value =
            serde_json::from_str(&fs::read_to_string(out["map"].as_str().unwrap()).unwrap())
                .unwrap();
        let tiles = map["tiles"].as_array().unwrap();
        let cropped: Vec<bool> = tiles
            .iter()
            .map(|t| t["face_crop"].as_bool().unwrap())
            .collect();
        assert!(cropped.contains(&true) && cropped.contains(&false));
    }

    #[test]
    fn decide_rejects_unknown_and_ambiguous_ids() {
        let root = temp_root("ids");
        let config = test_config(&root);
        let src = root.join("imgs");
        seed_images(&src, 2);
        let init = init_session(&config, src.to_str().unwrap(), 1, None, None).unwrap();
        let session = session_id_of(&init);

        let err = decide(&config, &session, "deadbeef", "accept", "", "a").unwrap_err();
        assert!(err.contains("unknown image id"));
        let err = decide(&config, &session, "zz", "accept", "", "a").unwrap_err();
        assert!(err.contains("unknown image id"));
        let err = decide(&config, &session, "", "accept", "", "a").unwrap_err();
        assert!(err.contains("empty id"));
    }
}
