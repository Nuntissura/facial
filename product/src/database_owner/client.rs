//! Query adapter, admission priority, deadlines, and owner generation fencing.
use super::{
    process::{self, ExitState, Process},
    protocol::{self, Request, Startup},
    response::Response,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::IntoFuture,
    marker::PhantomData,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, TryLockError,
    },
    time::{Duration, Instant},
};
use surrealdb::types::{SurrealValue, Value};

const MATCH_LIMIT: Duration = Duration::from_millis(2_000);
const FOREGROUND_LIMIT: Duration = Duration::from_secs(30);
const MAX_PENDING: usize = 32;
const MAX_PENDING_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone)]
struct Context {
    owner_id: String,
    epoch: u64,
    deadline: Option<Instant>,
    background: bool,
    match_scoped: bool,
}
thread_local! { static CONTEXT: RefCell<Vec<Context>> = const { RefCell::new(Vec::new()) }; }

pub(crate) struct MatchUnitScope {
    previous_len: usize,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for MatchUnitScope {
    fn drop(&mut self) {
        CONTEXT.with(|slot| slot.borrow_mut().truncate(self.previous_len));
    }
}

#[derive(Clone)]
pub struct Db {
    owner: Arc<Owner>,
    background: bool,
}
struct Owner {
    id: String,
    root: PathBuf,
    database: String,
    schema_version: u64,
    epoch: AtomicU64,
    foreground: AtomicUsize,
    pending: AtomicUsize,
    pending_bytes: AtomicUsize,
    pid: AtomicU64,
    phase: AtomicUsize,
    timeouts: AtomicU64,
    unknown_outcomes: AtomicU64,
    peak_process_bytes: AtomicUsize,
    peak_job_bytes: AtomicUsize,
    uncertain: Mutex<Vec<PendingOperation>>,
    state: Mutex<State>,
    #[cfg(test)]
    admission_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}
struct State {
    process: Option<Process>,
    retired: Vec<Arc<ExitState>>,
    acknowledged: Vec<String>,
}
#[derive(Clone)]
struct PendingOperation {
    operation_id: String,
    digest: String,
}

impl Db {
    pub(crate) fn open(root: &Path, database: &str, schema_version: u64) -> Result<Self, String> {
        let owner = Arc::new(Owner {
            id: uuid::Uuid::new_v4().simple().to_string(),
            root: root.to_path_buf(),
            database: database.into(),
            schema_version,
            epoch: AtomicU64::new(1),
            foreground: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            pending_bytes: AtomicUsize::new(0),
            uncertain: Mutex::new(Vec::new()),
            pid: AtomicU64::new(0),
            phase: AtomicUsize::new(0),
            timeouts: AtomicU64::new(0),
            unknown_outcomes: AtomicU64::new(0),
            peak_process_bytes: AtomicUsize::new(0),
            peak_job_bytes: AtomicUsize::new(0),
            state: Mutex::new(State {
                process: None,
                retired: Vec::new(),
                acknowledged: Vec::new(),
            }),
            #[cfg(test)]
            admission_pause: Mutex::new(None),
        });
        let deadline = Instant::now() + FOREGROUND_LIMIT;
        {
            let mut state = owner.lock(deadline, false)?;
            owner.ensure_process(&mut state, deadline)?;
        }
        Ok(Self {
            owner,
            background: false,
        })
    }
    pub(crate) fn background(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            background: true,
        }
    }
    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        let phase = match self.owner.phase.load(Ordering::Acquire) {
            1 => "starting",
            2 => "ready",
            3 => "exit_pending",
            _ => "stopped",
        };
        serde_json::json!({
            "owner_id": self.owner.id, "epoch": self.owner.epoch.load(Ordering::Acquire),
            "pid": self.owner.pid.load(Ordering::Acquire), "phase": phase,
            "pending_count": self.owner.pending.load(Ordering::Acquire), "pending_bytes": self.owner.pending_bytes.load(Ordering::Acquire),
            "foreground_waiters": self.owner.foreground.load(Ordering::Acquire),
            "timeouts": self.owner.timeouts.load(Ordering::Acquire), "unknown_outcomes": self.owner.unknown_outcomes.load(Ordering::Acquire),
            "unreconciled_operations": self.owner.uncertain.try_lock().ok().map(|pending| pending.len()),
            "async_threads": super::runtime::ASYNC_THREADS, "blocking_threads": super::runtime::BLOCKING_THREADS,
            "peak_process_bytes": self.owner.peak_process_bytes.load(Ordering::Acquire),
            "peak_job_bytes": self.owner.peak_job_bytes.load(Ordering::Acquire),
        })
    }
    pub(crate) fn begin_match_unit(&self, deadline: Instant) -> Result<MatchUnitScope, String> {
        if !self
            .owner
            .uncertain
            .lock()
            .map_err(|_| "database outcome registry poisoned")?
            .is_empty()
        {
            return Err("commit_outcome_unknown: explicit canonical reconciliation required before Match admission".into());
        }
        #[cfg(test)]
        {
            let pause = self.owner.admission_pause.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                entered.wait();
                release.wait();
            }
        }
        self.begin_unit(Some(deadline), true, true)
    }
    #[cfg(test)]
    pub(crate) fn test_pause_after_empty_uncertain(
        &self,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    ) {
        *self.owner.admission_pause.lock().unwrap() = Some((entered, release));
    }
    pub(crate) fn reconcile_pending_operations(&self) -> Result<(), String> {
        let pending = self
            .owner
            .uncertain
            .lock()
            .map_err(|_| "database outcome registry poisoned")?
            .clone();
        if pending.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + FOREGROUND_LIMIT;
        let _admission = Admission::new(&self.owner, true, 0)?;
        let mut state = self.owner.lock(deadline, false)?;
        self.owner.ensure_process(&mut state, deadline)?;
        for operation in pending {
            let mut request = self.owner.receipt_read_request(&operation)?;
            request.acknowledged = state.acknowledged.clone();
            let outcome = state
                .process
                .as_ref()
                .ok_or("database owner unavailable")?
                .exchange(protocol::encode(&request)?, deadline);
            let reply = match outcome {
                Ok(reply) => reply,
                Err(error) => {
                    self.owner.retire(&mut state);
                    return Err(error);
                }
            };
            if reply.owner_id != self.owner.id
                || reply.epoch != request.epoch
                || reply.operation_id != request.operation_id
                || reply.digest.as_deref() != Some(request.digest.as_str())
            {
                self.owner.retire(&mut state);
                return Err("database receipt reconciliation reply mismatch".into());
            }
            if let Some(error) = reply.error {
                return Err(error);
            }
            state
                .acknowledged
                .retain(|id| !request.acknowledged.contains(id));
            let mut response = Response {
                results: reply.results.into_iter().map(Some).collect(),
            };
            let row: Option<serde_json::Value> = response.take(0)?;
            if row.as_ref().is_some_and(|row| {
                row.get("digest").and_then(serde_json::Value::as_str)
                    != Some(operation.digest.as_str())
            }) {
                return Err("database operation receipt digest mismatch".into());
            }
            // Presence proves commit; absence after the dead owner's lock was
            // released proves rollback. Callers reload their canonical cursor;
            // no SQL is replayed and no cache success is synthesized here.
            if row.is_some() {
                state.acknowledge(operation.operation_id.clone())?;
            }
            self.owner
                .uncertain
                .lock()
                .map_err(|_| "database outcome registry poisoned")?
                .retain(|item| item.operation_id != operation.operation_id);
        }
        Ok(())
    }
    pub(crate) fn begin_media_unit(&self) -> Result<MatchUnitScope, String> {
        self.begin_unit(Some(Instant::now() + FOREGROUND_LIMIT), false, false)
    }
    pub(crate) fn begin_match_transaction(&self) -> Result<MatchUnitScope, String> {
        if !self
            .owner
            .uncertain
            .lock()
            .map_err(|_| "database outcome registry poisoned")?
            .is_empty()
        {
            return Err("commit_outcome_unknown: explicit canonical reconciliation required before Match mutation".into());
        }
        self.begin_unit(None, false, true)
    }
    fn begin_unit(
        &self,
        deadline: Option<Instant>,
        background: bool,
        match_scoped: bool,
    ) -> Result<MatchUnitScope, String> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err("safe_unit_timeout: database unit deadline expired".into());
        }
        let mut context = Context {
            owner_id: self.owner.id.clone(),
            epoch: self.owner.epoch.load(Ordering::Acquire),
            deadline,
            background,
            match_scoped,
        };
        let previous_len = CONTEXT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(existing) = slot
                .iter()
                .rev()
                .find(|existing| existing.owner_id == context.owner_id)
            {
                context.deadline = match (context.deadline, existing.deadline) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                context.epoch = existing.epoch;
                context.background |= existing.background;
                context.match_scoped |= existing.match_scoped;
            }
            let previous_len = slot.len();
            slot.push(context);
            previous_len
        });
        Ok(MatchUnitScope {
            previous_len,
            _thread: PhantomData,
        })
    }
    pub(crate) fn match_unit_deadline(&self) -> Option<Instant> {
        self.context().and_then(|context| context.deadline)
    }
    fn context(&self) -> Option<Context> {
        CONTEXT.with(|slot| {
            slot.borrow()
                .iter()
                .rev()
                .find(|context| context.owner_id == self.owner.id)
                .cloned()
        })
    }
    pub(crate) fn retain_until_owner_exit<T: Send + 'static>(&self, value: T) {
        let exits = match self.owner.state.lock() {
            Ok(state) => state.retired.clone(),
            Err(_) => {
                std::mem::forget(value);
                return;
            }
        };
        process::retain_until_exit(exits, value);
    }
    pub fn query(&self, sql: impl AsRef<str>) -> Query {
        let context = self.context();
        let background =
            self.background || context.as_ref().is_some_and(|context| context.background);
        Query {
            db: self.clone(),
            sql: sql.as_ref().to_string(),
            bindings: Ok(BTreeMap::new()),
            background,
            match_scoped: self.background
                || context.as_ref().is_some_and(|context| context.match_scoped),
            epoch: context.as_ref().map_or_else(
                || self.owner.epoch.load(Ordering::Acquire),
                |context| context.epoch,
            ),
            deadline: context
                .and_then(|context| context.deadline)
                .unwrap_or_else(|| {
                    Instant::now()
                        + if background {
                            MATCH_LIMIT
                        } else {
                            FOREGROUND_LIMIT
                        }
                }),
            reply_delay_after_commit_ms: None,
            #[cfg(test)]
            encode_delay: Duration::ZERO,
        }
    }
    pub fn use_ns(&self, namespace: impl AsRef<str>) -> Selection {
        Selection {
            db: self.clone(),
            namespace: namespace.as_ref().to_string(),
        }
    }
}

