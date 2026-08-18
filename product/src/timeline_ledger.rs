//! WP-077 isolated embedded SurrealDB ledger.
//!
//! This module deliberately has no dependency on `media_db`. It accepts only
//! bounded worker observations and creates all persistent records itself.

use crate::surreal_store;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use surrealdb::types::SurrealValue;

const ANCHOR_NAME: &str = "timeline-maintenance.yaml";
const NAMESPACE: &str = "facial";
const DATABASE: &str = "timeline_ledger";
const SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
const ENGINE_VERSION: &str = env!("FACIAL_SURREALDB_VERSION");
const ENGINE_MARKER_NAME: &str = "engine.json";
const MIGRATION_FORMAT: &str = "facial-timeline-ledger-migration-v1";
const LOGICAL_FORMAT: &str = "facial-timeline-ledger-logical-v2";
const SOURCE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_MAX_BYTES: usize = 16 * 1024 * 1024;

type EmbeddedDb = std::sync::Arc<surreal_store::Store>;

#[derive(Debug, Clone)]
struct LedgerPaths {
    project_root: PathBuf,
    anchor: PathBuf,
    state_root: PathBuf,
    database_root: PathBuf,
    captures_root: PathBuf,
    engine_marker: PathBuf,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct RejectionAudit {
    pub audit_id: String,
    pub job_id: String,
    pub code: String,
    pub detail: String,
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, SurrealValue, PartialEq, Eq, PartialOrd, Ord,
)]
struct ProposalRow {
    proposal_id: String,
    job_id: String,
    source_id: String,
    #[serde(default)]
    capture_id: String,
    #[serde(default)]
    canonical_url: String,
    source_kind: String,
    state: String,
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, SurrealValue, PartialEq, Eq, PartialOrd, Ord,
)]
struct CaptureRow {
    #[serde(default)]
    capture_id: String,
    source_id: String,
    canonical_url: String,
    content_sha256: String,
    capture_path: String,
    byte_length: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, SurrealValue)]
struct LedgerMeta {
    version: u32,
    engine: String,
    engine_version: String,
    namespace: String,
    database: String,
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, SurrealValue, PartialEq, Eq, PartialOrd, Ord,
)]
struct RejectionRow {
    audit_id: String,
    job_id: String,
    code: String,
    detail: String,
}

#[derive(
    Debug, Clone, Default, Deserialize, Serialize, SurrealValue, PartialEq, Eq, PartialOrd, Ord,
)]
struct ReceiptRow {
    receipt_id: String,
    receipt_kind: String,
    job_scope: String,
    terminal_id: String,
    requested: u64,
    captured: u64,
    rejected: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct MigrationBundle {
    format: String,
    source_engine: String,
    source_database_sha256_before_open: String,
    source_database_sha256_after_close: String,
    proposals: Vec<ProposalRow>,
    captures: Vec<CaptureRow>,
    rejections: Vec<RejectionRow>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LogicalBundle {
    format: String,
    engine: String,
    engine_version: String,
    schema_version: u32,
    proposals: Vec<ProposalRow>,
    captures: Vec<CaptureRow>,
    rejections: Vec<RejectionRow>,
    #[serde(default)]
    receipts: Vec<ReceiptRow>,
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
                    engine_marker: state_root.join(ENGINE_MARKER_NAME),
                    state_root,
                });
            }
        }
        Err(format!(
            "no {ANCHOR_NAME} found at or above {}",
            start.display()
        ))
    }

    fn ensure_state_dirs(&self) -> Result<(), String> {
        fs::create_dir_all(&self.state_root)
            .map_err(|error| format!("create {}: {error}", self.state_root.display()))?;
        fs::create_dir_all(&self.captures_root)
            .map_err(|error| format!("create {}: {error}", self.captures_root.display()))
    }
}

/// Provider-neutral timeline intake, schema migration, integrity, and logical backup CLI.
pub fn run_cli(args: &[String]) -> Result<Value, String> {
    let result = run_cli_inner(args);
    if let Err(error) = &result {
        audit_malformed_submission(args, error);
    }
    result
}

fn run_cli_inner(args: &[String]) -> Result<Value, String> {
    let (command, rest) = args.split_first().ok_or_else(|| {
        "missing command; use init|doctor|status|list-sources|upgrade-schema-v2|export-logical|rebuild-logical|import-v2-export|verify-v2-export|migrate-v2-export|propose-source|propose-source-batch".to_string()
    })?;
    if command == "propose-source-batch" {
        return run_proposal_batch(rest);
    }
    let flags = parse_flags(rest)?;
    let root = flags
        .get("project-root")
        .ok_or_else(|| "--project-root PATH is required".to_string())?;
    let paths = LedgerPaths::discover(Path::new(root))?;

    match command.as_str() {
        "init" => run_async(init(&paths)),
        "doctor" => run_async(doctor(&paths)),
        "status" => run_async(status(&paths)),
        "upgrade-schema-v2" => run_async(upgrade_schema_v1_to_v2(&paths)),
        "list-sources" => {
            let job_prefix = flags.get("job-prefix").map(String::as_str);
            run_async(list_sources(&paths, job_prefix))
        }
        "export-logical" => {
            let out = required(&flags, "out")?;
            run_async(export_logical(&paths, Path::new(out)))
        }
        "rebuild-logical" => {
            let bundle = required(&flags, "bundle")?;
            run_async(rebuild_logical(&paths, Path::new(bundle)))
        }
        "import-v2-export" => {
            let bundle = required(&flags, "bundle")?;
            run_async(import_v2_export(&paths, Path::new(bundle)))
        }
        "verify-v2-export" => {
            let bundle = required(&flags, "bundle")?;
            run_async(verify_v2_export(&paths, Path::new(bundle)))
        }
        "migrate-v2-export" => {
            let bundle = required(&flags, "bundle")?;
            run_async(migrate_v2_export(&paths, Path::new(bundle)))
        }
        "propose-source" => {
            let job_id = required(&flags, "job")?;
            let url = required(&flags, "url")?;
            let source_kind = required(&flags, "source-kind")?;
            validate_job_id(job_id)?;
            validate_source_kind(source_kind)?;
            run_async(propose_source(&paths, job_id, url, source_kind))
        }
        other => Err(format!(
            "unknown timeline-ledger command {other}; use init|doctor|status|list-sources|upgrade-schema-v2|export-logical|rebuild-logical|import-v2-export|verify-v2-export|migrate-v2-export|propose-source|propose-source-batch"
        )),
    }
}

fn audit_malformed_submission(args: &[String], error: &str) {
    let Some(command) = args.first() else { return };
    if command != "propose-source" && command != "propose-source-batch" {
        return;
    }
    let flag_value = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_str())
    };
    let Some(root) = flag_value("--project-root") else {
        return;
    };
    let Ok(paths) = LedgerPaths::discover(Path::new(root)) else {
        return;
    };
    let job_id = flag_value("--job")
        .or_else(|| flag_value("--job-prefix"))
        .unwrap_or("INVALID-SUBMISSION");
    let job_id = job_id.chars().take(128).collect::<String>();
    let detail = error.chars().take(2048).collect::<String>();
    let _ = run_async(async {
        let db = open(&paths).await?;
        initialize_schema(&db).await?;
        rejection(&db, &job_id, "MALFORMED_SUBMISSION", &detail).await
    });
}

fn run_proposal_batch(args: &[String]) -> Result<Value, String> {
    let flags = parse_repeated_flags(args, "url")?;
    let root = required_repeated_single(&flags, "project-root")?;
    let job_prefix = required_repeated_single(&flags, "job-prefix")?;
    let source_kind = required_repeated_single(&flags, "source-kind")?;
    validate_job_id(job_prefix)?;
    if job_prefix.len() > 123 {
        return Err("--job-prefix must leave room for the generated -0001 suffix".to_string());
    }
    validate_source_kind(source_kind)?;
    let urls = flags
        .get("url")
        .ok_or_else(|| "at least one --url is required".to_string())?;
    if urls.len() > 1000 {
        return Err("propose-source-batch accepts at most 1000 --url values".to_string());
    }
    if urls.iter().any(|url| url.len() > 4096) {
        return Err("each --url must be at most 4096 bytes".to_string());
    }
    let paths = LedgerPaths::discover(Path::new(root))?;
    run_async(propose_sources(&paths, job_prefix, urls, source_kind))
}

pub(crate) fn discover_project_root(start: &Path) -> Result<PathBuf, String> {
    Ok(LedgerPaths::discover(start)?.project_root)
}

pub(crate) fn load_captured_sources(start: &Path) -> Result<Vec<CapturedSource>, String> {
    let paths = LedgerPaths::discover(start)?;
    run_async(load_captured_sources_async(&paths))
}

pub(crate) fn load_rejection_audits(start: &Path) -> Result<Vec<RejectionAudit>, String> {
    let paths = LedgerPaths::discover(start)?;
    run_async(async {
        let db = open(&paths).await?;
        let meta = read_schema_meta(&db)
            .await?
            .ok_or_else(|| "ledger schema metadata is missing".to_string())?;
        validate_schema_meta(&meta)?;
        let mut response = db
            .query("SELECT audit_id, job_id, code, detail FROM rejection_audit ORDER BY audit_id DESC;")
            .await
            .map_err(|error| format!("load rejection audits: {error}"))?;
        let rows: Vec<RejectionRow> = response
            .take(0)
            .map_err(|error| format!("decode rejection audits: {error}"))?;
        Ok(rows
            .into_iter()
            .map(|row| RejectionAudit {
                audit_id: row.audit_id,
                job_id: row.job_id,
                code: row.code,
                detail: row.detail,
            })
            .collect())
    })
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

fn parse_repeated_flags(
    args: &[String],
    repeated_key: &str,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, String> {
    let mut result = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {flag}"))?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        let values = result.entry(key.to_string()).or_default();
        if key != repeated_key && !values.is_empty() {
            return Err(format!("{flag} was supplied more than once"));
        }
        values.push(value.to_string());
        index += 2;
    }
    Ok(result)
}

fn required_repeated_single<'a>(
    flags: &'a std::collections::BTreeMap<String, Vec<String>>,
    key: &str,
) -> Result<&'a str, String> {
    flags
        .get(key)
        .and_then(|values| values.first())
        .map(String::as_str)
        .ok_or_else(|| format!("--{key} is required"))
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
    surreal_store::run(future)
}

async fn open(paths: &LedgerPaths) -> Result<EmbeddedDb, String> {
    paths.ensure_state_dirs()?;
    require_current_engine(paths)?;
    connect_embedded_database(&paths.database_root).await
}

async fn connect_embedded_database(database_root: &Path) -> Result<EmbeddedDb, String> {
    // Must use the async entry point: this runs inside the shared runtime, and
    // the synchronous one block_on's that same runtime.
    surreal_store::open_database_async(database_root, DATABASE, SCHEMA_VERSION as u64).await
}

async fn connect_uncached_embedded_database(database_root: &Path) -> Result<EmbeddedDb, String> {
    connect_embedded_database(database_root).await
}

