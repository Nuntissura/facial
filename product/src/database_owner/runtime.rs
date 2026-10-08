//! Child-only engine runtime. This is entered before configuration or services.
use super::protocol::{self, Reply, Request, Startup};
use std::{
    ffi::OsStr,
    io::{self, Write},
    time::Instant,
};
use surrealdb::types::Value;

const RECEIPTS: &str = "facial_database_operation";
pub(super) const ASYNC_THREADS: usize = 2;
pub(super) const BLOCKING_THREADS: usize = 4;

pub(super) fn phase_trace_enabled() -> bool {
    trace_enabled(std::env::var_os("FACIAL_DBOWNER_PHASE_TRACE").as_deref())
}

fn trace_enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    StartupRead,
    StartupValidated,
    EngineOpenBegin,
    EngineOpenEnd,
    ReceiptSchemaBegin,
    ReceiptSchemaEnd,
    StartupEncode,
    StartupWrite,
    StartupReady,
    RequestRead,
    RequestReceived,
    RequestValidated,
    AcknowledgmentsBegin,
    AcknowledgmentsEnd,
    QueryBegin,
    QueryEnd,
    DecodeBegin,
    DecodeEnd,
    ReplyEncode,
    ReplyWrite,
    ReplySent,
    Failed,
}

struct PhaseTrace {
    enabled: bool,
    emitted: usize,
    origin: Instant,
}

impl PhaseTrace {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            emitted: 0,
            origin: Instant::now(),
        }
    }

    fn record(&mut self, phase: Phase, epoch: u64, operation: Option<&str>) -> Option<String> {
        if !self.enabled || self.emitted >= 128 {
            return None;
        }
        self.emitted += 1;
        let operation = operation
            .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        Some(serde_json::json!({
            "phase": phase, "elapsed_us": self.origin.elapsed().as_micros().min(u64::MAX as u128) as u64,
            "epoch": epoch, "operation_id": operation
        }).to_string())
    }

    fn emit(&mut self, phase: Phase, epoch: u64, operation: Option<&str>) {
        if let Some(record) = self.record(phase, epoch, operation) {
            // Opt-in diagnostics can affect timing; never write private-pipe stdout.
            let _ = writeln!(io::stderr().lock(), "{record}");
        }
    }
}

pub(crate) fn entry() -> i32 {
    let mut trace = PhaseTrace::new(phase_trace_enabled());
    match serve(&mut trace) {
        Ok(()) => 0,
        Err(_) => {
            trace.emit(Phase::Failed, 0, None);
            1
        }
    }
}