pub struct Selection {
    db: Db,
    namespace: String,
}
impl Selection {
    pub async fn use_db(self, database: impl AsRef<str>) -> Result<(), String> {
        if self.namespace != "facial" || self.db.owner.database != database.as_ref() {
            Err("database owner namespace/database are immutable".into())
        } else {
            Ok(())
        }
    }
}

pub struct Query {
    db: Db,
    sql: String,
    bindings: Result<BTreeMap<String, Value>, String>,
    background: bool,
    match_scoped: bool,
    epoch: u64,
    deadline: Instant,
    reply_delay_after_commit_ms: Option<u64>,
    #[cfg(test)]
    encode_delay: Duration,
}
impl Query {
    #[cfg(test)]
    pub(crate) fn test_delay_encode_chunk(mut self, duration: Duration) -> Self {
        self.encode_delay = duration;
        self
    }
    #[cfg(test)]
    pub(crate) fn test_delay_reply_after_commit(mut self, duration: Duration) -> Self {
        self.reply_delay_after_commit_ms =
            Some(duration.as_millis().min(u128::from(u64::MAX)) as u64);
        self
    }
    pub fn bind(mut self, value: impl SurrealValue) -> Self {
        if Instant::now() >= self.deadline {
            self.bindings = Err("safe_unit_timeout: database deadline before binding".into());
            return self;
        }
        let value = value.into_value();
        if let Ok(bindings) = &mut self.bindings {
            match value {
                Value::Object(object) => bindings.extend(object.into_inner()),
                Value::Array(values) => {
                    let values = values.into_vec();
                    if values.len() % 2 != 0 {
                        self.bindings = Err("database binding tuple length is odd".into());
                        return self;
                    }
                    let mut values = values.into_iter();
                    while let Some(key) = values.next() {
                        match key {
                            Value::String(key) => {
                                bindings.insert(key, values.next().expect("even binding tuple"));
                            }
                            _ => {
                                self.bindings = Err("database binding key must be a string".into());
                                break;
                            }
                        }
                    }
                }
                _ => {
                    self.bindings =
                        Err("database bindings must be an object or key/value tuple".into())
                }
            }
        }
        if Instant::now() >= self.deadline {
            self.bindings = Err("safe_unit_timeout: database deadline during binding".into());
        }
        self
    }
}
impl IntoFuture for Query {
    type Output = Result<Response, String>;
    type IntoFuture = Pin<Box<dyn std::future::Future<Output = Self::Output> + Send>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move { self.execute() })
    }
}
impl Query {
    fn execute(self) -> Result<Response, String> {
        #[cfg(test)]
        let _encode_delay = protocol::delay_encode(self.encode_delay);
        if Instant::now() >= self.deadline {
            return Err("safe_unit_timeout: database deadline before encoding".into());
        }
        let owner = &self.db.owner;
        // Reserve before serializing: queued requests cannot allocate their
        // maximum frames first and only then discover a saturated byte budget.
        let _admission = Admission::new(owner, !self.background, protocol::MAX_FRAME)?;
        let execution_deadline = self
            .deadline
            .checked_sub(Duration::from_millis(100))
            .unwrap_or(self.deadline);
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let bindings = self.bindings?;
        let digest = protocol::digest_until(&(&self.sql, &bindings), Some(execution_deadline))?;
        let mut request = Request {
            owner_id: self.db.owner.id.clone(),
            epoch: self.epoch,
            operation_id: operation_id.clone(),
            sql: self.sql,
            bindings,
            digest,
            acknowledged: Vec::new(),
            reply_delay_after_commit_ms: self.reply_delay_after_commit_ms,
        };
        {
            let state = owner.lock(execution_deadline, self.background)?;
            if self.epoch != owner.epoch.load(Ordering::Acquire) {
                return Err("database_owner_epoch_changed: stale operation rejected".into());
            }
            request.acknowledged = state.acknowledged.clone();
        }
        let bytes = protocol::encode_until(&request, Some(execution_deadline))?;
        let mut state = owner.lock(execution_deadline, self.background)?;
        if self.epoch != owner.epoch.load(Ordering::Acquire) {
            return Err("database_owner_epoch_changed: stale operation rejected".into());
        }
        // Retirement and outcome publication hold state before uncertain.
        // Recheck under that same dispatch lock: admission may have observed
        // an empty registry immediately before the retired epoch was published.
        if self.match_scoped
            && !owner
                .uncertain
                .lock()
                .map_err(|_| "database outcome registry poisoned")?
                .is_empty()
        {
            return Err("commit_outcome_unknown: explicit canonical reconciliation required before Match dispatch".into());
        }
        // State remains locked through outcome publication, reserving the last
        // registry slot for this transaction if its commit reply is lost.
        if request.sql.trim().starts_with("BEGIN TRANSACTION;")
            && request.sql.trim().ends_with("COMMIT TRANSACTION;")
            && owner
                .uncertain
                .lock()
                .map_err(|_| "database outcome registry poisoned")?
                .len()
                >= MAX_PENDING
        {
            return Err("database_owner_uncertainty_limit: transaction not dispatched; explicit canonical reconciliation required".into());
        }
        if let Err(error) = owner.ensure_process(&mut state, execution_deadline) {
            owner.confirm_retired_until(&state, self.deadline);
            return Err(error);
        }
        if Instant::now() >= execution_deadline {
            return Err("safe_unit_timeout: database deadline before dispatch".into());
        }
        let outcome = state
            .process
            .as_ref()
            .ok_or("database owner unavailable")?
            .exchange(bytes, execution_deadline);
        let reply = match outcome {
            Ok(reply) => reply,
            Err(error) => {
                if error.contains("safe_unit_timeout") {
                    owner.timeouts.fetch_add(1, Ordering::AcqRel);
                }
                owner.unknown_outcomes.fetch_add(1, Ordering::AcqRel);
                owner.retire(&mut state);
                owner.note_uncertain(&request)?;
                owner.confirm_retired_until(&state, self.deadline);
                return Err(format!(
                    "{error}; commit_outcome_unknown; operation_id={operation_id}"
                ));
            }
        };
        owner.observe_peaks(&state);
        if reply.owner_id != owner.id
            || reply.epoch != self.epoch
            || reply.operation_id != operation_id
            || reply.digest.as_deref() != Some(request.digest.as_str())
            || Instant::now() >= self.deadline
        {
            owner.timeouts.fetch_add(1, Ordering::AcqRel);
            owner.unknown_outcomes.fetch_add(1, Ordering::AcqRel);
            owner.retire(&mut state);
            owner.note_uncertain(&request)?;
            owner.confirm_retired_until(&state, self.deadline);
            return Err(format!("safe_unit_timeout: database owner late or mismatched reply; commit_outcome_unknown; operation_id={operation_id}"));
        }
        if let Some(error) = reply.error {
            return Err(error);
        }
        state
            .acknowledged
            .retain(|id| !request.acknowledged.contains(id));
        if request.sql.trim().starts_with("BEGIN TRANSACTION;")
            && request.sql.trim().ends_with("COMMIT TRANSACTION;")
            && reply.results.iter().all(Result::is_ok)
        {
            if let Err(error) = state.acknowledge(operation_id) {
                owner.note_uncertain(&request)?;
                return Err(error);
            }
        }
        Ok(Response {
            results: reply.results.into_iter().map(Some).collect(),
        })
    }
}

