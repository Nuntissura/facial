//! Child-only engine runtime. This is entered before configuration or services.
use super::protocol::{self, Reply, Request, Startup};
use std::io;
use surrealdb::types::Value;

const RECEIPTS: &str = "facial_database_operation";
pub(super) const ASYNC_THREADS: usize = 2;
pub(super) const BLOCKING_THREADS: usize = 4;

pub(crate) fn entry() -> i32 {
    match serve() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn serve() -> Result<(), String> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let startup: Startup = protocol::read_frame(&mut input)?;
    if startup.version != protocol::VERSION || startup.owner_id.len() != 32 || startup.epoch == 0 {
        return Err("database owner startup version or identity invalid".into());
    }
    crate::surreal_store::ensure_deterministic_hnsw_seed()?;
    let engine_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(ASYNC_THREADS)
        .max_blocking_threads(BLOCKING_THREADS)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let db = engine_runtime.block_on(async {
        let db = crate::surreal_store::create_engine(
            &crate::surreal_store::storage_path(&startup.database_root),
            &startup.database_root,
            &startup.database,
            startup.schema_version,
        )
        .await?;
        db.query(format!("DEFINE TABLE IF NOT EXISTS {RECEIPTS} SCHEMAFULL; DEFINE FIELD IF NOT EXISTS operation_id ON {RECEIPTS} TYPE string; DEFINE FIELD IF NOT EXISTS digest ON {RECEIPTS} TYPE string; DEFINE FIELD IF NOT EXISTS epoch ON {RECEIPTS} TYPE int;"))
            .await.map_err(|e| e.to_string())?.check().map_err(|e| e.to_string())?;
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
    protocol::write_frame(&mut output, &protocol::encode(&hello)?)?;
    let db = db?;
    loop {
        let request: Request = match protocol::read_frame(&mut input) {
            Ok(request) => request,
            Err(_) => return Ok(()),
        };
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
        let outcome = engine_runtime.block_on(execute(&db, &request));
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
        protocol::write_frame(&mut output, &protocol::encode(&reply)?)?;
    }
}

async fn execute(
    db: &crate::surreal_store::NativeDb,
    request: &Request,
) -> Result<Vec<Result<Value, String>>, String> {
    if request.acknowledged.len() > protocol::MAX_ACKNOWLEDGED {
        return Err("database owner acknowledgment limit".into());
    }
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
    let mut response = db
        .query(sql)
        .bind(Value::Object(bindings.into()))
        .await
        .map_err(|e| e.to_string())?;
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
    Ok(results)
}

use surrealdb::types::SurrealValue;
