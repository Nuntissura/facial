//! WP-077 isolated embedded SurrealDB ledger.
//!
//! This module deliberately has no dependency on `media_db`. It accepts only
//! bounded worker observations and creates all persistent records itself.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use surrealdb::engine::local::SurrealKv;
use surrealdb::Surreal;

const ANCHOR_NAME: &str = "timeline-maintenance.yaml";
const NAMESPACE: &str = "facial";
const DATABASE: &str = "timeline_ledger";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
struct LedgerPaths {
    project_root: PathBuf,
    anchor: PathBuf,
    state_root: PathBuf,
    database_root: PathBuf,
    captures_root: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct CapturedSource {
    pub proposal_id: String,
    pub job_id: String,
    pub source_id: String,
    pub source_kind: String,
    pub state: String,
    pub canonical_url: String,
    pub content_sha256: String,
    pub capture_path: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProposalRow {
    proposal_id: String,
    job_id: String,
    source_id: String,
    source_kind: String,
    state: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CaptureRow {
    source_id: String,
    canonical_url: String,
    content_sha256: String,
    capture_path: String,
    byte_length: u64,
}

impl LedgerPaths {
    fn discover(start: &Path) -> Result<Self, String> {
        let start = fs::canonicalize(start)
            .map_err(|error| format!("project root {}: {error}", start.display()))?;
        let root = if start.is_file() {
            start
                .parent()
                .ok_or_else(|| "project-root has no parent".to_string())?
        } else {
            start.as_path()
        };
        for candidate in std::iter::successors(Some(root), |path| path.parent()) {
            let anchor = candidate.join(ANCHOR_NAME);
            if anchor.is_file() {
                let state_root = candidate.join(".facial").join("timeline-ledger");
                return Ok(Self {
                    project_root: candidate.to_path_buf(),
                    anchor,
                    database_root: state_root.join("surrealdb"),
                    captures_root: state_root.join("captures"),
                    state_root,
                });
            }
        }
        Err(format!(
            "no {ANCHOR_NAME} found at or above {}",
            start.display()
        ))
    }

    fn ensure_dirs(&self) -> Result<(), String> {
        fs::create_dir_all(&self.database_root)
            .map_err(|error| format!("create {}: {error}", self.database_root.display()))?;
        fs::create_dir_all(&self.captures_root)
            .map_err(|error| format!("create {}: {error}", self.captures_root.display()))
    }
}

/// `facial-cli timeline-ledger init|doctor|status|propose-source --project-root PATH ...`
pub fn run_cli(args: &[String]) -> Result<Value, String> {
    let (command, rest) = args
        .split_first()
        .ok_or_else(|| "missing command; use init|doctor|status|propose-source".to_string())?;
    let flags = parse_flags(rest)?;
    let root = flags
        .get("project-root")
        .ok_or_else(|| "--project-root PATH is required".to_string())?;
    let paths = LedgerPaths::discover(Path::new(root))?;

    match command.as_str() {
        "init" => run_async(init(&paths)),
        "doctor" => run_async(doctor(&paths)),
        "status" => run_async(status(&paths)),
        "propose-source" => {
            let job_id = required(&flags, "job")?;
            let url = required(&flags, "url")?;
            let source_kind = required(&flags, "source-kind")?;
            validate_job_id(job_id)?;
            validate_source_kind(source_kind)?;
            run_async(propose_source(&paths, job_id, url, source_kind))
        }
        other => Err(format!(
            "unknown timeline-ledger command {other}; use init|doctor|status|propose-source"
        )),
    }
}

pub(crate) fn discover_project_root(start: &Path) -> Result<PathBuf, String> {
    Ok(LedgerPaths::discover(start)?.project_root)
}

pub(crate) fn load_captured_sources(start: &Path) -> Result<Vec<CapturedSource>, String> {
    let paths = LedgerPaths::discover(start)?;
    run_async(load_captured_sources_async(&paths))
}

fn parse_flags(args: &[String]) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut result = std::collections::BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {flag}"))?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if result.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!("{flag} was supplied more than once"));
        }
        index += 2;
    }
    Ok(result)
}

fn required<'a>(
    flags: &'a std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<&'a str, String> {
    flags
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("--{key} is required"))
}

fn validate_job_id(value: &str) -> Result<(), String> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("--job must be 8..128 uppercase letters, digits, or hyphens".to_string());
    }
    Ok(())
}