struct Admission<'a> {
    owner: &'a Owner,
    foreground: bool,
    bytes: usize,
}
impl State {
    fn acknowledge(&mut self, operation_id: String) -> Result<(), String> {
        if self.acknowledged.contains(&operation_id) {
            return Ok(());
        }
        if self.acknowledged.len() >= protocol::MAX_ACKNOWLEDGED {
            return Err("commit_outcome_unknown: database acknowledgment limit; explicit canonical reconciliation required".into());
        }
        self.acknowledged.push(operation_id);
        Ok(())
    }
}
impl<'a> Admission<'a> {
    fn new(owner: &'a Owner, foreground: bool, bytes: usize) -> Result<Self, String> {
        if owner.pending.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING {
            owner.pending.fetch_sub(1, Ordering::AcqRel);
            return Err("database_owner_queue_limit".into());
        }
        if owner.pending_bytes.fetch_add(bytes, Ordering::AcqRel)
            > MAX_PENDING_BYTES.saturating_sub(bytes)
        {
            owner.pending_bytes.fetch_sub(bytes, Ordering::AcqRel);
            owner.pending.fetch_sub(1, Ordering::AcqRel);
            return Err("database_owner_queue_bytes_limit".into());
        }
        if foreground {
            owner.foreground.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Self {
            owner,
            foreground,
            bytes,
        })
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if self.foreground {
            self.owner.foreground.fetch_sub(1, Ordering::AcqRel);
        }
        self.owner.pending.fetch_sub(1, Ordering::AcqRel);
        self.owner
            .pending_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
impl Owner {
    fn confirm_retired_until(&self, state: &State, deadline: Instant) {
        while state.retired.iter().any(|exit| !exit.confirmed_dead()) && Instant::now() < deadline {
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        if state.retired.iter().all(|exit| exit.confirmed_dead()) {
            self.phase.store(0, Ordering::Release);
            self.pid.store(0, Ordering::Release);
        }
    }
    fn observe_peaks(&self, state: &State) {
        if let Some((process, job)) = state
            .process
            .as_ref()
            .and_then(|process| process.exit.peak_memory())
        {
            self.peak_process_bytes.fetch_max(process, Ordering::AcqRel);
            self.peak_job_bytes.fetch_max(job, Ordering::AcqRel);
        }
    }
    fn note_uncertain(&self, request: &Request) -> Result<(), String> {
        if request.sql.trim().starts_with("BEGIN TRANSACTION;")
            && request.sql.trim().ends_with("COMMIT TRANSACTION;")
        {
            let mut uncertain = self
                .uncertain
                .lock()
                .map_err(|_| "database outcome registry poisoned")?;
            if uncertain
                .iter()
                .any(|operation| operation.operation_id == request.operation_id)
            {
                return Ok(());
            }
            // Dispatch checked capacity under state, which the caller still
            // holds. Hitting this guard means that ownership invariant broke.
            if uncertain.len() >= MAX_PENDING {
                return Err(format!("commit_outcome_unknown: database outcome registry invariant violated; operation_id={}; digest={}", request.operation_id, request.digest));
            }
            uncertain.push(PendingOperation {
                operation_id: request.operation_id.clone(),
                digest: request.digest.clone(),
            });
        }
        Ok(())
    }
    fn receipt_read_request(&self, operation: &PendingOperation) -> Result<Request, String> {
        let sql =
            "SELECT digest FROM ONLY type::record('facial_database_operation', $operation_id);"
                .to_string();
        let bindings = BTreeMap::from([(
            "operation_id".into(),
            Value::String(operation.operation_id.clone()),
        )]);
        let digest = protocol::digest_until(&(&sql, &bindings), None)?;
        Ok(Request {
            owner_id: self.id.clone(),
            epoch: self.epoch.load(Ordering::Acquire),
            operation_id: uuid::Uuid::new_v4().simple().to_string(),
            sql,
            bindings,
            digest,
            acknowledged: Vec::new(),
            reply_delay_after_commit_ms: None,
        })
    }
    fn lock(&self, deadline: Instant, background: bool) -> Result<MutexGuard<'_, State>, String> {
        loop {
            if Instant::now() >= deadline {
                return Err("safe_unit_timeout: database owner queue deadline".into());
            }
            if !background || self.foreground.load(Ordering::Acquire) == 0 {
                match self.state.try_lock() {
                    Ok(guard) => return Ok(guard),
                    Err(TryLockError::Poisoned(_)) => {
                        return Err("database owner state poisoned".into())
                    }
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    fn ensure_process(&self, state: &mut State, deadline: Instant) -> Result<(), String> {
        if state.process.is_some() {
            return Ok(());
        }
        state.retired.retain(|exit| !exit.confirmed_dead());
        if !state.retired.is_empty() {
            return Err("database_owner_exit_pending: replacement blocked".into());
        }
        self.phase.store(1, Ordering::Release);
        let process = match Process::spawn() {
            Ok(process) => process,
            Err(error) => {
                self.phase.store(0, Ordering::Release);
                return Err(error);
            }
        };
        self.pid
            .store(u64::from(process.exit.pid()), Ordering::Release);
        let startup = Startup {
            version: protocol::VERSION,
            database_root: self.root.clone(),
            database: self.database.clone(),
            schema_version: self.schema_version,
            owner_id: self.id.clone(),
            epoch: self.epoch.load(Ordering::Acquire),
        };
        let reply = process.exchange(protocol::encode(&startup)?, deadline);
        state.process = Some(process);
        match reply {
            Ok(reply)
                if reply.owner_id == self.id
                    && reply.epoch == startup.epoch
                    && reply.operation_id == "startup" =>
            {
                if let Some(error) = reply.error {
                    self.retire(state);
                    return Err(error);
                }
                self.phase.store(2, Ordering::Release);
                Ok(())
            }
            Ok(_) => {
                self.retire(state);
                Err("database owner startup protocol mismatch".into())
            }
            Err(error) => {
                self.retire(state);
                Err(error)
            }
        }
    }
    fn retire(&self, state: &mut State) {
        self.observe_peaks(state);
        if let Some(process) = state.process.take() {
            process.exit.terminate();
            self.phase.store(3, Ordering::Release);
            state.retired.push(process.exit.clone());
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
}