fn serve(trace: &mut PhaseTrace) -> Result<(), String> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    trace.emit(Phase::StartupRead, 0, None);
    let startup: Startup = protocol::read_frame(&mut input)?;
    if startup.version != protocol::VERSION || startup.owner_id.len() != 32 || startup.epoch == 0 {
        return Err("database owner startup version or identity invalid".into());
    }
    trace.emit(Phase::StartupValidated, startup.epoch, None);
    crate::surreal_store::ensure_deterministic_hnsw_seed()?;
    let engine_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(ASYNC_THREADS)
        .max_blocking_threads(BLOCKING_THREADS)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let db = engine_runtime.block_on(async {
        trace.emit(Phase::EngineOpenBegin, startup.epoch, None);
        let db = crate::surreal_store::create_engine(
            &crate::surreal_store::storage_path(&startup.database_root),
            &startup.database_root,
            &startup.database,
            startup.schema_version,
        )
        .await?;
        trace.emit(Phase::EngineOpenEnd, startup.epoch, None);
        trace.emit(Phase::ReceiptSchemaBegin, startup.epoch, None);
        db.query(format!("DEFINE TABLE IF NOT EXISTS {RECEIPTS} SCHEMAFULL; DEFINE FIELD IF NOT EXISTS operation_id ON {RECEIPTS} TYPE string; DEFINE FIELD IF NOT EXISTS digest ON {RECEIPTS} TYPE string; DEFINE FIELD IF NOT EXISTS epoch ON {RECEIPTS} TYPE int;"))
            .await.map_err(|e| e.to_string())?.check().map_err(|e| e.to_string())?;
        trace.emit(Phase::ReceiptSchemaEnd, startup.epoch, None);
        Ok(db)
    });
    let hello = Reply {
        owner_id: startup.owner_id.clone(),
        epoch: startup.epoch,
        operation_id: "startup".into(),
        digest: None,
        results: Vec::new(),
        error: db.as_ref().err().cloned(),
    };
    trace.emit(Phase::StartupEncode, startup.epoch, None);
    let hello_bytes = protocol::encode(&hello)?;
    trace.emit(Phase::StartupWrite, startup.epoch, None);
    protocol::write_frame(&mut output, &hello_bytes)?;
    let db = db?;
    trace.emit(Phase::StartupReady, startup.epoch, None);
    loop {
        trace.emit(Phase::RequestRead, startup.epoch, None);
        let request: Request = match protocol::read_frame(&mut input) {
            Ok(request) => request,
            Err(_) => return Ok(()),
        };
        trace.emit(
            Phase::RequestReceived,
            request.epoch,
            Some(&request.operation_id),
        );
        if request.owner_id != startup.owner_id || request.epoch != startup.epoch {
            return Err("database owner request identity mismatch".into());
        }
        let digest = protocol::digest_until(&(&request.sql, &request.bindings), None)?;
        if digest != request.digest || request.operation_id.len() != 32 {
            return Err("database owner operation identity invalid".into());
        }
        if request
            .reply_delay_after_commit_ms
            .is_some_and(|delay| delay > 10_000)
            || (request.reply_delay_after_commit_ms.is_some()
                && !(request.sql.trim().starts_with("BEGIN TRANSACTION;")
                    && request.sql.trim().ends_with("COMMIT TRANSACTION;")))
        {
            return Err("invalid database owner commit-delay probe".into());
        }
        trace.emit(
            Phase::RequestValidated,
            request.epoch,
            Some(&request.operation_id),
        );
        let outcome = engine_runtime.block_on(execute(&db, &request, trace));
        if outcome
            .as_ref()
            .is_ok_and(|results| results.iter().all(Result::is_ok))
        {
            if let Some(delay) = request.reply_delay_after_commit_ms {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
        }
        let reply = match outcome {
            Ok(results) => Reply {
                owner_id: startup.owner_id.clone(),
                epoch: startup.epoch,
                operation_id: request.operation_id,
                digest: Some(request.digest),
                results,
                error: None,
            },
            Err(error) => Reply {
                owner_id: startup.owner_id.clone(),
                epoch: startup.epoch,
                operation_id: request.operation_id,
                digest: Some(request.digest),
                results: Vec::new(),
                error: Some(error),
            },
        };
        trace.emit(Phase::ReplyEncode, reply.epoch, Some(&reply.operation_id));
        let reply_bytes = protocol::encode(&reply)?;
        trace.emit(Phase::ReplyWrite, reply.epoch, Some(&reply.operation_id));
        protocol::write_frame(&mut output, &reply_bytes)?;
        trace.emit(Phase::ReplySent, reply.epoch, Some(&reply.operation_id));
    }
}

