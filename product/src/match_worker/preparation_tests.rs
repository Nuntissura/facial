#[cfg(all(windows, debug_assertions))]
use super::*;

#[test]
#[cfg(all(windows, debug_assertions))]
fn wp086_worker_drop_confirms_job_exit_before_releasing_both_leases() {
    use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};

    let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
    let request = ResourceRequest {
        queued_bytes: 1024,
        ..ResourceRequest::default()
    };
    let mut worker = IsolatedMatchWorker::spawn_fault_harness().unwrap();
    let exited = worker.process.exit_observer();
    worker.retain_preparation_resources(governor.try_acquire(request).unwrap());
    worker.retain_discovery_resources(governor.try_acquire(request).unwrap());
    assert!(!exited());
    assert_eq!(governor.usage().unwrap().queued_bytes, 2048);
    drop(worker);
    let deadline = Instant::now() + Duration::from_secs(3);
    while governor.usage().unwrap().queued_bytes != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(governor.usage().unwrap().queued_bytes, 0);
    assert!(
        exited(),
        "admission must not be released before the whole owned Job exits"
    );
}

#[test]
#[cfg(all(windows, debug_assertions))]
fn wp086_exit_reaper_retains_leases_while_owned_job_is_alive() {
    use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};

    let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
    let request = ResourceRequest {
        queued_bytes: 1024,
        ..ResourceRequest::default()
    };
    let mut worker = IsolatedMatchWorker::spawn_fault_harness().unwrap();
    let exited = worker.process.exit_observer();
    worker.process.retain_resources_until_exit(vec![
        governor.try_acquire(request).unwrap(),
        governor.try_acquire(request).unwrap(),
    ]);
    assert!(!exited());
    assert_eq!(governor.usage().unwrap().queued_bytes, 2048);
    assert!(worker.shutdown_and_confirm());
    let deadline = Instant::now() + Duration::from_secs(3);
    while governor.usage().unwrap().queued_bytes != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(exited());
    assert_eq!(governor.usage().unwrap().queued_bytes, 0);
}

#[test]
#[cfg(all(windows, debug_assertions))]
fn wp086_preparation_lease_retained_until_worker_exit_allows_fresh_retry() {
    use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};

    let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
    let request = ResourceRequest {
        queued_bytes: 240 * 1024 * 1024,
        ..ResourceRequest::default()
    };
    let mut worker = IsolatedMatchWorker::spawn_fault_harness()
        .expect("build facial-cli before owned worker lease proof");
    worker.retain_preparation_resources(governor.try_acquire(request).unwrap());
    assert!(!worker.confirmed_dead());
    worker.release_dead_preparation_resources();
    assert_eq!(governor.usage().unwrap().queued_bytes, request.queued_bytes);
    assert!(matches!(governor.try_acquire(request), Err(error) if error == "resource_pressure"));

    let fence = WorkerFence {
        job_id: "preparation-lease-job".into(),
        asset_id: "asset".into(),
        media_key: "media".into(),
        schema_generation: "schema".into(),
        identity_revision: 1,
        catalog_revision: 1,
        admission_epoch: 1,
        model_generation: "fixture-generation".into(),
        media_fingerprint: "fixture-fingerprint".into(),
        track_id: None,
        timestamp_ms: None,
    };
    let error = worker
        .exercise_fault(HarnessFault::Hang, &fence)
        .unwrap_err();
    assert_eq!(error.code, "safe_unit_timeout");
    assert!(error.quarantined);
    assert!(
        worker.shutdown_and_confirm(),
        "owned child must be confirmed dead"
    );
    // Keep the old worker object alive: retry must not depend on dropping it.
    assert_eq!(governor.usage().unwrap().queued_bytes, request.queued_bytes);
    worker.release_dead_preparation_resources();
    assert_eq!(governor.usage().unwrap().queued_bytes, 0);
    let replacement = governor
        .try_acquire(request)
        .expect("fresh preparation must fit after confirmed exit");
    worker.release_dead_preparation_resources();
    assert_eq!(governor.usage().unwrap().queued_bytes, request.queued_bytes);
    drop(worker);
    assert_eq!(governor.usage().unwrap().queued_bytes, request.queued_bytes);
    drop(replacement);
    assert_eq!(governor.usage().unwrap().queued_bytes, 0);
}

#[test]
#[cfg(all(windows, debug_assertions))]
fn wp086_compute_lease_releases_on_response_but_waits_for_quarantined_job_exit() {
    use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};
    let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
    let request = ResourceRequest {
        cpu_inference: 2,
        queued_bytes: 1024,
        decoded_bytes: 4096,
        ..Default::default()
    };
    let mut worker = IsolatedMatchWorker::spawn_fault_harness().unwrap();
    let exited = worker.process.exit_observer();
    assert!(!exited());
    worker.finish_compute_resources(governor.try_acquire(request).unwrap());
    assert_eq!(governor.usage().unwrap().cpu_inference, 0);
    // Deterministically model failed termination: quarantine remains set while
    // the real owned child is alive, without racing TerminateJobObject.
    worker.quarantined = true;
    worker.finish_compute_resources(governor.try_acquire(request).unwrap());
    assert!(!exited());
    assert_eq!(governor.usage().unwrap().cpu_inference, 2);
    assert_eq!(governor.usage().unwrap().queued_bytes, 1024);
    assert_eq!(governor.usage().unwrap().decoded_bytes, 4096);
    assert!(governor.try_acquire(request).is_err());
    assert!(worker.shutdown_and_confirm());
    let deadline = Instant::now() + Duration::from_secs(3);
    while governor.usage().unwrap().cpu_inference != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(exited());
    assert_eq!(governor.usage().unwrap().cpu_inference, 0);
    assert_eq!(governor.usage().unwrap().queued_bytes, 0);
    assert_eq!(governor.usage().unwrap().decoded_bytes, 0);
    let replacement = governor.try_acquire(request).unwrap();
    drop(worker);
    assert_eq!(governor.usage().unwrap().cpu_inference, 2);
    drop(replacement);
    assert_eq!(governor.usage().unwrap().cpu_inference, 0);
}