fn require_current_engine(paths: &LedgerPaths) -> Result<(), String> {
    if paths.engine_marker.is_file() {
        let marker: Value = serde_json::from_slice(
            &fs::read(&paths.engine_marker)
                .map_err(|error| format!("read {}: {error}", paths.engine_marker.display()))?,
        )
        .map_err(|error| format!("parse {}: {error}", paths.engine_marker.display()))?;
        let marker_version = marker.get("engine_version").and_then(Value::as_str);
        if marker.get("engine").and_then(Value::as_str) == Some("surrealdb")
            && marker_version == Some(ENGINE_VERSION)
            && marker.get("schema_version").and_then(Value::as_u64) == Some(SCHEMA_VERSION as u64)
        {
            return Ok(());
        }
        return Err(format!(
            "timeline ledger engine marker does not match embedded SurrealDB {ENGINE_VERSION}; migrate or restore before opening"
        ));
    }
    let has_database_files = paths.database_root.is_dir()
        && fs::read_dir(&paths.database_root)
            .map_err(|error| format!("read {}: {error}", paths.database_root.display()))?
            .next()
            .is_some();
    if has_database_files {
        return Err(format!(
            "unmarked legacy timeline ledger at {}; run timeline-ledger migrate-v2-export --project-root PATH --bundle FILE before opening it with SurrealDB {ENGINE_VERSION}",
            paths.database_root.display()
        ));
    }
    Ok(())
}

fn write_engine_marker(path: &Path) -> Result<(), String> {
    let payload = json!({
        "engine": "surrealdb",
        "engine_version": ENGINE_VERSION,
        "schema_version": SCHEMA_VERSION,
        "namespace": NAMESPACE,
        "database": DATABASE,
    });
    write_json_atomic(path, &payload)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("timeline-ledger"),
        uuid::Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize {}: {error}", path.display()))?;
    fs::write(&temp, bytes).map_err(|error| format!("write {}: {error}", temp.display()))?;
    replace_file_atomic(&temp, path).map_err(|error| format!("publish {}: {error}", path.display()))
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

async fn initialize_schema(db: &EmbeddedDb) -> Result<(), String> {
    db.query("DEFINE TABLE IF NOT EXISTS ledger_meta SCHEMALESS;")
        .await
        .map_err(|error| format!("bootstrap ledger schema metadata: {error}"))?
        .check()
        .map_err(|error| format!("bootstrap ledger schema metadata: {error}"))?;
    if let Some(meta) = read_schema_meta(db).await? {
        validate_schema_meta(&meta)?;
    }
    apply_schema(db).await
}

async fn apply_schema(db: &EmbeddedDb) -> Result<(), String> {
    db.query(
        "
        DEFINE TABLE OVERWRITE ledger_meta SCHEMAFULL;
        DEFINE FIELD OVERWRITE version ON TABLE ledger_meta TYPE int;
        DEFINE FIELD OVERWRITE engine ON TABLE ledger_meta TYPE string;
        DEFINE FIELD OVERWRITE engine_version ON TABLE ledger_meta TYPE string;
        DEFINE FIELD OVERWRITE namespace ON TABLE ledger_meta TYPE string;
        DEFINE FIELD OVERWRITE database ON TABLE ledger_meta TYPE string;

        DEFINE TABLE OVERWRITE source_capture SCHEMAFULL;
        DEFINE FIELD OVERWRITE capture_id ON TABLE source_capture TYPE string;
        DEFINE FIELD OVERWRITE source_id ON TABLE source_capture TYPE string;
        DEFINE FIELD OVERWRITE canonical_url ON TABLE source_capture TYPE string;
        DEFINE FIELD OVERWRITE content_sha256 ON TABLE source_capture TYPE string;
        DEFINE FIELD OVERWRITE capture_path ON TABLE source_capture TYPE string;
        DEFINE FIELD OVERWRITE byte_length ON TABLE source_capture TYPE int ASSERT $value >= 0;
        DEFINE INDEX OVERWRITE source_capture_identity ON TABLE source_capture COLUMNS capture_id UNIQUE;
        DEFINE INDEX OVERWRITE source_capture_source ON TABLE source_capture COLUMNS source_id;

        DEFINE TABLE OVERWRITE source_proposal SCHEMAFULL;
        DEFINE FIELD OVERWRITE proposal_id ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE job_id ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE source_id ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE capture_id ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE canonical_url ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE source_kind ON TABLE source_proposal TYPE string;
        DEFINE FIELD OVERWRITE state ON TABLE source_proposal TYPE string;
        DEFINE INDEX OVERWRITE source_proposal_identity ON TABLE source_proposal COLUMNS proposal_id UNIQUE;
        DEFINE INDEX OVERWRITE source_proposal_job ON TABLE source_proposal COLUMNS job_id;

        DEFINE TABLE OVERWRITE rejection_audit SCHEMAFULL;
        DEFINE FIELD OVERWRITE audit_id ON TABLE rejection_audit TYPE string;
        DEFINE FIELD OVERWRITE job_id ON TABLE rejection_audit TYPE string;
        DEFINE FIELD OVERWRITE code ON TABLE rejection_audit TYPE string;
        DEFINE FIELD OVERWRITE detail ON TABLE rejection_audit TYPE string;
        DEFINE INDEX OVERWRITE rejection_audit_identity ON TABLE rejection_audit COLUMNS audit_id UNIQUE;
        DEFINE INDEX OVERWRITE rejection_audit_job ON TABLE rejection_audit COLUMNS job_id;

        DEFINE TABLE OVERWRITE ingestion_receipt SCHEMAFULL;
        DEFINE FIELD OVERWRITE receipt_id ON TABLE ingestion_receipt TYPE string;
        DEFINE FIELD OVERWRITE receipt_kind ON TABLE ingestion_receipt TYPE string ASSERT $value IN ['submission', 'batch', 'migration'];
        DEFINE FIELD OVERWRITE job_scope ON TABLE ingestion_receipt TYPE string;
        DEFINE FIELD OVERWRITE terminal_id ON TABLE ingestion_receipt TYPE string;
        DEFINE FIELD OVERWRITE requested ON TABLE ingestion_receipt TYPE int ASSERT $value >= 0;
        DEFINE FIELD OVERWRITE captured ON TABLE ingestion_receipt TYPE int ASSERT $value >= 0;
        DEFINE FIELD OVERWRITE rejected ON TABLE ingestion_receipt TYPE int ASSERT $value >= 0;
        DEFINE INDEX OVERWRITE ingestion_receipt_identity ON TABLE ingestion_receipt COLUMNS receipt_id UNIQUE;
        DEFINE INDEX OVERWRITE ingestion_receipt_scope ON TABLE ingestion_receipt COLUMNS job_scope;

        UPSERT ledger_meta:schema SET version = $version, engine = 'surrealdb', engine_version = $engine_version, namespace = $namespace, database = $database;
        ",
    )
    .bind(("version", SCHEMA_VERSION))
    .bind(("engine_version", ENGINE_VERSION))
    .bind(("namespace", NAMESPACE))
    .bind(("database", DATABASE))
    .await
    .map_err(|error| format!("apply ledger schema: {error}"))?
    .check()
    .map_err(|error| format!("apply ledger schema: {error}"))?;
    Ok(())
}

async fn read_schema_meta(db: &EmbeddedDb) -> Result<Option<LedgerMeta>, String> {
    let mut response = db
        .query(
            "SELECT version, engine, engine_version, namespace, database FROM ledger_meta:schema;",
        )
        .await
        .map_err(|error| format!("read ledger schema metadata: {error}"))?;
    response
        .take(0)
        .map_err(|error| format!("decode ledger schema metadata: {error}"))
}

fn validate_schema_meta(meta: &LedgerMeta) -> Result<(), String> {
    if meta.version != SCHEMA_VERSION
        || meta.engine != "surrealdb"
        || meta.engine_version != ENGINE_VERSION
        || meta.namespace != NAMESPACE
        || meta.database != DATABASE
    {
        return Err(format!(
            "ledger schema metadata is incompatible: expected schema {} on surrealdb {} {}/{}; observed schema {} on {} {} {}/{}",
            SCHEMA_VERSION,
            ENGINE_VERSION,
            NAMESPACE,
            DATABASE,
            meta.version,
            meta.engine,
            meta.engine_version,
            meta.namespace,
            meta.database
        ));
    }
    Ok(())
}

async fn init(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    Ok(path_receipt("initialized", paths))
}

async fn doctor(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    let meta = read_schema_meta(&db)
        .await?
        .ok_or_else(|| "ledger schema metadata is missing; doctor will not heal it".to_string())?;
    validate_schema_meta(&meta)?;
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, capture_id, canonical_url, source_kind, state FROM source_proposal; \
             SELECT capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture; \
             SELECT audit_id, job_id, code, detail FROM rejection_audit; \
             SELECT receipt_id, receipt_kind, job_scope, terminal_id, requested, captured, rejected FROM ingestion_receipt;",
        )
        .await
        .map_err(|error| format!("ledger doctor query: {error}"))?;
    let proposals: Vec<ProposalRow> = response
        .take(0)
        .map_err(|error| format!("ledger doctor decode: {error}"))?;
    let captures: Vec<CaptureRow> = response
        .take(1)
        .map_err(|error| format!("ledger doctor capture decode: {error}"))?;
    let rejections: Vec<RejectionRow> = response
        .take(2)
        .map_err(|error| format!("ledger doctor rejection decode: {error}"))?;
    let receipts: Vec<ReceiptRow> = response
        .take(3)
        .map_err(|error| format!("ledger doctor receipt decode: {error}"))?;
    validate_integrity(paths, &proposals, &captures)?;
    validate_receipts(&receipts)?;
    Ok(json!({
        "status": "ok",
        "anchor": paths.anchor,
        "project_root": paths.project_root,
        "state_root": paths.state_root,
        "database_root": paths.database_root,
        "media_database_touched": false,
        "engine_version": ENGINE_VERSION,
        "schema": meta,
        "proposal_count": proposals.len(),
        "capture_count": captures.len(),
        "rejection_count": rejections.len(),
        "receipt_count": receipts.len(),
        "proposal_row_set_sha256": digest_rows(&proposals)?,
        "capture_row_set_sha256": digest_rows(&captures)?,
        "rejection_row_set_sha256": digest_rows(&rejections)?,
        "receipt_row_set_sha256": digest_rows(&receipts)?,
    }))
}