async fn execute(
    db: &crate::surreal_store::NativeDb,
    request: &Request,
    trace: &mut PhaseTrace,
) -> Result<Vec<Result<Value, String>>, String> {
    if request.acknowledged.len() > protocol::MAX_ACKNOWLEDGED {
        return Err("database owner acknowledgment limit".into());
    }
    trace.emit(
        Phase::AcknowledgmentsBegin,
        request.epoch,
        Some(&request.operation_id),
    );
    for acknowledged in &request.acknowledged {
        if acknowledged.len() != 32 || !acknowledged.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("database owner acknowledgment invalid".into());
        }
        db.query(format!("DELETE type::record('{RECEIPTS}', $acknowledged);"))
            .bind(("acknowledged", acknowledged.clone()))
            .await
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
    }
    trace.emit(
        Phase::AcknowledgmentsEnd,
        request.epoch,
        Some(&request.operation_id),
    );
    // Application checkpoint producers already use this exact transaction
    // envelope. Receipt creation participates in their commit and cannot certify
    // a rollback. Other SQL retains the SDK's original statement semantics.
    let sql = request.sql.trim();
    let receipt = sql.starts_with("BEGIN TRANSACTION;") && sql.ends_with("COMMIT TRANSACTION;");
    let mut bindings = request.bindings.clone();
    let sql = if receipt {
        if bindings
            .keys()
            .any(|key| key.starts_with("__facial_owner_"))
        {
            return Err("reserved database owner binding".into());
        }
        bindings.insert(
            "__facial_owner_id".into(),
            Value::String(request.operation_id.clone()),
        );
        bindings.insert(
            "__facial_owner_digest".into(),
            Value::String(request.digest.clone()),
        );
        bindings.insert("__facial_owner_epoch".into(), request.epoch.into_value());
        format!("{} CREATE type::record('{RECEIPTS}', $__facial_owner_id) SET operation_id=$__facial_owner_id, digest=$__facial_owner_digest, epoch=$__facial_owner_epoch; COMMIT TRANSACTION;", sql.strip_suffix("COMMIT TRANSACTION;").unwrap())
    } else {
        sql.to_string()
    };
    trace.emit(
        Phase::QueryBegin,
        request.epoch,
        Some(&request.operation_id),
    );
    let mut response = db
        .query(sql)
        .bind(Value::Object(bindings.into()))
        .await
        .map_err(|e| e.to_string())?;
    trace.emit(Phase::QueryEnd, request.epoch, Some(&request.operation_id));
    trace.emit(
        Phase::DecodeBegin,
        request.epoch,
        Some(&request.operation_id),
    );
    let count = response.num_statements();
    let mut results = (0..count)
        .map(|index| response.take::<Value>(index).map_err(|e| e.to_string()))
        .collect::<Vec<_>>();
    if receipt {
        // BEGIN and COMMIT are both indexed responses in pinned SurrealDB.
        if results.len() < 3 {
            return Err("database receipt response shape invalid".into());
        }
        let receipt_result = results.remove(results.len() - 2);
        if let Err(error) = receipt_result {
            if results.iter().all(Result::is_ok) {
                return Err(format!("database operation receipt failed: {error}"));
            }
        }
    }
    trace.emit(Phase::DecodeEnd, request.epoch, Some(&request.operation_id));
    Ok(results)
}

use surrealdb::types::SurrealValue;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbowner_phase_trace_requires_exact_opt_in_and_is_bounded() {
        for value in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("true")),
            Some(OsStr::new("01")),
        ] {
            assert!(!trace_enabled(value));
        }
        assert!(trace_enabled(Some(OsStr::new("1"))));
        assert!(PhaseTrace::new(false)
            .record(Phase::QueryBegin, 1, None)
            .is_none());
        let mut trace = PhaseTrace::new(true);
        for _ in 0..128 {
            assert!(trace.record(Phase::QueryBegin, 1, None).is_some());
        }
        assert!(trace.record(Phase::QueryBegin, 1, None).is_none());
    }

    #[test]
    fn dbowner_phase_trace_emits_only_redacted_correlated_fields() {
        let mut trace = PhaseTrace::new(true);
        let private = "SELECT private_field FROM secret_path";
        let record = trace.record(Phase::QueryBegin, 7, Some(private)).unwrap();
        assert!(!record.contains(private));
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert!(value["operation_id"].is_null());
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert_eq!(value["phase"], "query_begin");
        assert_eq!(value["epoch"], 7);
        assert!(value["elapsed_us"].as_u64().is_some());
        let operation = "0123456789abcdef0123456789abcdef";
        let next: serde_json::Value =
            serde_json::from_str(&trace.record(Phase::ReplySent, 7, Some(operation)).unwrap())
                .unwrap();
        assert_eq!(next["operation_id"], operation);
        assert!(next["elapsed_us"].as_u64().unwrap() >= value["elapsed_us"].as_u64().unwrap());
    }
}
