//! Isolated SDK evaluation only; never establishes application compatibility.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, time::{Duration, Instant}};
use surrealdb::{Surreal, engine::local::{Db, SurrealKv}, types::SurrealValue};

const LEDGER_SOURCE: &str = include_str!("../../../src/timeline_ledger.rs");
const GUARD: &str = "facial-engine-compatibility-generated-fixture-v1";
const OLD_CLI: &str = "3b97d283589ccce5511d7027557d1ce208c5bbdd7e809337afc14136c934648b";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Serialize, Deserialize, SurrealValue)]
struct Proposal { proposal_id: String, job_id: String, source_id: String, capture_id: String, canonical_url: String, source_kind: String, state: String }
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
struct Capture { capture_id: String, source_id: String, canonical_url: String, content_sha256: String, capture_path: String, byte_length: u64 }
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
struct Rejection { audit_id: String, job_id: String, code: String, detail: String }
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
struct Receipt { receipt_id: String, receipt_kind: String, job_scope: String, terminal_id: String, requested: u64, captured: u64, rejected: u64 }
#[derive(Debug, Serialize, Deserialize, SurrealValue)]
struct Meta { version: u32, engine: String, engine_version: String, namespace: String, database: String }

fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn reject_raw_reparses(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::CurDir)) { return Err("absolute lexical fixture path required".into()); }
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        if meta.file_type().is_symlink() { return Err("raw path symlink rejected".into()); }
        #[cfg(windows)] {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 { return Err("raw path reparse ancestor rejected".into()); }
        }
    }
    Ok(())
}
fn authorized_root(raw: &Path, mode: &str) -> Result<PathBuf> {
    reject_raw_reparses(raw)?;
    let exe = std::env::current_exe()?;
    reject_raw_reparses(&exe)?;
    let release = exe.parent().ok_or("executable parent missing")?;
    let cargo = release.parent().ok_or("cargo parent missing")?;
    let artifacts = cargo.parent().ok_or("artifacts parent missing")?;
    if release.file_name().and_then(|s| s.to_str()) != Some("release") || cargo.file_name().and_then(|s| s.to_str()) != Some("cargo") || artifacts.file_name().and_then(|s| s.to_str()) != Some("build-artifacts") { return Err("candidate executable outside canonical guarded release path".into()); }
    let dbase = artifacts.join("wp087-engine-candidate");
    let cbase = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA missing")?).join("Temp/facial-installer-verify-bea764ea1d7b4beeb4cdce35c2abf452");
    let parent = raw.parent().ok_or("fixture parent missing")?;
    if parent != dbase && !(mode == "fresh-schema" && parent == cbase) { return Err("fixture outside approved candidate boundaries or C copy mode".into()); }
    let name = raw.file_name().and_then(|s| s.to_str()).ok_or("fixture name missing")?;
    let id = name.strip_prefix("seed-").or_else(|| name.strip_prefix("fresh-")).unwrap_or(name).replace('-', "");
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("generated GUID fixture name required".into()); }
    Ok(fs::canonicalize(raw)?)
}
fn confined(root: &Path, path: &Path) -> Result<PathBuf> {
    let actual = fs::canonicalize(path)?;
    if !actual.starts_with(root) { return Err("fixture path escapes authorized root".into()); }
    let mut cursor = path.to_path_buf();
    while cursor.starts_with(root) {
        let meta = fs::symlink_metadata(&cursor)?;
        if meta.file_type().is_symlink() { return Err("fixture symlink rejected".into()); }
        #[cfg(windows)] {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 { return Err("fixture reparse point rejected".into()); }
        }
        if cursor == root { break; }
        if !cursor.pop() { break; }
    }
    Ok(actual)
}
fn tree_hash(root: &Path, dir: &Path) -> Result<String> {
    fn visit(root: &Path, base: &Path, dir: &Path, out: &mut Vec<(String,String)>) -> Result<()> {
        confined(root, dir)?;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path(); confined(root, &path)?;
            if path.is_dir() { visit(root, base, &path, out)?; }
            else { out.push((path.strip_prefix(base)?.to_string_lossy().replace('\\', "/"), digest(&fs::read(&path)?))); }
        }
        Ok(())
    }
    let mut rows = Vec::new(); visit(root, dir, dir, &mut rows)?; rows.sort();
    Ok(digest(&serde_json::to_vec(&rows)?))
}
fn copy_tree(root: &Path, source: &Path, target: &Path) -> Result<()> {
    confined(root, source)?;
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?; let path = entry.path(); confined(root, &path)?;
        let dest = target.join(entry.file_name());
        if path.is_dir() { copy_tree(root, &path, &dest)?; } else { fs::copy(&path, &dest)?; }
    }
    Ok(())
}
async fn open(path: &Path) -> Result<Surreal<Db>> {
    let value = path.to_string_lossy();
    let value = value.strip_prefix(r"\\?\").unwrap_or(&value).to_string();
    let db = Surreal::new::<SurrealKv>(value).sync("every").await?;
    db.use_ns("facial").use_db("timeline_ledger").await?;
    Ok(db)
}
async fn rows(db: &Surreal<Db>) -> Result<Value> {
    let mut q = db.query("SELECT proposal_id, job_id, source_id, capture_id, canonical_url, source_kind, state FROM source_proposal; SELECT capture_id, source_id, canonical_url, content_sha256, capture_path, byte_length FROM source_capture; SELECT audit_id, job_id, code, detail FROM rejection_audit; SELECT receipt_id, receipt_kind, job_scope, terminal_id, requested, captured, rejected FROM ingestion_receipt;").await?.check()?;
    let p: Vec<Proposal> = q.take(0)?; let c: Vec<Capture> = q.take(1)?;
    let r: Vec<Rejection> = q.take(2)?; let i: Vec<Receipt> = q.take(3)?;
    let mut result = json!({"proposals":p,"captures":c,"rejections":r,"receipts":i});
    for key in ["proposals","captures","rejections","receipts"] {
        result[key].as_array_mut().ok_or("row collection is not array")?.sort_by_key(|row| row.to_string());
    }
    Ok(result)
}
fn expected(root: &Path) -> Result<Value> {
    let manifest_path = root.join("populated-fixture.json"); confined(root, &manifest_path)?;
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
    if manifest["diagnostic_only"] != true || manifest["synthetic_fixture"] != true || manifest["legacy_hashes_are_fixture_inputs"] != true || manifest["cli_sha256"] != OLD_CLI { return Err("independent seed manifest contract mismatch".into()); }
    let path = root.join("canonical-324-logical.json"); confined(root, &path)?;
    for (field, wanted) in [("project_root", root.to_path_buf()), ("database_root", root.join(".facial/timeline-ledger/surrealdb")), ("expected_rows", path.clone())] {
        let supplied = PathBuf::from(manifest[field].as_str().ok_or("manifest path absent")?);
        reject_raw_reparses(&supplied)?;
        if fs::canonicalize(supplied)? != fs::canonicalize(wanted)? { return Err("manifest path binding mismatch".into()); }
    }
    let bytes = fs::read(path)?;
    if manifest["expected_rows_sha256"].as_str() != Some(digest(&bytes).as_str()) { return Err("independent old export digest mismatch".into()); }
    let results = manifest["results"].as_array().ok_or("independent command observations absent")?;
    if results.len() != 3 || results.iter().any(|r| r["exit_code"].as_i64() != Some(0) || r["pid"].as_u64().unwrap_or(0) == 0) { return Err("independent old CLI observations failed".into()); }
    for (observation, verb) in results.iter().zip(["import-v2-export", "doctor", "export-logical"]) {
        let command = observation["command"].as_array().ok_or("old command arguments absent")?;
        if command.get(1).and_then(Value::as_str) != Some("timeline-ledger") || command.get(2).and_then(Value::as_str) != Some(verb) || command.get(3).and_then(Value::as_str) != Some("--project-root") { return Err("old command route mismatch".into()); }
        let cli = PathBuf::from(command.first().and_then(Value::as_str).ok_or("old CLI path absent")?);
        reject_raw_reparses(&cli)?;
        if digest(&fs::read(cli)?) != OLD_CLI { return Err("observed old CLI binary digest changed".into()); }
        let project = PathBuf::from(command.get(4).and_then(Value::as_str).ok_or("old project path absent")?);
        reject_raw_reparses(&project)?;
        if fs::canonicalize(project)? != root { return Err("old command project binding mismatch".into()); }
        for (suffix, field) in [("stdout.json", "stdout_sha256"), ("stderr.txt", "stderr_sha256")] {
            let output = root.join(format!("{verb}.{suffix}")); confined(root, &output)?;
            if observation[field].as_str() != Some(digest(&fs::read(output)?).as_str()) { return Err("independent command output digest mismatch".into()); }
        }
    }
    let doctor: Value = serde_json::from_slice(&fs::read(root.join("doctor.stdout.json"))?)?;
    let export: Value = serde_json::from_slice(&fs::read(root.join("export-logical.stdout.json"))?)?;
    if doctor["status"] != "ok" || doctor["engine_version"] != "3.2.4" || doctor["schema"]["version"] != 2 || export["status"] != "exported" || export["sha256"] != manifest["expected_rows_sha256"] { return Err("independent canonical old read binding mismatch".into()); }
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if value["format"] != "facial-timeline-ledger-logical-v2" || value["engine_version"] != "3.2.4" || value["schema_version"] != 2 { return Err("independent old CLI export contract mismatch".into()); }
    let mut result = json!({});
    for key in ["proposals","captures","rejections","receipts"] {
        let list = value[key].as_array_mut().ok_or("expected rows absent")?;
        if list.is_empty() { return Err("populated compatibility collections required".into()); }
        list.sort_by_key(|row| row.to_string()); result[key] = value[key].clone();
        if manifest["observed_counts"][key].as_u64() != Some(result[key].as_array().unwrap().len() as u64) { return Err("seed observed count binding mismatch".into()); }
    }
    Ok(result)
}
async fn run(mode: &str, root: &Path) -> Result<Value> {
    let original = root.join(".facial/timeline-ledger/surrealdb");
    let copied = root.join("candidate-copy");
    if mode == "fresh-schema" {
        let fresh = root.join("candidate-fresh");
        if fresh.exists() { return Err("fresh target already exists; no uncertain replay".into()); }
        fs::create_dir(&fresh)?;
        let db = open(&fresh).await?;
        let engine_version = db.version().await?.to_string();
        let source = LEDGER_SOURCE.replace("\r\n", "\n");
        let sql = source.split_once("const LEDGER_SCHEMA_SQL: &str = \"").ok_or("schema source anchor absent")?.1.split_once("\";\n").ok_or("schema source end absent")?.0.to_string();
        if sql.matches("DEFINE ").count() != 42 || sql.matches("UPSERT ").count() != 1 { return Err("exact 43-statement schema contract changed".into()); }
        let start = Instant::now();
        tokio::time::timeout(Duration::from_secs(30), async {
            db.query(sql.clone()).bind(("version", 2u32)).bind(("engine_version", engine_version.clone())).bind(("namespace", "facial")).bind(("database", "timeline_ledger")).await?.check()?;
            Ok::<(), surrealdb::Error>(())
        }).await.map_err(|_| "schema deadline exceeded; commit outcome unknown, preserve root")??;
        let mut query = db.query("SELECT version, engine, engine_version, namespace, database FROM ledger_meta:schema;").await?.check()?;
        let metadata: Vec<Meta> = query.take(0)?;
        if metadata.len() != 1 || serde_json::to_value(&metadata[0])? != json!({"version":2,"engine":"surrealdb","engine_version":engine_version,"namespace":"facial","database":"timeline_ledger"}) { return Err("fresh schema canonical metadata mismatch".into()); }
        return Ok(json!({"mode":mode,"engine_version":engine_version,"schema_elapsed_us":start.elapsed().as_micros(),"schema_sql_sha256":digest(sql.as_bytes()),"mutating_statement_count":43}));
    }
    let mut wanted = expected(root)?; let original_hash = tree_hash(root, &original)?;
    if mode == "verify-written" {
        wanted["rejections"].as_array_mut().ok_or("rejections array absent")?.push(json!({"audit_id":"KTL-REJ-engine-candidate","job_id":"ENGINE-CANDIDATE","code":"ISOLATED_FIXTURE","detail":"3.3 write restart proof"}));
        wanted["rejections"].as_array_mut().unwrap().sort_by_key(|row| row.to_string());
    }
    if mode == "verify-copy" {
        if copied.exists() { return Err("copy target already exists; no uncertain replay".into()); }
        copy_tree(root, &original, &copied)?;
        if tree_hash(root, &copied)? != original_hash { return Err("physical copy digest differs before opening".into()); }
        use std::io::Write;
        let mut proof = fs::OpenOptions::new().write(true).create_new(true).open(root.join("original-tree.sha256"))?;
        proof.write_all(original_hash.as_bytes())?;
        proof.sync_all()?;
    } else {
        let proof = root.join("original-tree.sha256"); confined(root, &proof)?;
        if fs::read_to_string(proof)? != original_hash { return Err("original fixture changed".into()); }
    }
    confined(root, &copied)?;
    let db = open(&copied).await?;
    let engine_version = db.version().await?.to_string();
    let before = rows(&db).await?;
    if before != wanted { return Err("canonical row mismatch: preserve original and candidate".into()); }
    if mode == "write-copy" {
        db.query("BEGIN TRANSACTION; CREATE rejection_audit:engine_candidate SET audit_id='KTL-REJ-engine-candidate', job_id='ENGINE-CANDIDATE', code='ISOLATED_FIXTURE', detail='3.3 write restart proof'; COMMIT TRANSACTION;").await?.check()?;
    } else if mode != "verify-copy" && mode != "verify-written" { return Err("unknown mode".into()); }
    if tree_hash(root, &original)? != original_hash { return Err("original changed during candidate evaluation".into()); }
    Ok(json!({"mode":mode,"engine_version":engine_version,"original_tree_sha256":original_hash,"canonical_rows_sha256":digest(&serde_json::to_vec(&before)?),"counts":{"proposals":before["proposals"].as_array().unwrap().len(),"captures":before["captures"].as_array().unwrap().len(),"rejections":before["rejections"].as_array().unwrap().len(),"receipts":before["receipts"].as_array().unwrap().len()}}))
}
fn main() {
    let outcome = (|| -> Result<Value> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 { return Err("usage: candidate MODE GENERATED_FIXTURE_ROOT".into()); }
        if !["fresh-schema","verify-copy","write-copy","verify-written"].contains(&args[1].as_str()) { return Err("unknown mode".into()); }
        let root = authorized_root(Path::new(&args[2]), &args[1])?;
        let sentinel = root.join("fixture-kind.txt"); confined(&root, &sentinel)?;
        if fs::read_to_string(sentinel)?.trim() != GUARD { return Err("generated fixture authorization missing".into()); }
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let mut result = runtime.block_on(run(&args[1], &root))?;
        result["scope"] = json!("isolated_sdk_evaluation_not_application_or_package_acceptance");
        result["sdk_version"] = json!("3.3.0"); result["sync"] = json!("every");
        result["ledger_source_sha256"] = json!(digest(LEDGER_SOURCE.as_bytes()));
        Ok(result)
    })();
    match outcome { Ok(value) => println!("{value}"), Err(error) => { eprintln!("{}", json!({"status":"failed","error":error.to_string(),"preserve_fixtures":true})); std::process::exit(1); } }
}
