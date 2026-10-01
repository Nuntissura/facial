//! Independent production-boundary probes for the shared database owner.
//! The production store path spawns the hidden facial-cli owner; no mock engine.

use crate::{media_db::MediaDb, surreal_store};
use std::{
    collections::VecDeque,
    io::Read,
    path::PathBuf,
    sync::{mpsc, Arc, Barrier},
    time::{Duration, Instant},
};
use surrealdb::types::{Bytes, Decimal, Number, Object, RecordId, Value};

fn workspace(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "facial-database-owner-independent-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn close_workspace(path: &PathBuf) {
    surreal_store::wait_until_closed(&MediaDb::db_path(path)).unwrap();
    std::fs::remove_dir_all(path).unwrap();
}

fn drain_output_tail(mut reader: impl Read) -> String {
    const MAX_TAIL: usize = 64 * 1024;
    let mut tail = VecDeque::with_capacity(MAX_TAIL);
    let mut block = [0u8; 4096];
    loop {
        match reader.read(&mut block) {
            Ok(0) => break,
            Ok(length) => {
                for byte in &block[..length] {
                    if tail.len() == MAX_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(*byte);
                }
            }
            Err(error) => return format!("child-output-read-error: {error}"),
        }
    }
    String::from_utf8_lossy(&tail.into_iter().collect::<Vec<_>>()).into_owned()
}

fn uncertain_operation_id(error: &str) -> &str {
    let id = error
        .split("operation_id=")
        .nth(1)
        .expect("unknown outcome omitted durable operation ID");
    assert_eq!(id.len(), 32, "unexpected operation ID shape");
    id
}

fn canonical_receipt(
    store: &surreal_store::Store,
    operation_id: &str,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let db = store.db();
        let id = operation_id.to_owned();
        let outcome = surreal_store::run(async move {
            let mut response = db.query(
                "SELECT operation_id, digest FROM ONLY type::record('facial_database_operation', $operation_id);"
            ).bind(("operation_id", id)).await.map_err(|error| error.to_string())?;
            response.take(0)
        });
        match outcome {
            Ok(row) => return row,
            Err(error)
                if error.contains("database_owner_exit_pending") && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("fresh canonical receipt query failed: {error}"),
        }
    }
}

