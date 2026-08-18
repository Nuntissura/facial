//! Shared embedded SurrealDB runtime and per-path handle registry.
//!
//! Facial's synchronous UI/API surfaces use one process-owned Tokio runtime.
//! Concurrent opens of the same database root share the same embedded handle,
//! while weak registry entries let tests and relocated workspaces release file
//! locks normally after their last owner is dropped.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use surrealdb::engine::local::{Db, SurrealKv};
use surrealdb::Surreal;

const ENGINE_MARKER_NAME: &str = "engine.json";
pub const ENGINE_VERSION: &str = env!("FACIAL_SURREALDB_VERSION");
const NAMESPACE: &str = "facial";
const DATABASE: &str = "application";

pub type EmbeddedDb = Surreal<Db>;

pub struct Store {
    db: Option<EmbeddedDb>,
    database_root: PathBuf,
    storage_path: String,
    transaction_lock: RwLock<()>,
    session_id: String,
    database: String,
    marker_schema_version: u64,
}

impl Deref for Store {
    type Target = EmbeddedDb;

    fn deref(&self) -> &Self::Target {
        self.db.as_ref().expect("embedded SurrealDB store is open")
    }
}

impl Store {
    pub fn db(&self) -> EmbeddedDb {
        self.db
            .as_ref()
            .expect("embedded SurrealDB store is open")
            .clone()
    }

    pub fn transaction_lock(&self) -> &RwLock<()> {
        &self.transaction_lock
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Dropping the final SDK handle closes its router channel. The embedded
        // router then runs Datastore::shutdown, including SurrealKV's final
        // flush and lock release. Keep the registry tombstone in place until
        // that release completes so an immediate reopen cannot race a second
        // engine against the first engine's shutdown.
        drop(self.db.take());
        let database_root = self.database_root.clone();
        let storage_path = self.storage_path.clone();
        let close = move || finish_close(database_root, storage_path);
        if let Err(error) = std::thread::Builder::new()
            .name("surrealdb-close".to_string())
            .spawn(close)
        {
            eprintln!("embedded SurrealDB close worker unavailable: {error}");
            finish_close(self.database_root.clone(), self.storage_path.clone());
        }
    }
}

struct StoreRegistry {
    entries: Mutex<BTreeMap<String, Weak<Store>>>,
    close_failures: Mutex<BTreeMap<String, String>>,
    closed: Condvar,
    /// Paths with an open in progress. The async open path must release the
    /// entries guard before awaiting the engine, so this reservation keeps the
    /// "one engine per path" guarantee the guard used to provide on its own.
    creating: Mutex<std::collections::BTreeSet<String>>,
}

/// Outcome of one non-awaiting registry inspection.
enum RegistryStep {
    /// An open handle already exists for this path.
    Ready(Arc<Store>),
    /// This caller reserved the path and must create the engine.
    Create,
    /// Another caller is creating, or a previous handle is still closing.
    Retry,
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static REGISTRY: OnceLock<StoreRegistry> = OnceLock::new();

pub fn run<T>(future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("create Facial embedded-SurrealDB runtime")
        })
        .block_on(future)
}

fn registry() -> &'static StoreRegistry {
    REGISTRY.get_or_init(|| StoreRegistry {
        entries: Mutex::new(BTreeMap::new()),
        close_failures: Mutex::new(BTreeMap::new()),
        closed: Condvar::new(),
        creating: Mutex::new(std::collections::BTreeSet::new()),
    })
}

pub fn open(database_root: &Path) -> Result<Arc<Store>, String> {
    open_database(database_root, DATABASE, 1)
}

/// Synchronous open for UI/API surfaces that are not already inside the
/// runtime. It shares one implementation with the async entry point so the two
/// can never diverge in registry semantics.
pub fn open_database(
    database_root: &Path,
    database: &str,
    marker_schema_version: u64,
) -> Result<Arc<Store>, String> {
    run(open_database_async(
        database_root,
        database,
        marker_schema_version,
    ))
}