fn validate_integrity(
    paths: &LedgerPaths,
    proposals: &[ProposalRow],
    captures: &[CaptureRow],
) -> Result<(), String> {
    let captures_by_id = captures
        .iter()
        .map(|capture| (capture.capture_id.as_str(), capture))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut referenced = std::collections::BTreeSet::new();
    for proposal in proposals {
        let capture = captures_by_id
            .get(proposal.capture_id.as_str())
            .ok_or_else(|| {
                format!(
                    "proposal {} references missing capture {}",
                    proposal.proposal_id, proposal.capture_id
                )
            })?;
        if proposal.source_id != capture.source_id
            || proposal.canonical_url != capture.canonical_url
            || proposal.state != "captured"
        {
            return Err(format!(
                "proposal {} relationship fields do not match capture {}",
                proposal.proposal_id, proposal.capture_id
            ));
        }
        referenced.insert(capture.capture_id.as_str());
    }
    for capture in captures {
        if !referenced.contains(capture.capture_id.as_str()) {
            return Err(format!(
                "orphan capture {} has no proposal",
                capture.capture_id
            ));
        }
        let relative = capture_relative_path(&capture.content_sha256)?;
        if capture.capture_path != relative.to_string_lossy() {
            return Err(format!(
                "capture {} stores non-portable or inconsistent path {}",
                capture.capture_id, capture.capture_path
            ));
        }
        let path = paths.state_root.join(&relative);
        let bytes = fs::read(&path).map_err(|error| {
            format!(
                "read capture {} at {}: {error}",
                capture.capture_id,
                path.display()
            )
        })?;
        if bytes.len() as u64 != capture.byte_length || hex_sha256(&bytes) != capture.content_sha256
        {
            return Err(format!(
                "capture {} fails byte-length/SHA-256 verification",
                capture.capture_id
            ));
        }
        let parsed = Url::parse(&capture.canonical_url)
            .map_err(|error| format!("capture {} URL is invalid: {error}", capture.capture_id))?;
        require_https(parsed)
            .map_err(|error| format!("capture {} URL is invalid: {error}", capture.capture_id))?;
    }
    Ok(())
}

fn validate_receipts(receipts: &[ReceiptRow]) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    for receipt in receipts {
        if receipt.requested != receipt.captured.saturating_add(receipt.rejected) {
            return Err(format!(
                "ingestion receipt {} does not balance requested against captured plus rejected",
                receipt.receipt_id
            ));
        }
        if !matches!(
            receipt.receipt_kind.as_str(),
            "submission" | "batch" | "migration"
        ) || receipt.job_scope.is_empty()
            || receipt.terminal_id.is_empty()
            || receipt.receipt_id
                != receipt_id_for(
                    &receipt.receipt_kind,
                    &receipt.job_scope,
                    &receipt.terminal_id,
                )
        {
            return Err(format!(
                "ingestion receipt {} has an invalid identity contract",
                receipt.receipt_id
            ));
        }
        if !ids.insert(receipt.receipt_id.as_str()) {
            return Err(format!(
                "duplicate ingestion receipt identity {}",
                receipt.receipt_id
            ));
        }
    }
    Ok(())
}

fn migration_receipts(
    proposals: &[ProposalRow],
    rejections: &[RejectionRow],
    migration_scope: &str,
) -> Vec<ReceiptRow> {
    let mut receipts = proposals
        .iter()
        .map(|proposal| receipt_row("migration", migration_scope, &proposal.proposal_id, 1, 1, 0))
        .chain(rejections.iter().map(|rejection| {
            receipt_row("migration", migration_scope, &rejection.audit_id, 1, 0, 1)
        }))
        .collect::<Vec<_>>();
    receipts.sort();
    receipts.dedup_by(|left, right| left.receipt_id == right.receipt_id);
    receipts
}

async fn status(paths: &LedgerPaths) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let mut response = db
        .query("SELECT count() AS count FROM source_proposal GROUP ALL; SELECT count() AS count FROM rejection_audit GROUP ALL; SELECT count() AS count FROM ingestion_receipt GROUP ALL;")
        .await
        .map_err(|error| format!("ledger status query: {error}"))?;
    let proposals: Vec<Value> = response
        .take(0)
        .map_err(|error| format!("decode proposal count: {error}"))?;
    let rejections: Vec<Value> = response
        .take(1)
        .map_err(|error| format!("decode rejection count: {error}"))?;
    let receipts: Vec<Value> = response
        .take(2)
        .map_err(|error| format!("decode receipt count: {error}"))?;
    Ok(json!({
        "status": "ok",
        "proposal_count": proposals.first().and_then(|value| value.get("count")).cloned().unwrap_or(json!(0)),
        "rejection_count": rejections.first().and_then(|value| value.get("count")).cloned().unwrap_or(json!(0)),
        "receipt_count": receipts.first().and_then(|value| value.get("count")).cloned().unwrap_or(json!(0)),
        "project_root": paths.project_root,
        "engine_version": ENGINE_VERSION,
    }))
}

async fn load_all_rows(
    db: &EmbeddedDb,
) -> Result<
    (
        Vec<ProposalRow>,
        Vec<CaptureRow>,
        Vec<RejectionRow>,
        Vec<ReceiptRow>,
    ),
    String,
> {
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, capture_id, canonical_url, source_kind, state FROM source_proposal; \
             SELECT capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture; \
             SELECT audit_id, job_id, code, detail FROM rejection_audit; \
             SELECT receipt_id, receipt_kind, job_scope, terminal_id, requested, captured, rejected FROM ingestion_receipt;",
        )
        .await
        .map_err(|error| format!("read logical ledger rows: {error}"))?;
    let proposals = response
        .take(0)
        .map_err(|error| format!("decode logical proposals: {error}"))?;
    let captures = response
        .take(1)
        .map_err(|error| format!("decode logical captures: {error}"))?;
    let rejections = response
        .take(2)
        .map_err(|error| format!("decode logical rejections: {error}"))?;
    let receipts = response
        .take(3)
        .map_err(|error| format!("decode logical receipts: {error}"))?;
    Ok((proposals, captures, rejections, receipts))
}

async fn load_legacy_rows(
    db: &EmbeddedDb,
) -> Result<(Vec<ProposalRow>, Vec<CaptureRow>, Vec<RejectionRow>), String> {
    // Schema v1 predates capture_id (and the proposal's canonical_url), so those
    // columns are NONE on a legacy row. The row structs mark them
    // `#[serde(default)]`, but the SurrealValue derive that actually decodes
    // here does not honour serde attributes and rejects NONE for a String.
    // Coalesce in this legacy reader only: the upgrade re-derives both values
    // from the capture's canonical URL and content hash, so for v1 rows only
    // successful decoding matters. The v2 readers stay strict.
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, capture_id ?? '' AS capture_id, canonical_url ?? '' AS canonical_url, source_kind, state FROM source_proposal; \
             SELECT capture_id ?? '' AS capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture; \
             SELECT audit_id, job_id, code, detail FROM rejection_audit;",
        )
        .await
        .map_err(|error| format!("read legacy logical ledger rows: {error}"))?;
    Ok((
        response
            .take(0)
            .map_err(|error| format!("decode legacy proposals: {error}"))?,
        response
            .take(1)
            .map_err(|error| format!("decode legacy captures: {error}"))?,
        response
            .take(2)
            .map_err(|error| format!("decode legacy rejections: {error}"))?,
    ))
}

async fn persist_rows_transaction(
    db: &EmbeddedDb,
    proposals: &[ProposalRow],
    captures: &[CaptureRow],
    rejections: &[RejectionRow],
    receipts: &[ReceiptRow],
    replace: bool,
) -> Result<(), String> {
    validate_receipts(receipts)?;
    let sql = if replace {
        "BEGIN TRANSACTION;
         DELETE source_proposal; DELETE source_capture; DELETE rejection_audit; DELETE ingestion_receipt;
         FOR $row IN $captures { UPSERT type::record('source_capture', $row.capture_id) CONTENT $row; };
         FOR $row IN $proposals { UPSERT type::record('source_proposal', $row.proposal_id) CONTENT $row; };
         FOR $row IN $rejections { UPSERT type::record('rejection_audit', $row.audit_id) CONTENT $row; };
         FOR $row IN $receipts { UPSERT type::record('ingestion_receipt', $row.receipt_id) CONTENT $row; };
         UPSERT ledger_meta:schema SET version = $schema_version, engine = 'surrealdb', engine_version = $engine_version, namespace = $namespace, database = $database;
         COMMIT TRANSACTION;"
    } else {
        "BEGIN TRANSACTION;
         FOR $row IN $captures { UPSERT type::record('source_capture', $row.capture_id) CONTENT $row; };
         FOR $row IN $proposals { UPSERT type::record('source_proposal', $row.proposal_id) CONTENT $row; };
         FOR $row IN $rejections { UPSERT type::record('rejection_audit', $row.audit_id) CONTENT $row; };
         FOR $row IN $receipts { UPSERT type::record('ingestion_receipt', $row.receipt_id) CONTENT $row; };
         UPSERT ledger_meta:schema SET version = $schema_version, engine = 'surrealdb', engine_version = $engine_version, namespace = $namespace, database = $database;
         COMMIT TRANSACTION;"
    };
    db.query(sql)
        .bind(("captures", captures.to_vec()))
        .bind(("proposals", proposals.to_vec()))
        .bind(("rejections", rejections.to_vec()))
        .bind(("receipts", receipts.to_vec()))
        .bind(("schema_version", SCHEMA_VERSION))
        .bind(("engine_version", ENGINE_VERSION))
        .bind(("namespace", NAMESPACE))
        .bind(("database", DATABASE))
        .await
        .map_err(|error| format!("persist logical ledger transaction: {error}"))?
        .check()
        .map_err(|error| format!("persist logical ledger transaction: {error}"))?;
    Ok(())
}