#[test]
fn owner_preserves_pinned_sdk_index_and_typed_value_semantics() {
    let root = workspace("sdk-response");
    let database_root = MediaDb::db_path(&root);
    let store = surreal_store::open(&database_root).unwrap();
    let db = store.db();

    // SurrealDB 3.2.4's numeric-index take removes only that statement;
    // a query-level Ok may still contain indexed statement errors.
    let mut response = surreal_store::run(async move {
        db.query(
            "CREATE owner_probe:one SET value = 1; \
             LET $invalid: datetime = 'not-a-datetime'; \
             SELECT * FROM owner_probe:one;",
        )
        .await
        .map_err(|error| error.to_string())
    })
    .unwrap();
    let errors = response.take_errors();
    assert_eq!(errors.len(), 1, "exactly the typed LET statement must fail");
    assert!(
        errors.contains_key(&1),
        "statement indices shifted across IPC"
    );
    let created: Vec<Value> = response.take(0).unwrap();
    let selected: Vec<Value> = response.take(2).unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(selected.len(), 1);
    assert_eq!(created[0], selected[0]);
    assert!(
        response.check().is_ok(),
        "take_errors must remove only errors"
    );

    let record_id = RecordId::new("owner_probe", "typed-id");
    let db = store.db();
    let mut typed = surreal_store::run(async move {
        db.query("RETURN $id; RETURN NONE;")
            .bind(("id", record_id.clone()))
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let expected = Value::RecordId(RecordId::new("owner_probe", "typed-id"));
    let observed: Value = typed.take(0).unwrap();
    assert_eq!(
        observed, expected,
        "record ID became a plain string across IPC"
    );
    let none: Value = typed.take(1).unwrap();
    assert_eq!(
        none,
        Value::None,
        "Surreal NONE became JSON null across IPC"
    );
    let missing: Option<Value> = typed.take(2).unwrap();
    assert!(missing.is_none(), "missing statement must be absent");

    // This assertion comes from pinned surrealdb-3.2.4/src/opt/query.rs:
    // a present scalar NONE is lifted to one element for Vec<T>, while a
    // missing statement is an empty vector. It catches facade drift.
    let db = store.db();
    let mut present_none = surreal_store::run(async move {
        db.query("RETURN NONE;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let lifted: Vec<Value> = present_none.take(0).unwrap();
    assert_eq!(lifted, vec![Value::None]);
    let absent: Vec<Value> = present_none.take(1).unwrap();
    assert!(absent.is_empty());

    // The private binary wire must preserve typed Surreal values through the
    // real owner engine, including distinctions JSON alone can flatten.
    let mut nested = Object::new();
    nested.insert("none", Value::None);
    nested.insert("null", Value::Null);
    nested.insert("bytes", Value::Bytes(Bytes::from(vec![0, 1, 0xff, 0])));
    nested.insert(
        "decimal",
        Value::Number(Number::Decimal(Decimal::new(12345, 3))),
    );
    nested.insert(
        "record",
        Value::RecordId(RecordId::new("owner_probe", "typed-id")),
    );
    let expected_nested = Value::Object(nested);
    let bound = expected_nested.clone();
    let db = store.db();
    let mut typed = surreal_store::run(async move {
        db.query("RETURN $payload;")
            .bind(("payload", bound))
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let observed_nested: Value = typed.take(0).unwrap();
    assert_eq!(
        observed_nested, expected_nested,
        "typed nested Value changed across owner IPC"
    );

    drop(store);
    close_workspace(&root);
}

#[test]
fn nested_timeline_like_owner_scope_keeps_match_original_deadline() {
    let media_root = workspace("cross-root-media");
    let timeline_root = workspace("cross-root-timeline");
    let media = surreal_store::open(&MediaDb::db_path(&media_root)).unwrap();
    let timeline =
        surreal_store::open_database(&timeline_root.join("ledger"), "timeline", 1).unwrap();
    let media_scope = media
        .begin_match_unit(Instant::now() + Duration::from_millis(100))
        .unwrap();
    let timeline_scope = timeline.begin_media_unit().unwrap();
    std::thread::sleep(Duration::from_millis(130));
    let db = media.db();
    let expired = surreal_store::run(async move {
        db.query("RETURN 1;")
            .await
            .map_err(|error| error.to_string())
    });
    assert!(
        expired
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("safe_unit_timeout")),
        "nested Timeline owner scope erased the original Match deadline"
    );
    drop(timeline_scope);
    drop(media_scope);
    let db = media.db();
    let mut fresh = surreal_store::run(async move {
        db.query("RETURN 1;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let value: Value = fresh.take(0).unwrap();
    assert_eq!(value, Value::Number(1.into()));
    drop(timeline);
    drop(media);
    surreal_store::wait_until_closed(&timeline_root.join("ledger")).unwrap();
    std::fs::remove_dir_all(timeline_root).unwrap();
    close_workspace(&media_root);
}

#[test]
fn bounded_binary_encode_expires_before_dispatch_without_retiring_owner() {
    let root = workspace("encode-deadline");
    let database_root = MediaDb::db_path(&root);
    let store = surreal_store::open(&database_root).unwrap();
    let db = store.db();
    surreal_store::run(async move {
        db.query("DEFINE TABLE owner_probe SCHEMALESS;")
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let before = store.owner_diagnostics();
    let payload = "x".repeat(2 * 1024 * 1024);
    let started = Instant::now();
    let scope = store
        .begin_match_unit(started + Duration::from_millis(2_000))
        .unwrap();
    let db = store.db();
    let outcome = surreal_store::run(async move {
        db.query("BEGIN TRANSACTION; UPSERT owner_probe:encode_deadline SET marker = $payload; COMMIT TRANSACTION;")
            .bind(("payload", payload))
            .test_delay_encode_chunk(Duration::from_millis(50))
            .await
            .map_err(|error| error.to_string())
    });
    let error = outcome
        .as_ref()
        .err()
        .expect("slow encoding unexpectedly dispatched a transaction");
    assert!(
        error.contains("safe_unit_timeout"),
        "wrong pre-dispatch deadline result: {error}"
    );
    assert!(
        !error.contains("commit_outcome_unknown"),
        "undispatched SQL acquired commit uncertainty: {error}"
    );
    assert!(
        started.elapsed() <= Duration::from_millis(2_000),
        "encoding exceeded original Match deadline"
    );
    drop(scope);
    let after = store.owner_diagnostics();
    assert_eq!(after["owner_id"], before["owner_id"]);
    assert_eq!(
        after["epoch"], before["epoch"],
        "pre-dispatch encode timeout retired a healthy owner"
    );
    assert_eq!(
        after["pid"], before["pid"],
        "pre-dispatch encode timeout spawned a replacement owner"
    );
    let db = store.db();
    let mut response = surreal_store::run(async move {
        db.query("SELECT marker FROM ONLY owner_probe:encode_deadline;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let row: Option<serde_json::Value> = response.take(0).unwrap();
    assert!(row.is_none(), "timed-out encoder dispatched its mutation");
    drop(store);
    close_workspace(&root);
}

#[test]
fn timed_out_match_owner_cannot_duplicate_queued_media_mutation() {
    let root = workspace("media-after-match-timeout");
    let database_root = MediaDb::db_path(&root);
    let media = MediaDb::open(&root);
    assert!(media.is_writable(), "foreground Media store did not open");
    let favorite = root.join("one.jpg");
    let favorite_text = favorite.to_string_lossy().into_owned();
    media.add_favorite(&favorite_text).unwrap();
    assert_eq!(media.favorites().len(), 1);
    let store = surreal_store::open(&database_root).unwrap();
    let store_for_match = store.clone();
    let (started_tx, started_rx) = mpsc::sync_channel(1);

    let match_query = std::thread::spawn(move || {
        let started = Instant::now();
        let _match_scope = store_for_match
            .begin_match_unit(started + Duration::from_millis(2_000))
            .unwrap();
        started_tx.send(()).unwrap();
        let db = store_for_match.db();
        let result = surreal_store::run(async move {
            db.query("SLEEP 5s;")
                .await
                .map_err(|error| error.to_string())
        });
        (
            started.elapsed(),
            result.and_then(|response| response.check().map(|_| ())),
        )
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let admission_deadline = Instant::now() + Duration::from_millis(500);
    while store.owner_diagnostics()["pending_count"].as_u64() != Some(1) {
        assert!(
            Instant::now() < admission_deadline,
            "Match query never entered the owner queue"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));

    let foreground_started = Instant::now();
    let toggled = media.toggle_favorite(&favorite_text).unwrap();
    assert!(
        !toggled,
        "the one foreground toggle must remove the existing favorite"
    );
    assert!(
        foreground_started.elapsed() < Duration::from_secs(5),
        "Media waited for the full sleeping Match query"
    );
    let (elapsed, match_result) = match_query.join().unwrap();
    assert!(
        match_result.is_err(),
        "five-second Match query escaped its two-second unit"
    );
    assert!(
        elapsed <= Duration::from_millis(2_000),
        "Match terminal outcome missed original deadline: {elapsed:?}"
    );
    assert!(
        media.favorites().is_empty(),
        "foreground toggle was lost or replayed"
    );

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let lock = database_root.join("LOCK");
        assert!(lock.exists(), "the owner has no canonical SurrealKV lock");
        let second_engine_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(lock);
        assert!(
            second_engine_lock.is_err(),
            "a second engine could acquire the live root"
        );
    }

    drop(media);
    drop(store);
    surreal_store::wait_until_closed(&database_root).unwrap();
    let reopened = MediaDb::open(&root);
    assert!(
        reopened.is_writable(),
        "fresh owner could not reopen the canonical root"
    );
    assert!(
        reopened.favorites().is_empty(),
        "canonical favorite changed after owner replacement"
    );
    drop(reopened);
    close_workspace(&root);
}

#[test]
fn timed_out_transaction_reconciles_rollback_before_match_readmission() {
    let root = workspace("transaction-rollback");
    let database_root = MediaDb::db_path(&root);
    let store = surreal_store::open(&database_root).unwrap();
    let db = store.db();
    surreal_store::run(async move {
        db.query("DEFINE TABLE owner_probe SCHEMALESS;")
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let started = Instant::now();
    let _scope = store
        .begin_match_unit(started + Duration::from_millis(2_000))
        .unwrap();
    let db = store.db();
    let outcome = surreal_store::run(async move {
        db.query(
            "BEGIN TRANSACTION; \
             CREATE owner_probe:rollback SET committed = true; \
             SLEEP 5s; \
             COMMIT TRANSACTION;",
        )
        .await
        .map_err(|error| error.to_string())
    });
    assert!(
        outcome
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("commit_outcome_unknown")),
        "a killed in-flight transaction must report an uncertain outcome"
    );
    let operation_id = uncertain_operation_id(outcome.as_ref().err().unwrap()).to_owned();
    assert!(started.elapsed() <= Duration::from_millis(2_000));
    drop(_scope);
    let blocked = store.begin_match_unit(Instant::now() + Duration::from_secs(2));
    assert!(
        blocked
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("commit_outcome_unknown")),
        "Match must stop specifically for the unknown transaction outcome"
    );
    assert!(
        canonical_receipt(&store, &operation_id).is_none(),
        "a rolled-back transaction published a durable receipt"
    );

    // Reopen the canonical owner after confirmed termination, inspect its
    // durable receipt, and require a fresh canonical row read. No replay.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match store.reconcile_pending_operations() {
            Ok(()) => break,
            Err(error)
                if error.contains("database_owner_exit_pending") && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("canonical receipt reconciliation failed: {error}"),
        }
    }
    let db = store.db();
    let mut response = surreal_store::run(async move {
        db.query("SELECT committed FROM ONLY owner_probe:rollback;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let row: Option<serde_json::Value> = response.take(0).unwrap();
    assert!(
        row.is_none(),
        "a killed transaction leaked an uncommitted row"
    );
    drop(store);
    close_workspace(&root);
}

#[test]
fn match_admission_race_after_empty_check_cannot_dispatch_before_reconciliation() {
    let root = workspace("admission-uncertainty-race");
    let database_root = MediaDb::db_path(&root);
    let store = surreal_store::open(&database_root).unwrap();
    let db = store.db();
    surreal_store::run(async move {
        db.query("DEFINE TABLE owner_probe SCHEMALESS;")
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();

    let timed_store = store.clone();
    let timed = std::thread::spawn(move || {
        let started = Instant::now();
        let _scope = timed_store
            .begin_match_unit(started + Duration::from_millis(2_000))
            .unwrap();
        let db = timed_store.db();
        let outcome = surreal_store::run(async move {
            db.query("BEGIN TRANSACTION; CREATE owner_probe:race_rollback SET marker = true; SLEEP 5s; COMMIT TRANSACTION;")
                .await
                .map_err(|error| error.to_string())
        });
        (started.elapsed(), outcome)
    });
    let admission_deadline = Instant::now() + Duration::from_millis(500);
    while store.owner_diagnostics()["pending_count"].as_u64() != Some(1) {
        assert!(
            Instant::now() < admission_deadline,
            "timed transaction never entered the owner queue"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let passed_empty_check = Arc::new(Barrier::new(2));
    let resume_admission = Arc::new(Barrier::new(2));
    store
        .db()
        .test_pause_after_empty_uncertain(passed_empty_check.clone(), resume_admission.clone());
    let racing_store = store.clone();
    let racing = std::thread::spawn(move || {
        let scope = racing_store.begin_match_unit(Instant::now() + Duration::from_secs(5));
        match scope {
            Ok(_scope) => {
                let db = racing_store.db();
                surreal_store::run(async move {
                    db.query("BEGIN TRANSACTION; CREATE owner_probe:race_escape SET marker = true; COMMIT TRANSACTION;")
                        .await
                        .map_err(|error| error.to_string())?
                        .check()
                        .map_err(|error| error.to_string())
                })
            }
            Err(error) => Err(error),
        }
    });
    passed_empty_check.wait();
    let timed_result = timed.join();
    let pending_unknown = store.owner_diagnostics()["unreconciled_operations"].as_u64();
    resume_admission.wait();
    let racing_result = racing.join();
    let (elapsed, timed_outcome) = timed_result.unwrap();
    assert!(
        timed_outcome
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("commit_outcome_unknown")),
        "the first transaction did not establish an unknown outcome"
    );
    assert!(elapsed <= Duration::from_millis(2_000));
    assert_eq!(
        pending_unknown,
        Some(1),
        "the admission hook did not span a pending canonical reconciliation"
    );
    let racing_error = racing_result
        .unwrap()
        .err()
        .expect("new-epoch Match mutation dispatched while a prior outcome was unknown");
    assert!(
        racing_error.contains("commit_outcome_unknown"),
        "admission failed for a different reason: {racing_error}"
    );

    let foreground_deadline = Instant::now() + Duration::from_secs(5);
    let mut foreground = loop {
        let db = store.db();
        let outcome = surreal_store::run(async move {
            db.query("RETURN 17; SELECT marker FROM ONLY owner_probe:race_escape;")
                .await
                .map_err(|error| error.to_string())
        });
        match outcome {
            Ok(response) => break response,
            Err(error)
                if error.contains("database_owner_exit_pending")
                    && Instant::now() < foreground_deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("Media foreground query failed during Match uncertainty: {error}"),
        }
    };
    let value: Value = foreground.take(0).unwrap();
    assert_eq!(
        value,
        Value::Number(17.into()),
        "Media foreground work was blocked by Match uncertainty"
    );
    let escaped: Option<serde_json::Value> = foreground.take(1).unwrap();
    assert!(
        escaped.is_none(),
        "racing Match mutation reached the canonical store"
    );

    let reconcile_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match store.reconcile_pending_operations() {
            Ok(()) => break,
            Err(error)
                if error.contains("database_owner_exit_pending")
                    && Instant::now() < reconcile_deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("canonical reconciliation failed: {error}"),
        }
    }
    let _scope = store
        .begin_match_unit(Instant::now() + Duration::from_secs(2))
        .expect("Match did not reopen after explicit canonical reconciliation");
    let db = store.db();
    let mut fresh = surreal_store::run(async move {
        db.query("RETURN 1;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let value: Value = fresh.take(0).unwrap();
    assert_eq!(value, Value::Number(1.into()));
    drop(_scope);
    drop(store);
    close_workspace(&root);
}

#[test]
fn committed_transaction_with_lost_ack_reconciles_without_replay() {
    let root = workspace("committed-ack-loss");
    let database_root = MediaDb::db_path(&root);
    let store = surreal_store::open(&database_root).unwrap();
    let db = store.db();
    surreal_store::run(async move {
        db.query("CREATE owner_probe:ack_loss SET commits = 0;")
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    let started = Instant::now();
    let scope = store
        .begin_match_unit(started + Duration::from_millis(2_000))
        .unwrap();
    let db = store.db();
    let outcome = surreal_store::run(async move {
        db.query(
            "BEGIN TRANSACTION; UPDATE owner_probe:ack_loss SET commits += 1; COMMIT TRANSACTION;",
        )
        .test_delay_reply_after_commit(Duration::from_secs(5))
        .await
        .map_err(|error| error.to_string())
    });
    assert!(
        outcome
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("commit_outcome_unknown")),
        "post-commit ACK loss must report an uncertain outcome"
    );
    let operation_id = uncertain_operation_id(outcome.as_ref().err().unwrap()).to_owned();
    assert!(started.elapsed() <= Duration::from_millis(2_000));
    drop(scope);
    let blocked = store.begin_match_unit(Instant::now() + Duration::from_secs(2));
    assert!(
        blocked
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("commit_outcome_unknown")),
        "Match must wait for the exact uncertain transaction to reconcile"
    );
    let receipt = canonical_receipt(&store, &operation_id)
        .expect("committed transaction has no durable receipt");
    assert_eq!(
        receipt
            .get("operation_id")
            .and_then(serde_json::Value::as_str),
        Some(operation_id.as_str())
    );
    assert_eq!(
        receipt
            .get("digest")
            .and_then(serde_json::Value::as_str)
            .map(str::len),
        Some(64)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match store.reconcile_pending_operations() {
            Ok(()) => break,
            Err(error)
                if error.contains("database_owner_exit_pending") && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("committed receipt reconciliation failed: {error}"),
        }
    }
    let db = store.db();
    let mut response = surreal_store::run(async move {
        db.query("SELECT commits FROM ONLY owner_probe:ack_loss;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let row: Option<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(
        row.as_ref()
            .and_then(|row| row.get("commits"))
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "canonical committed increment was lost or replayed after ACK loss"
    );
    drop(store);
    surreal_store::wait_until_closed(&database_root).unwrap();
    let reopened = surreal_store::open(&database_root).unwrap();
    let db = reopened.db();
    let mut response = surreal_store::run(async move {
        db.query("SELECT commits FROM ONLY owner_probe:ack_loss;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let row: Option<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(
        row.as_ref()
            .and_then(|row| row.get("commits"))
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    drop(reopened);
    close_workspace(&root);
}

#[test]
fn owned_parent_crash_harness() {
    let Some(root) = std::env::var_os("FACIAL_DB_OWNER_CRASH_HARNESS_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let store = surreal_store::open(&MediaDb::db_path(&root)).unwrap();
    let db = store.db();
    surreal_store::run(async move {
        db.query("UPSERT owner_probe:parent_crash SET committed = true;")
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .unwrap();
    // Simulates an abrupt GUI/CLI parent exit, skipping Store Drop. Its
    // app-owned Job must terminate the hidden owner and release SurrealKV.
    std::process::exit(0);
}

#[test]
fn owner_job_exits_with_abrupt_parent_and_releases_committed_store() {
    let root = workspace("parent-crash");
    let current = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(current);
    command
        .arg("--exact")
        .arg("database_owner_independent_tests::owned_parent_crash_harness")
        .arg("--nocapture")
        .env("FACIAL_DB_OWNER_CRASH_HARNESS_ROOT", &root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut parent = command.spawn().unwrap();
    let stdout = parent.stdout.take().unwrap();
    let stderr = parent.stderr.take().unwrap();
    let stdout_tail = std::thread::spawn(move || drain_output_tail(stdout));
    let stderr_tail = std::thread::spawn(move || drain_output_tail(stderr));
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = parent.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = parent.kill(); // this test owns this exact harness child
            let _ = parent.wait();
            let stdout = stdout_tail.join().unwrap();
            let stderr = stderr_tail.join().unwrap();
            panic!("abrupt-parent harness did not finish within 15 seconds; stdout-tail={stdout:?}; stderr-tail={stderr:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_tail.join().unwrap();
    let stderr = stderr_tail.join().unwrap();
    assert!(
        status.success(),
        "abrupt-parent harness did not commit its marker: {status}; stdout-tail={stdout:?}; stderr-tail={stderr:?}"
    );

    let database_root = MediaDb::db_path(&root);
    let reopened = surreal_store::open(&database_root).unwrap();
    let db = reopened.db();
    let mut response = surreal_store::run(async move {
        db.query("SELECT committed FROM ONLY owner_probe:parent_crash;")
            .await
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let marker: Option<serde_json::Value> = response.take(0).unwrap();
    assert_eq!(
        marker.and_then(|row| row.get("committed").cloned()),
        Some(serde_json::Value::Bool(true))
    );
    drop(reopened);
    close_workspace(&root);
}