/// Async open for callers already running inside the shared runtime.
///
/// The synchronous path used to hold the registry guard across the engine
/// creation, which forced that creation to be a nested `block_on`. Calling it
/// from an `async fn` therefore panicked with "Cannot start a runtime from
/// within a runtime" — and because the panic happened while the guard was
/// held, it poisoned the registry for the rest of the process, so every later
/// open failed too. This entry point awaits the engine with no guard held and
/// reserves the path instead, which preserves the one-engine-per-path
/// guarantee the guard used to provide.
pub async fn open_database_async(
    database_root: &Path,
    database: &str,
    marker_schema_version: u64,
) -> Result<Arc<Store>, String> {
    std::fs::create_dir_all(database_root)
        .map_err(|error| format!("create {}: {error}", database_root.display()))?;
    let database_root = std::fs::canonicalize(database_root)
        .map_err(|error| format!("canonicalize {}: {error}", database_root.display()))?;
    let storage_path = storage_path(&database_root);
    let registry = registry();

    // Phase 1: reserve the path. Every branch releases the guard before this
    // function awaits anything.
    loop {
        let step = inspect_registry(
            registry,
            &storage_path,
            &database_root,
            database,
            marker_schema_version,
        )?;
        match step {
            RegistryStep::Ready(store) => return Ok(store),
            RegistryStep::Create => break,
            RegistryStep::Retry => {
                // A prior handle is still closing, or another caller is
                // creating this path. Yield instead of blocking a runtime
                // worker on the Condvar.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    // Phase 2: create the engine with no registry guard held. The reservation
    // is released on every exit path, including the error paths below.
    let outcome = create_engine(&storage_path, &database_root, database, marker_schema_version)
        .await
        .map(|db| {
            Arc::new(Store {
                db: Some(db),
                database_root: database_root.clone(),
                storage_path: storage_path.clone(),
                transaction_lock: RwLock::new(()),
                session_id: uuid::Uuid::new_v4().simple().to_string(),
                database: database.to_string(),
                marker_schema_version,
            })
        });

    // Phase 3: publish (or release) the reservation.
    let mut creating = registry
        .creating
        .lock()
        .map_err(|_| "embedded SurrealDB creation registry is poisoned".to_string())?;
    creating.remove(&storage_path);
    drop(creating);

    let store = outcome?;
    let mut entries = registry
        .entries
        .lock()
        .map_err(|_| "embedded SurrealDB registry is poisoned".to_string())?;
    // Another caller may have won the race while this one awaited; prefer the
    // published handle so the process never holds two engines for one path.
    if let Some(existing) = entries.get(&storage_path).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    entries.insert(storage_path, Arc::downgrade(&store));
    Ok(store)
}

/// One non-awaiting registry inspection. The guard is dropped when this
/// returns, so the caller may await safely.
fn inspect_registry(
    registry: &StoreRegistry,
    storage_path: &str,
    database_root: &Path,
    database: &str,
    marker_schema_version: u64,
) -> Result<RegistryStep, String> {
    let entries = registry
        .entries
        .lock()
        .map_err(|_| "embedded SurrealDB registry is poisoned".to_string())?;
    if let Some(weak) = entries.get(storage_path) {
        if let Some(store) = weak.upgrade() {
            if store.database != database || store.marker_schema_version != marker_schema_version {
                return Err(format!(
                    "embedded SurrealDB root {} is already open on database {} schema {}; requested database {} schema {}",
                    database_root.display(),
                    store.database,
                    store.marker_schema_version,
                    database,
                    marker_schema_version
                ));
            }
            return Ok(RegistryStep::Ready(store));
        }
        // A tombstone means the previous handle is still shutting down. Surface
        // a recorded close failure rather than waiting on it forever.
        if let Some(error) = registry
            .close_failures
            .lock()
            .map_err(|_| "embedded SurrealDB close-state registry is poisoned".to_string())?
            .get(storage_path)
            .cloned()
        {
            return Err(error);
        }
        return Ok(RegistryStep::Retry);
    }
    drop(entries);

    let mut creating = registry
        .creating
        .lock()
        .map_err(|_| "embedded SurrealDB creation registry is poisoned".to_string())?;
    if creating.contains(storage_path) {
        return Ok(RegistryStep::Retry);
    }
    creating.insert(storage_path.to_string());
    Ok(RegistryStep::Create)
}

async fn create_engine(
    storage_path: &str,
    database_root: &Path,
    database: &str,
    marker_schema_version: u64,
) -> Result<EmbeddedDb, String> {
    ensure_engine_marker(database_root, database, marker_schema_version)?;
    let db = Surreal::new::<SurrealKv>(storage_path.to_string())
        .sync("every")
        .await
        .map_err(|error| format!("open embedded SurrealDB: {error}"))?;
    db.use_ns(NAMESPACE)
        .use_db(database.to_string())
        .await
        .map_err(|error| format!("select embedded SurrealDB namespace: {error}"))?;
    Ok(db)
}

/// Wait until a dropped store at this path has completed its embedded-engine
/// shutdown. Normal app exit does not block on this; callers that must move or
/// reopen a workspace inside the same process use this explicit barrier.
pub fn wait_until_closed(database_root: &Path) -> Result<(), String> {
    let database_root = std::fs::canonicalize(database_root)
        .map_err(|error| format!("canonicalize {}: {error}", database_root.display()))?;
    let storage_path = storage_path(&database_root);
    let Some(registry) = REGISTRY.get() else {
        return Ok(());
    };
    let mut entries = registry
        .entries
        .lock()
        .map_err(|_| "embedded SurrealDB registry is poisoned".to_string())?;
    while entries.contains_key(&storage_path) {
        if let Some(error) = registry
            .close_failures
            .lock()
            .map_err(|_| "embedded SurrealDB close-state registry is poisoned".to_string())?
            .get(&storage_path)
            .cloned()
        {
            return Err(error);
        }
        entries = registry
            .closed
            .wait(entries)
            .map_err(|_| "embedded SurrealDB registry is poisoned".to_string())?;
    }
    Ok(())
}

fn finish_close(database_root: PathBuf, storage_path: String) {
    loop {
        match wait_for_database_release(&database_root) {
            Ok(()) => break,
            Err(error) => {
                if let Some(registry) = REGISTRY.get() {
                    if let Ok(mut failures) = registry.close_failures.lock() {
                        failures.insert(storage_path.clone(), error);
                        registry.closed.notify_all();
                    }
                }
            }
        }
    }
    if let Some(registry) = REGISTRY.get() {
        if let Ok(mut failures) = registry.close_failures.lock() {
            failures.remove(&storage_path);
        }
        if let Ok(mut entries) = registry.entries.lock() {
            entries.remove(&storage_path);
            registry.closed.notify_all();
        }
    }
}

#[cfg(windows)]
fn wait_for_database_release(database_root: &Path) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let lock_path = database_root.join("LOCK");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let released = !lock_path.exists()
            || std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(&lock_path)
                .is_ok();
        if released {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "embedded SurrealDB lock at {} was not released within 10 seconds",
                lock_path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(windows))]
fn wait_for_database_release(_database_root: &Path) -> Result<(), String> {
    // POSIX permits renaming open files and the SDK provides no public close
    // future. Yield briefly so the router task can perform its shutdown before
    // a same-process reopen.
    std::thread::sleep(std::time::Duration::from_millis(10));
    Ok(())
}

fn ensure_engine_marker(
    database_root: &Path,
    database: &str,
    marker_schema_version: u64,
) -> Result<(), String> {
    let state_root = database_root
        .parent()
        .ok_or_else(|| format!("{} has no state-root parent", database_root.display()))?;
    let marker = state_root.join(ENGINE_MARKER_NAME);
    if marker.is_file() {
        let value: Value = serde_json::from_slice(
            &std::fs::read(&marker)
                .map_err(|error| format!("read {}: {error}", marker.display()))?,
        )
        .map_err(|error| format!("parse {}: {error}", marker.display()))?;
        let version = value.get("engine_version").and_then(Value::as_str);
        if value.get("engine").and_then(Value::as_str) == Some("surrealdb")
            && version == Some(ENGINE_VERSION)
            && value.get("schema_version").and_then(Value::as_u64) == Some(marker_schema_version)
            && value
                .get("namespace")
                .and_then(Value::as_str)
                .is_none_or(|value| value == NAMESPACE)
            && value
                .get("database")
                .and_then(Value::as_str)
                .is_none_or(|value| value == database)
        {
            return Ok(());
        }
        return Err(format!(
            "embedded database marker {} is incompatible with SurrealDB {ENGINE_VERSION}",
            marker.display()
        ));
    }
    let has_database_files = std::fs::read_dir(database_root)
        .map_err(|error| format!("read {}: {error}", database_root.display()))?
        .next()
        .is_some();
    if has_database_files {
        return Err(format!(
            "unmarked embedded database at {}; refusing to guess its engine",
            database_root.display()
        ));
    }
    write_json_atomic(
        &marker,
        &json!({
            "engine": "surrealdb",
            "engine_version": ENGINE_VERSION,
            "namespace": NAMESPACE,
            "database": database,
            "schema_version": marker_schema_version,
        }),
    )
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("engine"),
        uuid::Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize {}: {error}", path.display()))?;
    std::fs::write(&temp, bytes).map_err(|error| format!("write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| format!("publish {}: {error}", path.display()))
}

fn storage_path(path: &Path) -> String {
    let display = path.to_string_lossy();
    if let Some(rest) = display.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = display.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        display.into_owned()
    }
}