fn validate_source_kind(value: &str) -> Result<(), String> {
    match value {
        "official-group" | "official-agency" | "broadcaster" | "venue" | "brand" | "platform" => {
            Ok(())
        }
        _ => Err(
            "--source-kind must be official-group|official-agency|broadcaster|venue|brand|platform"
                .to_string(),
        ),
    }
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create timeline-ledger runtime: {error}"))?
        .block_on(future)
}

async fn open(paths: &LedgerPaths) -> Result<Surreal<surrealdb::engine::local::Db>, String> {
    paths.ensure_dirs()?;
    let db = Surreal::new::<SurrealKv>(paths.database_root.clone())
        .await
        .map_err(|error| format!("open SurrealDB: {error}"))?;
    db.use_ns(NAMESPACE)
        .use_db(DATABASE)
        .await
        .map_err(|error| format!("select ledger namespace: {error}"))?;
    Ok(db)
}

async fn initialize_schema(db: &Surreal<surrealdb::engine::local::Db>) -> Result<(), String> {
    db.query(
        "
        DEFINE TABLE ledger_meta SCHEMALESS;
        DEFINE TABLE source_capture SCHEMALESS;
        DEFINE TABLE source_proposal SCHEMALESS;
        DEFINE TABLE rejection_audit SCHEMALESS;
        UPSERT ledger_meta:schema SET version = $version, engine = 'surrealdb', namespace = $namespace, database = $database;
        ",
    )
    .bind(("version", SCHEMA_VERSION))
    .bind(("namespace", NAMESPACE))
    .bind(("database", DATABASE))
    .await
    .map_err(|error| format!("apply ledger schema: {error}"))?;
    Ok(())
}

async fn init(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    Ok(path_receipt("initialized", paths))
}

async fn doctor(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let mut response = db
        .query("SELECT version, engine, namespace, database FROM ledger_meta:schema;")
        .await
        .map_err(|error| format!("ledger doctor query: {error}"))?;
    let meta: Option<Value> = response
        .take(0)
        .map_err(|error| format!("ledger doctor decode: {error}"))?;
    Ok(json!({
        "status": "ok",
        "anchor": paths.anchor,
        "project_root": paths.project_root,
        "state_root": paths.state_root,
        "database_root": paths.database_root,
        "media_database_touched": false,
        "schema": meta,
    }))
}

async fn status(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let mut response = db
        .query("SELECT count() AS count FROM source_proposal GROUP ALL; SELECT count() AS count FROM rejection_audit GROUP ALL;")
        .await
        .map_err(|error| format!("ledger status query: {error}"))?;
    let proposals: Vec<Value> = response
        .take(0)
        .map_err(|error| format!("decode proposal count: {error}"))?;
    let rejections: Vec<Value> = response
        .take(1)
        .map_err(|error| format!("decode rejection count: {error}"))?;
    Ok(json!({
        "status": "ok",
        "proposal_count": proposals.first().and_then(|value| value.get("count")).cloned().unwrap_or(json!(0)),
        "rejection_count": rejections.first().and_then(|value| value.get("count")).cloned().unwrap_or(json!(0)),
        "project_root": paths.project_root,
    }))
}