async fn upgrade_schema_v1_to_v2(paths: &LedgerPaths) -> Result<Value, String> {
    paths.ensure_state_dirs()?;
    let marker: Value =
        serde_json::from_slice(&fs::read(&paths.engine_marker).map_err(|error| {
            format!(
                "read legacy marker {}: {error}",
                paths.engine_marker.display()
            )
        })?)
        .map_err(|error| {
            format!(
                "parse legacy marker {}: {error}",
                paths.engine_marker.display()
            )
        })?;
    if marker.get("engine").and_then(Value::as_str) != Some("surrealdb")
        || marker.get("engine_version").and_then(Value::as_str) != Some(ENGINE_VERSION)
        || marker.get("schema_version").and_then(Value::as_u64)
            != Some(LEGACY_SCHEMA_VERSION as u64)
    {
        return Err(format!(
            "upgrade-schema-v2 requires an exact SurrealDB {ENGINE_VERSION} schema-v1 marker"
        ));
    }
    let db = surreal_store::open_database_async(
        &paths.database_root,
        DATABASE,
        LEGACY_SCHEMA_VERSION as u64,
    )
    .await?;
    let meta = read_schema_meta(&db)
        .await?
        .ok_or_else(|| "legacy ledger schema metadata is missing".to_string())?;
    if meta.version != LEGACY_SCHEMA_VERSION
        || meta.engine != "surrealdb"
        || meta.engine_version != ENGINE_VERSION
        || meta.namespace != NAMESPACE
        || meta.database != DATABASE
    {
        return Err(
            "legacy ledger schema metadata does not match the v1 upgrade predecessor".to_string(),
        );
    }
    let (legacy_proposals, legacy_captures, rejections) = load_legacy_rows(&db).await?;
    let captures_by_source = legacy_captures
        .iter()
        .map(|capture| (capture.source_id.clone(), capture))
        .collect::<std::collections::BTreeMap<_, _>>();
    if captures_by_source.len() != legacy_captures.len() {
        return Err("legacy ledger contains duplicate source identities".to_string());
    }
    for capture in &legacy_captures {
        let path = paths
            .state_root
            .join(capture_relative_path(&capture.content_sha256)?);
        let bytes = fs::read(&path)
            .map_err(|error| format!("read legacy capture {}: {error}", path.display()))?;
        if bytes.len() as u64 != capture.byte_length || hex_sha256(&bytes) != capture.content_sha256
        {
            return Err(format!(
                "legacy capture {} fails predecessor verification",
                capture.source_id
            ));
        }
    }
    let mut captures = std::collections::BTreeMap::<String, CaptureRow>::new();
    let mut proposals = Vec::with_capacity(legacy_proposals.len());
    for proposal in &legacy_proposals {
        let legacy_capture = captures_by_source
            .get(&proposal.source_id)
            .ok_or_else(|| format!("legacy proposal {} has no capture", proposal.proposal_id))?;
        let source_id = source_id_for_url(&legacy_capture.canonical_url);
        let capture_id = capture_id_for(&source_id, &legacy_capture.content_sha256);
        let capture = CaptureRow {
            capture_id: capture_id.clone(),
            source_id: source_id.clone(),
            canonical_url: legacy_capture.canonical_url.clone(),
            content_sha256: legacy_capture.content_sha256.to_ascii_lowercase(),
            capture_path: capture_relative_path(&legacy_capture.content_sha256)?
                .to_string_lossy()
                .to_string(),
            byte_length: legacy_capture.byte_length,
        };
        if let Some(previous) = captures.insert(capture_id.clone(), capture.clone()) {
            if previous != capture {
                return Err(format!(
                    "schema-v2 capture identity collision at {capture_id}"
                ));
            }
        }
        proposals.push(ProposalRow {
            proposal_id: proposal_id_for(&proposal.job_id, &source_id, &capture_id),
            job_id: proposal.job_id.clone(),
            source_id,
            capture_id,
            canonical_url: legacy_capture.canonical_url.clone(),
            source_kind: proposal.source_kind.clone(),
            state: proposal.state.clone(),
        });
    }
    let captures = captures.into_values().collect::<Vec<_>>();
    validate_integrity(paths, &proposals, &captures)?;
    let receipts = migration_receipts(&proposals, &rejections, "schema-v1-to-v2");
    validate_receipts(&receipts)?;
    let backup = LogicalBundle {
        format: "facial-timeline-ledger-schema-v1-predecessor".to_string(),
        engine: "surrealdb".to_string(),
        engine_version: ENGINE_VERSION.to_string(),
        schema_version: LEGACY_SCHEMA_VERSION,
        proposals: legacy_proposals,
        captures: legacy_captures,
        rejections: rejections.clone(),
        receipts: Vec::new(),
    };
    let backup_bytes = serde_json::to_vec_pretty(&backup)
        .map_err(|error| format!("serialize schema-v1 backup: {error}"))?;
    let backup_sha256 = hex_sha256(&backup_bytes);
    let backup_path = paths.state_root.join("migrations").join(format!(
        "schema-v1-predecessor-{}.json",
        &backup_sha256[..20]
    ));
    write_json_atomic(&backup_path, &backup)?;
    let staging_state_root = paths
        .state_root
        .join("migrations")
        .join(format!("schema-v2-staging-{}", uuid::Uuid::new_v4()));
    let staging_database_root = staging_state_root.join("surrealdb");
    let staging_db =
        surreal_store::open_database_async(&staging_database_root, DATABASE, SCHEMA_VERSION as u64)
            .await?;
    initialize_schema(&staging_db).await?;
    persist_rows_transaction(
        &staging_db,
        &proposals,
        &captures,
        &rejections,
        &receipts,
        true,
    )
    .await?;
    let (staged_proposals, staged_captures, staged_rejections, staged_receipts) =
        load_all_rows(&staging_db).await?;
    validate_integrity(paths, &staged_proposals, &staged_captures)?;
    validate_receipts(&staged_receipts)?;
    if digest_rows(&staged_proposals)? != digest_rows(&proposals)?
        || digest_rows(&staged_captures)? != digest_rows(&captures)?
        || digest_rows(&staged_rejections)? != digest_rows(&rejections)?
        || digest_rows(&staged_receipts)? != digest_rows(&receipts)?
    {
        return Err("schema-v2 staging database row digest mismatch".to_string());
    }
    drop(staging_db);
    surreal_store::wait_until_closed(&staging_database_root)?;
    drop(db);
    surreal_store::wait_until_closed(&paths.database_root)?;

    let backups_root = paths.state_root.join("backups");
    fs::create_dir_all(&backups_root)
        .map_err(|error| format!("create {}: {error}", backups_root.display()))?;
    let predecessor_database =
        backups_root.join(format!("surrealdb-schema-v1-{}", &backup_sha256[..20]));
    if predecessor_database.exists() {
        return Err(format!(
            "schema-v1 database backup already exists: {}",
            predecessor_database.display()
        ));
    }
    fs::rename(&paths.database_root, &predecessor_database).map_err(|error| {
        format!(
            "move schema-v1 database {} to {}: {error}",
            paths.database_root.display(),
            predecessor_database.display()
        )
    })?;
    if let Err(error) = fs::rename(&staging_database_root, &paths.database_root) {
        let rollback = fs::rename(&predecessor_database, &paths.database_root);
        return Err(format!(
            "publish schema-v2 database failed: {error}; schema-v1 rollback={}",
            rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|rollback_error| rollback_error.to_string())
        ));
    }
    if let Err(error) = write_engine_marker(&paths.engine_marker) {
        let failed_v2 = staging_state_root.join("surrealdb-publish-failed");
        let preserve = fs::rename(&paths.database_root, &failed_v2);
        let rollback = fs::rename(&predecessor_database, &paths.database_root);
        let marker_restore = write_json_atomic(&paths.engine_marker, &marker);
        return Err(format!(
            "publish schema-v2 marker failed: {error}; preserve-v2={}; schema-v1 rollback={}; marker rollback={}",
            preserve
                .map(|_| "ok".to_string())
                .unwrap_or_else(|move_error| move_error.to_string()),
            rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|rollback_error| rollback_error.to_string()),
            marker_restore
                .map(|_| "ok".to_string())
                .unwrap_or_else(|marker_error| marker_error.to_string())
        ));
    }
    let staging_marker = staging_state_root.join(ENGINE_MARKER_NAME);
    if staging_marker.exists() {
        fs::remove_file(&staging_marker).map_err(|error| {
            format!(
                "remove staging marker {}: {error}",
                staging_marker.display()
            )
        })?;
    }
    fs::remove_dir(&staging_state_root).map_err(|error| {
        format!(
            "remove empty staging root {}: {error}",
            staging_state_root.display()
        )
    })?;
    let current_db = open(paths).await?;
    let (current_proposals, current_captures, current_rejections, current_receipts) =
        load_all_rows(&current_db).await?;
    validate_integrity(paths, &current_proposals, &current_captures)?;
    validate_receipts(&current_receipts)?;
    if digest_rows(&current_proposals)? != digest_rows(&proposals)?
        || digest_rows(&current_captures)? != digest_rows(&captures)?
        || digest_rows(&current_rejections)? != digest_rows(&rejections)?
        || digest_rows(&current_receipts)? != digest_rows(&receipts)?
    {
        return Err("published schema-v2 database row digest mismatch".to_string());
    }
    Ok(json!({
        "status": "upgraded",
        "from_schema_version": LEGACY_SCHEMA_VERSION,
        "schema_version": SCHEMA_VERSION,
        "engine_version": ENGINE_VERSION,
        "proposal_count": proposals.len(),
        "capture_count": captures.len(),
        "rejection_count": rejections.len(),
        "receipt_count": receipts.len(),
        "predecessor_backup": backup_path,
        "predecessor_backup_sha256": backup_sha256,
        "predecessor_database": predecessor_database,
    }))
}

async fn export_logical(paths: &LedgerPaths, out: &Path) -> Result<Value, String> {
    let db = open(paths).await?;
    let meta = read_schema_meta(&db)
        .await?
        .ok_or_else(|| "ledger schema metadata is missing".to_string())?;
    validate_schema_meta(&meta)?;
    let (mut proposals, mut captures, mut rejections, mut receipts) = load_all_rows(&db).await?;
    validate_integrity(paths, &proposals, &captures)?;
    validate_receipts(&receipts)?;
    proposals.sort();
    captures.sort();
    rejections.sort();
    receipts.sort();
    let bundle = LogicalBundle {
        format: LOGICAL_FORMAT.to_string(),
        engine: "surrealdb".to_string(),
        engine_version: ENGINE_VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        proposals,
        captures,
        rejections,
        receipts,
    };
    write_json_atomic(out, &bundle)?;
    let bytes =
        fs::read(out).map_err(|error| format!("read logical export {}: {error}", out.display()))?;
    Ok(json!({
        "status": "exported",
        "out": out,
        "sha256": hex_sha256(&bytes),
        "proposal_count": bundle.proposals.len(),
        "capture_count": bundle.captures.len(),
        "rejection_count": bundle.rejections.len(),
        "receipt_count": bundle.receipts.len(),
    }))
}

async fn rebuild_logical(paths: &LedgerPaths, bundle_path: &Path) -> Result<Value, String> {
    let bytes = fs::read(bundle_path)
        .map_err(|error| format!("read logical bundle {}: {error}", bundle_path.display()))?;
    let bundle: LogicalBundle = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse logical bundle {}: {error}", bundle_path.display()))?;
    if bundle.format != LOGICAL_FORMAT
        || bundle.engine != "surrealdb"
        || bundle.engine_version != ENGINE_VERSION
        || bundle.schema_version != SCHEMA_VERSION
    {
        return Err("logical bundle engine/schema contract is incompatible".to_string());
    }
    validate_integrity(paths, &bundle.proposals, &bundle.captures)?;
    validate_receipts(&bundle.receipts)?;
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let (proposals, captures, rejections, receipts) = load_all_rows(&db).await?;
    if !proposals.is_empty()
        || !captures.is_empty()
        || !rejections.is_empty()
        || !receipts.is_empty()
    {
        return Err("logical rebuild target is not empty".to_string());
    }
    persist_rows_transaction(
        &db,
        &bundle.proposals,
        &bundle.captures,
        &bundle.rejections,
        &bundle.receipts,
        false,
    )
    .await?;
    let (mut proposals, mut captures, mut rejections, mut receipts) = load_all_rows(&db).await?;
    validate_integrity(paths, &proposals, &captures)?;
    proposals.sort();
    captures.sort();
    rejections.sort();
    receipts.sort();
    let mut expected_proposals = bundle.proposals.clone();
    let mut expected_captures = bundle.captures.clone();
    let mut expected_rejections = bundle.rejections.clone();
    let mut expected_receipts = bundle.receipts.clone();
    expected_proposals.sort();
    expected_captures.sort();
    expected_rejections.sort();
    expected_receipts.sort();
    if digest_rows(&proposals)? != digest_rows(&expected_proposals)?
        || digest_rows(&captures)? != digest_rows(&expected_captures)?
        || digest_rows(&rejections)? != digest_rows(&expected_rejections)?
        || digest_rows(&receipts)? != digest_rows(&expected_receipts)?
    {
        return Err("logical rebuild row digest mismatch".to_string());
    }
    Ok(json!({
        "status": "rebuilt",
        "bundle_sha256": hex_sha256(&bytes),
        "proposal_count": proposals.len(),
        "capture_count": captures.len(),
        "rejection_count": rejections.len(),
        "receipt_count": receipts.len(),
    }))
}

