//! Opt-in real CUDA allocation proof through the actual hidden worker; no model promotion.
use super::*;
#[test]
#[ignore = "requires built facial-cli and the exact verified owned CUDA candidate payload"]
fn wp086_cuda_managed_pool_rejects_single_and_aggregate_excess_and_reuses_after_async_free() {
    use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let root = repo
        .join("build-artifacts/tmp/wp086-cuda/runtime")
        .canonicalize()
        .expect("verified candidate runtime required");
    let limit = 16 * 1024 * 1024;
    let governor = MatchResourceGovernor::new(ResourceBudget::default()).unwrap();
    let resources = governor
        .try_acquire(ResourceRequest {
            queued_bytes: 8 * 1024 * 1024,
            worker_memory_bytes: crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
            gpu_vram_bytes: limit,
            ..Default::default()
        })
        .unwrap();
    let current = std::env::current_exe().unwrap();
    let mut worker = IsolatedMatchWorker::spawn_at_with_candidate_resources(
        &worker_executable(&current, true).unwrap(),
        false,
        resources,
        Some(&root),
    )
    .unwrap();
    let fence = WorkerFence {
        job_id: uuid::Uuid::new_v4().to_string(),
        asset_id: "candidate-pool".into(),
        media_key: "candidate-pool".into(),
        identity_revision: 1,
        catalog_revision: 1,
        track_id: None,
        timestamp_ms: None,
        schema_generation: "candidate-pool-proof-v1".into(),
        model_generation: "a".repeat(64),
        media_fingerprint: "b".repeat(64),
        admission_epoch: 1,
    };
    let native = worker
        .bootstrap_candidate_limit(&root, limit, &fence)
        .expect("supervised payload bootstrap failed");
    println!(
        "{}",
        serde_json::json!({"candidate_only":true,"driver_api":native.driver_api,"cudart_api":native.cudart_api,"pool_limit_bytes":limit})
    );
    let mut samples = Vec::new();
    for step in 0..=8 {
        let response = worker
            .execute(Operation::CandidatePoolBoundary { step }, &fence)
            .expect("supervised pool boundary failed");
        let Output::CandidatePoolBoundary {
            step: observed,
            peaks,
        } = response
        else {
            panic!("unexpected boundary output")
        };
        assert_eq!(observed, step);
        assert_eq!(
            governor.usage().unwrap().gpu_vram_bytes,
            limit,
            "async free must not release worker reservation"
        );
        if let Some(peaks) = peaks {
            // Emit before assertions so a rejected allocation bound preserves its numeric evidence.
            println!(
                "{}",
                serde_json::json!({"candidate_only":true,"pool_boundary_step":step,"pool_limit_bytes":peaks.limit_bytes,"reserved_high_bytes":peaks.reserved_high_bytes,"used_high_bytes":peaks.used_high_bytes})
            );
            assert_eq!(peaks.limit_bytes, limit);
            assert!(
                peaks.used_high_bytes <= peaks.reserved_high_bytes,
                "step={step} used_high={} reserved_high={}",
                peaks.used_high_bytes,
                peaks.reserved_high_bytes
            );
            assert!(
                peaks.reserved_high_bytes <= limit,
                "step={step} limit={limit} reserved_high={} used_high={}",
                peaks.reserved_high_bytes,
                peaks.used_high_bytes
            );
            if step >= 6 {
                assert!(peaks.used_high_bytes >= 9 * 1024 * 1024);
            } else if step >= 2 {
                assert!(peaks.used_high_bytes >= 8 * 1024 * 1024);
            }
            samples.push((step, peaks.reserved_high_bytes, peaks.used_high_bytes));
        } else {
            assert!(step < 2, "live pool omitted counters");
        }
    }
    assert_eq!(samples.len(), 7);
    assert!(
        worker.shutdown_and_confirm(),
        "owned CUDA job exit unconfirmed"
    );
    drop(worker);
    assert_eq!(governor.usage().unwrap().gpu_vram_bytes, 0);
    println!(
        "{}",
        serde_json::json!({"candidate_only": true, "promoted": false, "pool_limit_bytes": limit, "steps_reserved_used": samples, "driver_library_overhead_measured": false})
    );
}