async fn load_captured_sources_async(paths: &LedgerPaths) -> Result<Vec<CapturedSource>, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, source_kind, state FROM source_proposal; \
             SELECT source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture;",
        )
        .await
        .map_err(|error| format!("load timeline source dashboard: {error}"))?;
    let proposals: Vec<ProposalRow> = response
        .take(0)
        .map_err(|error| format!("decode source proposals: {error}"))?;
    let captures: Vec<CaptureRow> = response
        .take(1)
        .map_err(|error| format!("decode source captures: {error}"))?;
    let captures = captures
        .into_iter()
        .map(|capture| (capture.source_id.clone(), capture))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut rows = proposals
        .into_iter()
        .map(|proposal| {
            let capture = captures
                .get(&proposal.source_id)
                .cloned()
                .unwrap_or_default();
            CapturedSource {
                proposal_id: proposal.proposal_id,
                job_id: proposal.job_id,
                source_id: proposal.source_id,
                source_kind: proposal.source_kind,
                state: proposal.state,
                canonical_url: capture.canonical_url,
                content_sha256: capture.content_sha256,
                capture_path: capture.capture_path,
                byte_length: capture.byte_length,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| right.proposal_id.cmp(&left.proposal_id));
    Ok(rows)
}

async fn propose_source(
    paths: &LedgerPaths,
    job_id: &str,
    raw_url: &str,
    source_kind: &str,
) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let url = match Url::parse(raw_url)
        .map_err(|error| error.to_string())
        .and_then(require_https)
    {
        Ok(url) => url,
        Err(error) => return rejection(&db, job_id, "INVALID_URL", &error).await,
    };
    let response = match reqwest::Client::builder()
        .user_agent("FacialTimelineLedger/1")
        .build()
        .map_err(|error| format!("build source client: {error}"))?
        .get(url.clone())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return rejection(&db, job_id, "SOURCE_UNREACHABLE", &error.to_string()).await
        }
    };
    if !response.status().is_success() {
        return rejection(
            &db,
            job_id,
            "SOURCE_HTTP_STATUS",
            &response.status().to_string(),
        )
        .await;
    }
    let canonical_url = response.url().as_str().to_string();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return rejection(&db, job_id, "SOURCE_BODY_UNREADABLE", &error.to_string()).await
        }
    };
    let hash = hex_sha256(&body);
    let capture_path = paths.captures_root.join(format!("{hash}.bin"));
    if !capture_path.exists() {
        fs::write(&capture_path, &body)
            .map_err(|error| format!("write source capture: {error}"))?;
    }
    let source_id = format!("KTL-SRC-{}", &hash[..20]);
    let proposal_id = format!("KTL-PROP-{}", &hash[..20]);
    db.query(
        "
        UPSERT type::thing('source_capture', $source_id) SET source_id = $source_id, canonical_url = $url, content_sha256 = $hash, capture_path = $capture_path, byte_length = $bytes;
        UPSERT type::thing('source_proposal', $proposal_id) SET proposal_id = $proposal_id, job_id = $job_id, source_id = $source_id, source_kind = $source_kind, state = 'captured';
        ",
    )
    .bind(("source_id", source_id.clone()))
    .bind(("proposal_id", proposal_id.clone()))
    .bind(("url", canonical_url.clone()))
    .bind(("hash", hash.clone()))
    .bind(("capture_path", capture_path.to_string_lossy().to_string()))
    .bind(("bytes", body.len() as u64))
    .bind(("job_id", job_id.to_string()))
    .bind(("source_kind", source_kind.to_string()))
    .await
    .map_err(|error| format!("persist source proposal: {error}"))?;
    Ok(json!({
        "status": "captured",
        "job_id": job_id,
        "proposal_id": proposal_id,
        "source_id": source_id,
        "canonical_url": canonical_url,
        "content_sha256": hash,
        "capture_path": capture_path,
        "canonical_fact_written": false,
    }))
}

fn require_https(url: Url) -> Result<Url, String> {
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("source URL must be an absolute https URL".to_string());
    }
    Ok(url)
}

async fn rejection(
    db: &Surreal<surrealdb::engine::local::Db>,
    job_id: &str,
    code: &str,
    detail: &str,
) -> Result<Value, String> {
    let digest = hex_sha256(format!("{job_id}\n{code}\n{detail}").as_bytes());
    let audit_id = format!("KTL-REJ-{}", &digest[..20]);
    db.query("UPSERT type::thing('rejection_audit', $audit_id) SET audit_id = $audit_id, job_id = $job_id, code = $code, detail = $detail;")
        .bind(("audit_id", audit_id.clone()))
        .bind(("job_id", job_id.to_string()))
        .bind(("code", code.to_string()))
        .bind(("detail", detail.to_string()))
        .await
        .map_err(|error| format!("persist rejection audit: {error}"))?;
    Ok(
        json!({"status": "rejected", "job_id": job_id, "rejection_id": audit_id, "code": code, "canonical_fact_written": false}),
    )
}

fn path_receipt(status: &str, paths: &LedgerPaths) -> Value {
    json!({
        "status": status,
        "anchor": paths.anchor,
        "project_root": paths.project_root,
        "state_root": paths.state_root,
        "database_root": paths.database_root,
        "media_database_touched": false,
        "schema_version": SCHEMA_VERSION,
    })
}

fn hex_sha256(input: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(input);
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("facial-timeline-ledger-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join(ANCHOR_NAME), "version: 1\n").unwrap();
        root
    }

    #[test]
    fn discovers_anchor_after_project_relocation() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root.join("nested")).unwrap();
        assert_eq!(paths.project_root, fs::canonicalize(&root).unwrap());
        assert!(paths
            .database_root
            .ends_with(".facial/timeline-ledger/surrealdb"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_malformed_job_before_database_open() {
        assert!(validate_job_id("bad job").is_err());
        assert!(validate_source_kind("anything-goes").is_err());
    }

    #[test]
    fn rejects_non_https_urls() {
        assert!(require_https(Url::parse("file:///tmp/x").unwrap()).is_err());
        assert!(require_https(Url::parse("https://example.test/a").unwrap()).is_ok());
    }
}