async fn migrate_v2_export(paths: &LedgerPaths, bundle_path: &Path) -> Result<Value, String> {
    paths.ensure_state_dirs()?;
    if paths.engine_marker.exists() {
        return Err(format!(
            "{} already has an engine marker; refusing to overwrite an initialized ledger",
            paths.engine_marker.display()
        ));
    }
    if !paths.database_root.is_dir() {
        return Err(format!(
            "legacy database is missing: {}",
            paths.database_root.display()
        ));
    }
    let bundle_bytes = fs::read(bundle_path)
        .map_err(|error| format!("read migration bundle {}: {error}", bundle_path.display()))?;
    let bundle: MigrationBundle = serde_json::from_slice(&bundle_bytes)
        .map_err(|error| format!("parse migration bundle {}: {error}", bundle_path.display()))?;
    validate_migration_bundle(paths, &bundle)?;
    let bundle_sha256 = hex_sha256(&bundle_bytes);
    let backups = paths.state_root.join("backups");
    fs::create_dir_all(&backups)
        .map_err(|error| format!("create migration backups {}: {error}", backups.display()))?;
    let source_key = bundle
        .source_database_sha256_after_close
        .get(..16)
        .ok_or_else(|| "migration source hash is too short".to_string())?;
    let backup = backups.join(format!("surrealdb-v2-{source_key}"));
    if backup.exists() {
        return Err(format!(
            "migration backup already exists: {}",
            backup.display()
        ));
    }
    fs::rename(&paths.database_root, &backup).map_err(|error| {
        format!(
            "move legacy database {} to {}: {error}",
            paths.database_root.display(),
            backup.display()
        )
    })?;
    let migrated_db = match import_bundle_to(&paths.database_root, &bundle).await {
        Ok(db) => db,
        Err(error) => {
            let failed_v3 = paths
                .state_root
                .join(format!("surrealdb-v3-failed-{}", uuid::Uuid::new_v4()));
            let move_failed = fs::rename(&paths.database_root, &failed_v3);
            let rollback = fs::rename(&backup, &paths.database_root);
            return Err(format!(
                "migrate exported rows failed: {error}; preserve-failed-v3={}; legacy rollback={}",
                move_failed
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|move_error| move_error.to_string()),
                rollback
                    .map(|_| "ok".to_string())
                    .unwrap_or_else(|rollback_error| rollback_error.to_string())
            ));
        }
    };
    if let Err(error) = write_engine_marker(&paths.engine_marker) {
        let failed_v3 = paths
            .state_root
            .join(format!("surrealdb-v3-failed-{}", uuid::Uuid::new_v4()));
        let move_new = fs::rename(&paths.database_root, &failed_v3);
        let rollback = fs::rename(&backup, &paths.database_root);
        return Err(format!(
            "write engine marker failed: {error}; move-new={}; legacy rollback={}",
            move_new
                .map(|_| "ok".to_string())
                .unwrap_or_else(|move_error| move_error.to_string()),
            rollback
                .map(|_| "ok".to_string())
                .unwrap_or_else(|rollback_error| rollback_error.to_string())
        ));
    }
    let _ = migrated_db;

    let receipt = json!({
        "status": "migrated",
        "source_engine": bundle.source_engine,
        "target_engine": "surrealdb",
        "target_engine_version": ENGINE_VERSION,
        "bundle_sha256": bundle_sha256,
        "source_database_sha256_before_open": bundle.source_database_sha256_before_open,
        "source_database_sha256_after_close": bundle.source_database_sha256_after_close,
        "proposal_count": bundle.proposals.len(),
        "capture_count": bundle.captures.len(),
        "rejection_count": bundle.rejections.len(),
        "proposal_id_set_sha256": digest_ids(bundle.proposals.iter().map(|row| row.proposal_id.as_str())),
        "capture_id_set_sha256": digest_ids(bundle.captures.iter().map(|row| row.source_id.as_str())),
        "rejection_id_set_sha256": digest_ids(bundle.rejections.iter().map(|row| row.audit_id.as_str())),
        "backup": backup,
        "database_root": paths.database_root,
        "canonical_fact_written": false,
    });
    let receipt_path = paths
        .state_root
        .join("migrations")
        .join(format!("migration-v2-to-v3-{}.json", &bundle_sha256[..20]));
    write_json_atomic(&receipt_path, &receipt)?;
    let mut output = receipt.as_object().cloned().unwrap_or_default();
    output.insert("receipt_path".to_string(), json!(receipt_path));
    Ok(Value::Object(output))
}

/// Import an independently verified v2 neutral export into a new v3 ledger.
/// Unlike `migrate-v2-export`, this path is for a relocated project whose
/// legacy database remains preserved at its old project root.
async fn import_v2_export(paths: &LedgerPaths, bundle_path: &Path) -> Result<Value, String> {
    paths.ensure_state_dirs()?;
    if paths.engine_marker.exists() {
        return Err(format!(
            "{} already has an engine marker; refusing to overwrite an initialized ledger",
            paths.engine_marker.display()
        ));
    }
    let database_has_files = paths.database_root.is_dir()
        && fs::read_dir(&paths.database_root)
            .map_err(|error| format!("read {}: {error}", paths.database_root.display()))?
            .next()
            .is_some();
    if database_has_files {
        return Err(format!(
            "target database is not empty: {}",
            paths.database_root.display()
        ));
    }
    let bundle_bytes = fs::read(bundle_path)
        .map_err(|error| format!("read migration bundle {}: {error}", bundle_path.display()))?;
    let bundle: MigrationBundle = serde_json::from_slice(&bundle_bytes)
        .map_err(|error| format!("parse migration bundle {}: {error}", bundle_path.display()))?;
    validate_migration_bundle(paths, &bundle)?;
    let bundle_sha256 = hex_sha256(&bundle_bytes);
    let imported_db = match import_bundle_to(&paths.database_root, &bundle).await {
        Ok(db) => db,
        Err(error) => {
            let failed_v3 = paths
                .state_root
                .join(format!("surrealdb-v3-failed-{}", uuid::Uuid::new_v4()));
            let preserve = if paths.database_root.exists() {
                fs::rename(&paths.database_root, &failed_v3)
                    .map(|_| format!("preserved at {}", failed_v3.display()))
                    .unwrap_or_else(|move_error| format!("preserve failed: {move_error}"))
            } else {
                "no target database was created".to_string()
            };
            return Err(format!("import exported rows failed: {error}; {preserve}"));
        }
    };
    if let Err(error) = write_engine_marker(&paths.engine_marker) {
        let failed_v3 = paths
            .state_root
            .join(format!("surrealdb-v3-failed-{}", uuid::Uuid::new_v4()));
        let preserve = fs::rename(&paths.database_root, &failed_v3)
            .map(|_| format!("preserved at {}", failed_v3.display()))
            .unwrap_or_else(|move_error| format!("preserve failed: {move_error}"));
        return Err(format!("write engine marker failed: {error}; {preserve}"));
    }
    let _ = imported_db;

    let receipt = json!({
        "status": "imported",
        "source_engine": bundle.source_engine,
        "target_engine": "surrealdb",
        "target_engine_version": ENGINE_VERSION,
        "bundle_sha256": bundle_sha256,
        "source_database_sha256_before_open": bundle.source_database_sha256_before_open,
        "source_database_sha256_after_close": bundle.source_database_sha256_after_close,
        "proposal_count": bundle.proposals.len(),
        "capture_count": bundle.captures.len(),
        "rejection_count": bundle.rejections.len(),
        "proposal_row_set_sha256": digest_rows(&bundle.proposals)?,
        "capture_row_set_sha256": digest_rows(&bundle.captures)?,
        "rejection_row_set_sha256": digest_rows(&bundle.rejections)?,
        "legacy_database_moved": false,
        "database_root": paths.database_root,
        "canonical_fact_written": false,
    });
    let receipt_path = paths
        .state_root
        .join("migrations")
        .join(format!("import-v2-to-v3-{}.json", &bundle_sha256[..20]));
    write_json_atomic(&receipt_path, &receipt)?;
    let mut output = receipt.as_object().cloned().unwrap_or_default();
    output.insert("receipt_path".to_string(), json!(receipt_path));
    Ok(Value::Object(output))
}

async fn verify_v2_export(paths: &LedgerPaths, bundle_path: &Path) -> Result<Value, String> {
    let bundle_bytes = fs::read(bundle_path)
        .map_err(|error| format!("read migration bundle {}: {error}", bundle_path.display()))?;
    let bundle: MigrationBundle = serde_json::from_slice(&bundle_bytes)
        .map_err(|error| format!("parse migration bundle {}: {error}", bundle_path.display()))?;
    validate_migration_bundle(paths, &bundle)?;
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let (proposals, captures, rejections) =
        verify_bundle_rows(&db, &paths.database_root, &bundle).await?;
    Ok(json!({
        "status": "verified",
        "engine_version": ENGINE_VERSION,
        "bundle_sha256": hex_sha256(&bundle_bytes),
        "proposal_count": proposals.len(),
        "capture_count": captures.len(),
        "rejection_count": rejections.len(),
        "proposal_row_set_sha256": digest_rows(&proposals)?,
        "capture_row_set_sha256": digest_rows(&captures)?,
        "rejection_row_set_sha256": digest_rows(&rejections)?,
        "database_root": paths.database_root,
        "capture_root": paths.captures_root,
        "canonical_fact_written": false,
    }))
}

fn validate_migration_bundle(paths: &LedgerPaths, bundle: &MigrationBundle) -> Result<(), String> {
    if bundle.format != MIGRATION_FORMAT || bundle.source_engine != "surrealdb-2.6.5" {
        return Err("migration bundle format/source engine is not supported".to_string());
    }
    if bundle.proposals.len() != bundle.captures.len() {
        return Err(format!(
            "migration proposal/capture mismatch: {} vs {}",
            bundle.proposals.len(),
            bundle.captures.len()
        ));
    }
    for (label, hash) in [
        (
            "source_database_sha256_before_open",
            &bundle.source_database_sha256_before_open,
        ),
        (
            "source_database_sha256_after_close",
            &bundle.source_database_sha256_after_close,
        ),
    ] {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("migration bundle has an invalid {label}"));
        }
    }
    let capture_ids = bundle
        .captures
        .iter()
        .map(|row| row.source_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if capture_ids.len() != bundle.captures.len() {
        return Err("migration bundle contains duplicate source IDs".to_string());
    }
    let proposal_ids = bundle
        .proposals
        .iter()
        .map(|row| row.proposal_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if proposal_ids.len() != bundle.proposals.len() {
        return Err("migration bundle contains duplicate proposal IDs".to_string());
    }
    let rejection_ids = bundle
        .rejections
        .iter()
        .map(|row| row.audit_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if rejection_ids.len() != bundle.rejections.len() {
        return Err("migration bundle contains duplicate rejection IDs".to_string());
    }
    for proposal in &bundle.proposals {
        if !capture_ids.contains(proposal.source_id.as_str()) {
            return Err(format!(
                "proposal {} has no captured source",
                proposal.proposal_id
            ));
        }
    }
    for capture in &bundle.captures {
        if capture.content_sha256.len() != 64
            || !capture
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "source {} has an invalid SHA-256",
                capture.source_id
            ));
        }
        let capture_path = paths.captures_root.join(format!(
            "{}.bin",
            capture.content_sha256.to_ascii_lowercase()
        ));
        let bytes = fs::read(&capture_path)
            .map_err(|error| format!("read captured source {}: {error}", capture_path.display()))?;
        if bytes.len() as u64 != capture.byte_length
            || hex_sha256(&bytes) != capture.content_sha256.to_ascii_lowercase()
        {
            return Err(format!(
                "captured source {} fails length/hash verification",
                capture.source_id
            ));
        }
    }
    Ok(())
}

async fn import_bundle_to(
    database_root: &Path,
    bundle: &MigrationBundle,
) -> Result<EmbeddedDb, String> {
    let db = connect_uncached_embedded_database(database_root)
        .await
        .map_err(|error| format!("create migrated SurrealDB: {error}"))?;
    db.use_ns(NAMESPACE)
        .use_db(DATABASE)
        .await
        .map_err(|error| format!("select migrated namespace: {error}"))?;
    initialize_schema(&db).await?;
    let captures_by_source = bundle
        .captures
        .iter()
        .map(|row| (row.source_id.as_str(), row))
        .collect::<std::collections::BTreeMap<_, _>>();
    let captures = bundle
        .captures
        .iter()
        .cloned()
        .map(|mut capture| {
            if capture.capture_id.is_empty() {
                capture.capture_id = capture.source_id.clone();
            }
            capture.capture_path = capture_relative_path(&capture.content_sha256)?
                .to_string_lossy()
                .to_string();
            Ok(capture)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let proposals = bundle
        .proposals
        .iter()
        .cloned()
        .map(|mut proposal| {
            let capture = captures_by_source
                .get(proposal.source_id.as_str())
                .ok_or_else(|| format!("missing capture for {}", proposal.proposal_id))?;
            if proposal.capture_id.is_empty() {
                proposal.capture_id = if capture.capture_id.is_empty() {
                    capture.source_id.clone()
                } else {
                    capture.capture_id.clone()
                };
            }
            if proposal.canonical_url.is_empty() {
                proposal.canonical_url = capture.canonical_url.clone();
            }
            Ok(proposal)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let receipts = migration_receipts(&proposals, &bundle.rejections, "surrealdb-v2-to-v3");
    persist_rows_transaction(
        &db,
        &proposals,
        &captures,
        &bundle.rejections,
        &receipts,
        false,
    )
    .await?;
    verify_bundle_rows(&db, database_root, bundle).await?;
    Ok(db)
}

async fn verify_bundle_rows(
    db: &EmbeddedDb,
    database_root: &Path,
    bundle: &MigrationBundle,
) -> Result<(Vec<ProposalRow>, Vec<CaptureRow>, Vec<RejectionRow>), String> {
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, capture_id, canonical_url, source_kind, state FROM source_proposal; \
             SELECT capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture; \
             SELECT audit_id, job_id, code, detail FROM rejection_audit;",
        )
        .await
        .map_err(|error| format!("verify migrated ledger: {error}"))?;
    let proposals: Vec<ProposalRow> = response
        .take(0)
        .map_err(|error| format!("verify proposal rows: {error}"))?;
    let captures: Vec<CaptureRow> = response
        .take(1)
        .map_err(|error| format!("verify capture rows: {error}"))?;
    let rejections: Vec<RejectionRow> = response
        .take(2)
        .map_err(|error| format!("verify rejection rows: {error}"))?;
    let mut expected_proposals = bundle.proposals.clone();
    let mut expected_captures = bundle
        .captures
        .iter()
        .cloned()
        .map(|mut row| {
            if row.capture_id.is_empty() {
                row.capture_id = row.source_id.clone();
            }
            row.capture_path = capture_relative_path(&row.content_sha256)
                .expect("validated migration capture hash")
                .to_string_lossy()
                .to_string();
            row
        })
        .collect::<Vec<_>>();
    for proposal in &mut expected_proposals {
        if proposal.capture_id.is_empty() {
            proposal.capture_id = proposal.source_id.clone();
        }
        if proposal.canonical_url.is_empty() {
            if let Some(capture) = expected_captures
                .iter()
                .find(|capture| capture.capture_id == proposal.capture_id)
            {
                proposal.canonical_url = capture.canonical_url.clone();
            }
        }
    }
    let mut expected_rejections = bundle.rejections.clone();
    let mut proposals = proposals;
    let mut captures = captures;
    let mut rejections = rejections;
    expected_proposals.sort();
    expected_captures.sort();
    expected_rejections.sort();
    proposals.sort();
    captures.sort();
    rejections.sort();
    if expected_proposals != proposals
        || expected_captures != captures
        || expected_rejections != rejections
    {
        return Err("migrated ledger rows do not exactly match the export bundle".to_string());
    }
    Ok((proposals, captures, rejections))
}

async fn load_captured_sources_async(paths: &LedgerPaths) -> Result<Vec<CapturedSource>, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let mut response = db
        .query(
            "SELECT proposal_id, job_id, source_id, capture_id, canonical_url, source_kind, state FROM source_proposal; \
             SELECT capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture;",
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
        .map(|capture| (capture.capture_id.clone(), capture))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut rows = proposals
        .into_iter()
        .map(|proposal| -> Result<CapturedSource, String> {
            let capture = captures.get(&proposal.capture_id).cloned().ok_or_else(|| {
                format!(
                    "proposal {} references missing capture {}",
                    proposal.proposal_id, proposal.capture_id
                )
            })?;
            if capture.source_id != proposal.source_id
                || capture.canonical_url != proposal.canonical_url
            {
                return Err(format!(
                    "proposal {} does not match capture {}",
                    proposal.proposal_id, proposal.capture_id
                ));
            }
            let capture_path = paths
                .state_root
                .join(capture_relative_path(&capture.content_sha256)?)
                .to_string_lossy()
                .to_string();
            Ok(CapturedSource {
                proposal_id: proposal.proposal_id,
                job_id: proposal.job_id,
                source_id: proposal.source_id,
                source_kind: proposal.source_kind,
                state: proposal.state,
                canonical_url: capture.canonical_url,
                content_sha256: capture.content_sha256,
                capture_path,
                byte_length: capture.byte_length,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    rows.sort_by(|left, right| right.proposal_id.cmp(&left.proposal_id));
    Ok(rows)
}

async fn list_sources(paths: &LedgerPaths, job_prefix: Option<&str>) -> Result<Value, String> {
    let mut rows = load_captured_sources_async(paths).await?;
    if let Some(prefix) = job_prefix {
        rows.retain(|row| row.job_id.starts_with(prefix));
    }
    Ok(json!({
        "status": "ok",
        "project_root": paths.project_root,
        "job_prefix": job_prefix,
        "source_count": rows.len(),
        "sources": rows,
    }))
}

async fn propose_source(
    paths: &LedgerPaths,
    job_id: &str,
    raw_url: &str,
    source_kind: &str,
) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let client = source_client()?;
    propose_source_with(&db, paths, &client, job_id, raw_url, source_kind).await
}

async fn propose_sources(
    paths: &LedgerPaths,
    job_prefix: &str,
    urls: &[String],
    source_kind: &str,
) -> Result<Value, String> {
    let db = open(paths).await?;
    initialize_schema(&db).await?;
    let client = source_client()?;
    let mut results = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        let job_id = format!("{job_prefix}-{:04}", index + 1);
        validate_job_id(&job_id)?;
        results.push(propose_source_with(&db, paths, &client, &job_id, url, source_kind).await?);
    }
    let captured = results
        .iter()
        .filter(|result| result.get("status").and_then(Value::as_str) == Some("captured"))
        .count();
    let rejected = results.len() - captured;
    let result_bytes = serde_json::to_vec(&results)
        .map_err(|error| format!("serialize proposal batch terminal state: {error}"))?;
    let result_digest = hex_sha256(&result_bytes);
    let terminal_id = format!("KTL-BATCH-{}", &result_digest[..20]);
    let receipt = receipt_row(
        "batch",
        job_prefix,
        &terminal_id,
        results.len() as u64,
        captured as u64,
        rejected as u64,
    );
    persist_receipt(&db, &receipt).await?;
    Ok(json!({
        "status": if rejected == 0 { "captured" } else { "completed-with-rejections" },
        "job_prefix": job_prefix,
        "source_kind": source_kind,
        "requested": results.len(),
        "captured": captured,
        "rejected": rejected,
        "receipt_id": receipt.receipt_id,
        "canonical_fact_written": false,
        "results": results,
    }))
}

fn source_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("FacialTimelineLedger/1")
        .connect_timeout(SOURCE_CONNECT_TIMEOUT)
        .timeout(SOURCE_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build source client: {error}"))
}

async fn propose_source_with(
    db: &EmbeddedDb,
    paths: &LedgerPaths,
    client: &reqwest::Client,
    job_id: &str,
    raw_url: &str,
    source_kind: &str,
) -> Result<Value, String> {
    let url = match Url::parse(raw_url)
        .map_err(|error| error.to_string())
        .and_then(validate_source_url)
    {
        Ok(url) => url,
        Err(error) => return rejection(&db, job_id, "INVALID_URL", &error).await,
    };
    let response = match fetch_with_network_policy(client, url).await {
        Ok(response) => response,
        Err((code, detail)) => return rejection(&db, job_id, code, &detail).await,
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
    if response
        .content_length()
        .is_some_and(|length| length > SOURCE_MAX_BYTES as u64)
    {
        return rejection(
            &db,
            job_id,
            "SOURCE_BODY_TOO_LARGE",
            &format!("declared response length exceeds {SOURCE_MAX_BYTES} bytes"),
        )
        .await;
    }
    let mut response = response;
    let mut body = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(chunk) => chunk,
            Err(error) => {
                return rejection(&db, job_id, "SOURCE_BODY_UNREADABLE", &error.to_string()).await
            }
        };
        let Some(chunk) = chunk else { break };
        if body.len().saturating_add(chunk.len()) > SOURCE_MAX_BYTES {
            return rejection(
                &db,
                job_id,
                "SOURCE_BODY_TOO_LARGE",
                &format!("streamed response exceeds {SOURCE_MAX_BYTES} bytes"),
            )
            .await;
        }
        body.extend_from_slice(&chunk);
    }
    let hash = hex_sha256(&body);
    let relative_capture_path = capture_relative_path(&hash)?;
    let capture_path = paths.state_root.join(&relative_capture_path);
    if !capture_path.exists() {
        fs::write(&capture_path, &body)
            .map_err(|error| format!("write source capture: {error}"))?;
    } else {
        let existing = fs::read(&capture_path)
            .map_err(|error| format!("verify existing source capture: {error}"))?;
        if existing.len() != body.len() || hex_sha256(&existing) != hash {
            return rejection(
                &db,
                job_id,
                "CAPTURE_INTEGRITY_CONFLICT",
                "existing content-addressed capture does not match its SHA-256 path",
            )
            .await;
        }
    }
    let source_id = source_id_for_url(&canonical_url);
    let capture_id = capture_id_for(&source_id, &hash);
    let proposal_id = proposal_id_for(job_id, &source_id, &capture_id);
    let receipt = receipt_row("submission", job_id, &proposal_id, 1, 1, 0);
    db.query(
        "
        BEGIN TRANSACTION;
        UPSERT type::record('source_capture', $capture_id) SET capture_id = $capture_id, source_id = $source_id, canonical_url = $url, content_sha256 = $hash, capture_path = $capture_path, byte_length = $bytes;
        UPSERT type::record('source_proposal', $proposal_id) SET proposal_id = $proposal_id, job_id = $job_id, source_id = $source_id, capture_id = $capture_id, canonical_url = $url, source_kind = $source_kind, state = 'captured';
        UPSERT type::record('ingestion_receipt', $receipt_id) CONTENT $receipt;
        COMMIT TRANSACTION;
        ",
    )
    .bind(("source_id", source_id.clone()))
    .bind(("capture_id", capture_id.clone()))
    .bind(("proposal_id", proposal_id.clone()))
    .bind(("url", canonical_url.clone()))
    .bind(("hash", hash.clone()))
    .bind((
        "capture_path",
        relative_capture_path.to_string_lossy().to_string(),
    ))
    .bind(("bytes", body.len() as u64))
    .bind(("job_id", job_id.to_string()))
    .bind(("source_kind", source_kind.to_string()))
    .bind(("receipt_id", receipt.receipt_id.clone()))
    .bind(("receipt", receipt))
    .await
    .map_err(|error| format!("persist source proposal: {error}"))?
    .check()
    .map_err(|error| format!("persist source proposal: {error}"))?;
    Ok(json!({
        "status": "captured",
        "job_id": job_id,
        "proposal_id": proposal_id,
        "source_id": source_id,
        "capture_id": capture_id,
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

fn validate_source_url(url: Url) -> Result<Url, String> {
    let url = require_https(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| "source URL has no host".to_string())?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        validate_public_ip(ip)?;
    } else {
        let domain = host.trim_end_matches('.').to_ascii_lowercase();
        if domain == "localhost"
            || domain.ends_with(".localhost")
            || domain.ends_with(".local")
            || domain.ends_with(".internal")
        {
            return Err("source URL host is blocked by the external-network policy".to_string());
        }
    }
    Ok(url)
}

fn validate_public_ip(ip: std::net::IpAddr) -> Result<(), String> {
    let blocked = match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
        }
    };
    if blocked {
        Err("source URL resolved to an address blocked by the external-network policy".to_string())
    } else {
        Ok(())
    }
}

async fn fetch_with_network_policy(
    client: &reqwest::Client,
    mut url: Url,
) -> Result<reqwest::Response, (&'static str, String)> {
    for hop in 0..=10 {
        url = validate_source_url(url).map_err(|error| ("SOURCE_NETWORK_POLICY", error))?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| ("SOURCE_UNREACHABLE", error.to_string()))?;
        let remote = response.remote_addr().ok_or_else(|| {
            (
                "SOURCE_NETWORK_POLICY",
                "HTTP transport did not expose the remote address".to_string(),
            )
        })?;
        validate_public_ip(remote.ip()).map_err(|error| ("SOURCE_NETWORK_POLICY", error))?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if hop == 10 {
            return Err((
                "SOURCE_REDIRECT_LIMIT",
                "source exceeded 10 redirects".to_string(),
            ));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| {
                (
                    "SOURCE_REDIRECT_INVALID",
                    "redirect response has no Location header".to_string(),
                )
            })?
            .to_str()
            .map_err(|error| ("SOURCE_REDIRECT_INVALID", error.to_string()))?;
        url = response
            .url()
            .join(location)
            .map_err(|error| ("SOURCE_REDIRECT_INVALID", error.to_string()))?;
    }
    unreachable!("bounded redirect loop returns on every terminal branch")
}

fn capture_relative_path(content_sha256: &str) -> Result<PathBuf, String> {
    if content_sha256.len() != 64 || !content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("capture SHA-256 must contain exactly 64 hexadecimal characters".to_string());
    }
    Ok(PathBuf::from("captures").join(format!("{}.bin", content_sha256.to_ascii_lowercase())))
}

fn source_id_for_url(canonical_url: &str) -> String {
    let digest = hex_sha256(canonical_url.as_bytes());
    format!("KTL-SRC-{}", &digest[..20])
}

fn capture_id_for(source_id: &str, content_sha256: &str) -> String {
    let digest = hex_sha256(format!("{source_id}\n{content_sha256}").as_bytes());
    format!("KTL-CAP-{}", &digest[..20])
}

fn proposal_id_for(job_id: &str, source_id: &str, capture_id: &str) -> String {
    let digest = hex_sha256(format!("{job_id}\n{source_id}\n{capture_id}").as_bytes());
    format!("KTL-PROP-{}", &digest[..20])
}

fn receipt_id_for(receipt_kind: &str, job_scope: &str, terminal_id: &str) -> String {
    let digest = hex_sha256(format!("{receipt_kind}\n{job_scope}\n{terminal_id}").as_bytes());
    format!("KTL-RCPT-{}", &digest[..20])
}

fn receipt_row(
    receipt_kind: &str,
    job_scope: &str,
    terminal_id: &str,
    requested: u64,
    captured: u64,
    rejected: u64,
) -> ReceiptRow {
    ReceiptRow {
        receipt_id: receipt_id_for(receipt_kind, job_scope, terminal_id),
        receipt_kind: receipt_kind.to_string(),
        job_scope: job_scope.to_string(),
        terminal_id: terminal_id.to_string(),
        requested,
        captured,
        rejected,
    }
}

async fn persist_receipt(db: &EmbeddedDb, receipt: &ReceiptRow) -> Result<(), String> {
    validate_receipts(std::slice::from_ref(receipt))?;
    db.query("UPSERT type::record('ingestion_receipt', $receipt_id) CONTENT $receipt;")
        .bind(("receipt_id", receipt.receipt_id.clone()))
        .bind(("receipt", receipt.clone()))
        .await
        .map_err(|error| format!("persist ingestion receipt: {error}"))?
        .check()
        .map_err(|error| format!("persist ingestion receipt: {error}"))?;
    Ok(())
}

async fn rejection(
    db: &EmbeddedDb,
    job_id: &str,
    code: &str,
    detail: &str,
) -> Result<Value, String> {
    let digest = hex_sha256(format!("{job_id}\n{code}\n{detail}").as_bytes());
    let audit_id = format!("KTL-REJ-{}", &digest[..20]);
    let receipt = receipt_row("submission", job_id, &audit_id, 1, 0, 1);
    db.query("BEGIN TRANSACTION; UPSERT type::record('rejection_audit', $audit_id) SET audit_id = $audit_id, job_id = $job_id, code = $code, detail = $detail; UPSERT type::record('ingestion_receipt', $receipt_id) CONTENT $receipt; COMMIT TRANSACTION;")
        .bind(("audit_id", audit_id.clone()))
        .bind(("job_id", job_id.to_string()))
        .bind(("code", code.to_string()))
        .bind(("detail", detail.to_string()))
        .bind(("receipt_id", receipt.receipt_id.clone()))
        .bind(("receipt", receipt))
        .await
        .map_err(|error| format!("persist rejection audit: {error}"))?
        .check()
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
        "engine_version": ENGINE_VERSION,
    })
}

fn hex_sha256(input: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(input);
    format!("{:x}", digest.finalize())
}

fn digest_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let ordered = ids
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n");
    hex_sha256(ordered.as_bytes())
}

fn digest_rows<T: Serialize>(rows: &[T]) -> Result<String, String> {
    let bytes =
        serde_json::to_vec(rows).map_err(|error| format!("serialize row digest: {error}"))?;
    Ok(hex_sha256(&bytes))
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

    fn cleanup_project(root: PathBuf) {
        if let Ok(paths) = LedgerPaths::discover(&root) {
            if paths.database_root.is_dir() {
                surreal_store::wait_until_closed(&paths.database_root).unwrap();
            }
        }
        fs::remove_dir_all(root).unwrap();
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
    fn batch_flags_allow_only_repeated_urls() {
        let args = [
            "--project-root",
            "project",
            "--job-prefix",
            "IVE-2025-BATCH",
            "--source-kind",
            "official-group",
            "--url",
            "https://example.test/one",
            "--url",
            "https://example.test/two",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let parsed = parse_repeated_flags(&args, "url").unwrap();
        assert_eq!(parsed["url"].len(), 2);
        assert_eq!(parsed["job-prefix"], vec!["IVE-2025-BATCH"]);

        let duplicate_kind = [
            "--source-kind",
            "official-group",
            "--source-kind",
            "broadcaster",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        assert!(parse_repeated_flags(&duplicate_kind, "url").is_err());
    }

    #[test]
    fn rejects_non_https_urls() {
        assert!(require_https(Url::parse("file:///tmp/x").unwrap()).is_err());
        assert!(require_https(Url::parse("https://example.test/a").unwrap()).is_ok());
        assert!(validate_source_url(Url::parse("https://127.0.0.1/a").unwrap()).is_err());
        assert!(validate_source_url(Url::parse("https://localhost/a").unwrap()).is_err());
    }

    #[test]
    fn identical_content_keeps_distinct_url_and_job_identity() {
        let hash = hex_sha256(b"identical response bytes");
        let source_a = source_id_for_url("https://example.test/a");
        let source_b = source_id_for_url("https://example.test/b");
        assert_ne!(source_a, source_b);
        let capture_a = capture_id_for(&source_a, &hash);
        let capture_b = capture_id_for(&source_b, &hash);
        assert_ne!(capture_a, capture_b);
        assert_ne!(
            proposal_id_for("IVE-JOB-0001", &source_a, &capture_a),
            proposal_id_for("IVE-JOB-0002", &source_a, &capture_a)
        );
        assert_eq!(
            capture_relative_path(&hash).unwrap(),
            PathBuf::from("captures").join(format!("{hash}.bin"))
        );
    }

    #[test]
    fn rejects_same_major_but_non_exact_engine_marker() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        paths.ensure_state_dirs().unwrap();
        write_json_atomic(
            &paths.engine_marker,
            &json!({
                "engine": "surrealdb",
                "engine_version": "3.99.0",
                "schema_version": SCHEMA_VERSION,
            }),
        )
        .unwrap();
        assert!(require_current_engine(&paths).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_to_open_an_unmarked_nonempty_legacy_database() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        fs::create_dir_all(&paths.database_root).unwrap();
        fs::write(paths.database_root.join("legacy.data"), b"legacy").unwrap();

        let error = require_current_engine(&paths).unwrap_err();

        assert!(error.contains("unmarked legacy timeline ledger"));
        assert!(!paths.engine_marker.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_rejects_a_capture_hash_mismatch_before_database_swap() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        paths.ensure_state_dirs().unwrap();
        fs::create_dir_all(&paths.database_root).unwrap();
        fs::write(paths.database_root.join("legacy.data"), b"legacy").unwrap();
        let hash = hex_sha256(b"expected");
        fs::write(paths.captures_root.join(format!("{hash}.bin")), b"wrong").unwrap();
        let bundle = migration_fixture(&hash, b"expected".len() as u64);

        let error = validate_migration_bundle(&paths, &bundle).unwrap_err();

        assert!(error.contains("fails length/hash verification"));
        assert!(paths.database_root.join("legacy.data").exists());
        assert!(!paths.engine_marker.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrades_a_populated_schema_v1_database_through_a_verified_staging_swap() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        paths.ensure_state_dirs().unwrap();
        let captured = b"schema-v1-populated-capture";
        let hash = hex_sha256(captured);
        fs::write(paths.captures_root.join(format!("{hash}.bin")), captured).unwrap();
        write_json_atomic(
            &paths.engine_marker,
            &json!({
                "engine": "surrealdb",
                "engine_version": ENGINE_VERSION,
                "schema_version": LEGACY_SCHEMA_VERSION,
                "namespace": NAMESPACE,
                "database": DATABASE,
            }),
        )
        .unwrap();
        let legacy_db = surreal_store::open_database(
            &paths.database_root,
            DATABASE,
            LEGACY_SCHEMA_VERSION as u64,
        )
        .unwrap();
        run_async(async {
            legacy_db
                .query(
                    "DEFINE TABLE OVERWRITE ledger_meta SCHEMALESS;
                     DEFINE TABLE OVERWRITE source_capture SCHEMALESS;
                     DEFINE TABLE OVERWRITE source_proposal SCHEMALESS;
                     DEFINE TABLE OVERWRITE rejection_audit SCHEMALESS;
                     UPSERT ledger_meta:schema SET version = $version, engine = 'surrealdb', engine_version = $engine_version, namespace = $namespace, database = $database;
                     UPSERT source_capture:legacy SET source_id = 'KTL-SRC-legacy-body', canonical_url = 'https://example.test/schema-v1', content_sha256 = $hash, capture_path = $legacy_path, byte_length = $bytes;
                     UPSERT source_proposal:legacy SET proposal_id = 'KTL-PROP-legacy-body', job_id = 'IVE-SCHEMA-V1', source_id = 'KTL-SRC-legacy-body', source_kind = 'platform', state = 'captured';
                     UPSERT rejection_audit:legacy SET audit_id = 'KTL-REJ-legacy', job_id = 'IVE-SCHEMA-V1-REJECT', code = 'SOURCE_UNREACHABLE', detail = 'fixture';",
                )
                .bind(("version", LEGACY_SCHEMA_VERSION))
                .bind(("engine_version", ENGINE_VERSION))
                .bind(("namespace", NAMESPACE))
                .bind(("database", DATABASE))
                .bind(("hash", hash.clone()))
                .bind(("legacy_path", "C:\\legacy\\absolute\\capture.bin"))
                .bind(("bytes", captured.len() as u64))
                .await
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();
        drop(legacy_db);
        surreal_store::wait_until_closed(&paths.database_root).unwrap();

        let receipt = run_async(upgrade_schema_v1_to_v2(&paths)).unwrap();
        assert_eq!(receipt["status"], "upgraded");
        assert_eq!(receipt["proposal_count"], 1);
        assert_eq!(receipt["capture_count"], 1);
        assert_eq!(receipt["rejection_count"], 1);
        assert_eq!(receipt["receipt_count"], 2);
        assert!(Path::new(receipt["predecessor_database"].as_str().unwrap()).is_dir());
        let marker: Value =
            serde_json::from_slice(&fs::read(&paths.engine_marker).unwrap()).unwrap();
        assert_eq!(marker["schema_version"], SCHEMA_VERSION);

        let doctor_receipt = run_async(doctor(&paths)).unwrap();
        assert_eq!(doctor_receipt["status"], "ok");
        assert_eq!(doctor_receipt["receipt_count"], 2);
        let db = run_async(open(&paths)).unwrap();
        let (proposals, captures, rejections, receipts) = run_async(load_all_rows(&db)).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(captures.len(), 1);
        assert_eq!(rejections.len(), 1);
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            captures[0].source_id,
            source_id_for_url("https://example.test/schema-v1")
        );
        assert_eq!(
            captures[0].capture_path,
            PathBuf::from("captures")
                .join(format!("{hash}.bin"))
                .to_string_lossy()
        );
        drop(db);

        let logical_path = root.join("schema-v2-logical.json");
        let export_receipt = run_async(export_logical(&paths, &logical_path)).unwrap();
        assert_eq!(export_receipt["receipt_count"], 2);
        let rebuild_root = temp_project();
        let rebuild_paths = LedgerPaths::discover(&rebuild_root).unwrap();
        rebuild_paths.ensure_state_dirs().unwrap();
        fs::write(
            rebuild_paths.captures_root.join(format!("{hash}.bin")),
            captured,
        )
        .unwrap();
        let rebuild_receipt = run_async(rebuild_logical(&rebuild_paths, &logical_path)).unwrap();
        assert_eq!(rebuild_receipt["proposal_count"], 1);
        assert_eq!(rebuild_receipt["capture_count"], 1);
        assert_eq!(rebuild_receipt["rejection_count"], 1);
        assert_eq!(rebuild_receipt["receipt_count"], 2);
        let rebuilt_doctor = run_async(doctor(&rebuild_paths)).unwrap();
        assert_eq!(rebuilt_doctor["receipt_count"], 2);
        cleanup_project(rebuild_root);
        cleanup_project(root);
    }

    #[test]
    fn migrates_a_verified_v2_export_and_preserves_exact_rows() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        paths.ensure_state_dirs().unwrap();
        fs::create_dir_all(&paths.database_root).unwrap();
        fs::write(paths.database_root.join("legacy.data"), b"legacy-database").unwrap();
        let captured = b"captured-source";
        let hash = hex_sha256(captured);
        fs::write(paths.captures_root.join(format!("{hash}.bin")), captured).unwrap();
        let bundle = migration_fixture(&hash, captured.len() as u64);
        let bundle_path = root.join("legacy-v2-export.json");
        fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

        let receipt = run_async(migrate_v2_export(&paths, &bundle_path)).unwrap();
        let ledger_status = run_async(status(&paths)).unwrap();

        assert_eq!(receipt["status"], "migrated");
        assert_eq!(receipt["proposal_count"], 1);
        assert_eq!(receipt["capture_count"], 1);
        assert_eq!(receipt["rejection_count"], 1);
        assert_eq!(
            receipt["proposal_id_set_sha256"],
            digest_ids(["KTL-PROP-fixture"])
        );
        assert_eq!(
            receipt["capture_id_set_sha256"],
            digest_ids(["KTL-SRC-fixture"])
        );
        assert_eq!(
            receipt["rejection_id_set_sha256"],
            digest_ids(["KTL-REJ-fixture"])
        );
        assert_eq!(ledger_status["proposal_count"], 1);
        assert_eq!(ledger_status["rejection_count"], 1);
        assert_eq!(ledger_status["receipt_count"], 2);
        assert_eq!(ledger_status["engine_version"], ENGINE_VERSION);
        assert!(paths.engine_marker.is_file());
        assert!(paths
            .state_root
            .join("backups")
            .join("surrealdb-v2-2222222222222222")
            .join("legacy.data")
            .is_file());
        let loaded = load_captured_sources(&root).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].proposal_id, "KTL-PROP-fixture");
        assert_eq!(loaded[0].source_id, "KTL-SRC-fixture");
        assert_eq!(
            Path::new(&loaded[0].capture_path),
            paths.captures_root.join(format!("{hash}.bin"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_a_verified_v2_export_into_an_empty_relocated_project() {
        let root = temp_project();
        let paths = LedgerPaths::discover(&root).unwrap();
        paths.ensure_state_dirs().unwrap();
        let captured = b"captured-source";
        let hash = hex_sha256(captured);
        fs::write(paths.captures_root.join(format!("{hash}.bin")), captured).unwrap();
        let bundle = migration_fixture(&hash, captured.len() as u64);
        let bundle_path = root.join("legacy-v2-export.json");
        fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

        let receipt = run_async(import_v2_export(&paths, &bundle_path)).unwrap();
        let loaded = load_captured_sources(&root).unwrap();

        assert_eq!(receipt["status"], "imported");
        assert_eq!(receipt["proposal_count"], 1);
        assert_eq!(receipt["capture_count"], 1);
        assert_eq!(receipt["rejection_count"], 1);
        assert_eq!(receipt["legacy_database_moved"], false);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].proposal_id, "KTL-PROP-fixture");
        assert_eq!(loaded[0].source_id, "KTL-SRC-fixture");
        assert_eq!(loaded[0].canonical_url, "https://example.test/source");
        assert_eq!(loaded[0].content_sha256, hash);
        assert_eq!(loaded[0].byte_length, captured.len() as u64);
        assert!(paths.engine_marker.is_file());
    }

    fn migration_fixture(content_sha256: &str, byte_length: u64) -> MigrationBundle {
        MigrationBundle {
            format: MIGRATION_FORMAT.to_string(),
            source_engine: "surrealdb-2.6.5".to_string(),
            source_database_sha256_before_open: "1".repeat(64),
            source_database_sha256_after_close: "2".repeat(64),
            proposals: vec![ProposalRow {
                proposal_id: "KTL-PROP-fixture".to_string(),
                job_id: "IVE-FIXTURE".to_string(),
                source_id: "KTL-SRC-fixture".to_string(),
                capture_id: String::new(),
                canonical_url: String::new(),
                source_kind: "platform".to_string(),
                state: "captured".to_string(),
            }],
            captures: vec![CaptureRow {
                capture_id: String::new(),
                source_id: "KTL-SRC-fixture".to_string(),
                canonical_url: "https://example.test/source".to_string(),
                content_sha256: content_sha256.to_string(),
                capture_path: "legacy-absolute-path".to_string(),
                byte_length,
            }],
            rejections: vec![RejectionRow {
                audit_id: "KTL-REJ-fixture".to_string(),
                job_id: "IVE-FIXTURE-REJECT".to_string(),
                code: "SOURCE_UNREACHABLE".to_string(),
                detail: "fixture".to_string(),
            }],
        }
    }
}
