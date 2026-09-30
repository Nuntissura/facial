//! Opt-in production-worker proof using local assets; no synthetic parity claims.
use super::*;

#[test]
#[ignore = "requires local real model/image, built facial-cli, and canonical Cargo guard TEMP"]
fn wp086_real_model_supervised_cpu_cuda_candidate_probe() {
    let product = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = product.parent().expect("product has repository parent");
    let model = product.join("models/w600k_r50.onnx");
    let image = repo.join("_source_checks/eDifFIQA/example_images/Aaron_Eckhart_0001.jpg");
    assert!(model.is_file(), "real model fixture missing");
    assert!(image.is_file(), "real image fixture missing");
    let guard_temp = repo
        .join("build-artifacts/tmp")
        .canonicalize()
        .expect("run through canonical Cargo guard");
    let actual_temp = std::env::temp_dir()
        .canonicalize()
        .expect("guard TEMP exists");
    assert!(
        actual_temp.starts_with(&guard_temp),
        "test TEMP must be inside canonical repository artifact root"
    );
    let root = actual_temp.join(format!("wp086-acceleration-real-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    struct OwnedFixture(PathBuf, bool);
    impl Drop for OwnedFixture {
        fn drop(&mut self) {
            if self.1 && !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.0);
            } else {
                eprintln!(
                    "Retained failed real-model fixture for independent reproduction: {}",
                    self.0.display()
                );
            }
        }
    }
    let mut owned = OwnedFixture(root, false);
    let manifest = owned.0.join("identity-manifest.json");
    // Explicit import/setup is distinct from each measured supervised Prepare.
    // Both measured runtimes independently reopen and verify these same bytes.
    drop(
        crate::identity::IdentityEngine::provision(&model, None, &manifest)
            .expect("real model provisioning failed"),
    );
    let (report, status) = execute(Options {
        manifest,
        image,
        samples: 2,
        cpu_only: false,
        cpu_two_thread: false,
        candidate_root: None,
    })
    .expect("real probe fixture input failed");
    println!("{report}");
    assert_eq!(report.get("promoted").and_then(Value::as_bool), Some(false));
    assert_eq!(
        report.get("candidate_only").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report.get("cpu_usable").and_then(Value::as_bool),
        Some(true),
        "real supervised CPU preparation/inference failed: {report}"
    );
    assert_eq!(
        status, 0,
        "candidate worker death was not confirmed: {report}"
    );
    assert_eq!(
        report.get("cpu_exit_confirmed").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report.get("cuda_exit_confirmed").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        report.get("selected_runtime").and_then(Value::as_str),
        Some("cpu")
    );
    assert_eq!(report["samples"].as_array().map(Vec::len), Some(2));
    if report["candidate_error"].is_string() {
        assert_eq!(report["candidate_parity"], Value::Bool(false));
        assert!(report["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|sample| sample["candidate_parity"] == Value::Bool(false)
                && sample["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("candidate_unavailable:"))));
    }
    owned.1 = true;
}

#[test]
#[cfg(windows)]
#[ignore = "requires FACIAL_WP086_MANIFEST, exact real face fixture, built facial-cli, and canonical Cargo guard TEMP"]
fn wp086_real_model_supervised_cpu_two_candidate_probe() {
    let product = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = product.parent().expect("product has repository parent");
    let manifest = PathBuf::from(
        std::env::var_os("FACIAL_WP086_MANIFEST").expect("explicit existing manifest required"),
    );
    let image = repo.join("_source_checks/eDifFIQA/example_images/Aaron_Eckhart_0001.jpg");
    assert!(manifest.is_file(), "explicit real manifest missing");
    assert!(image.is_file(), "exact real face fixture missing");
    let guard_temp = repo
        .join("build-artifacts/tmp")
        .canonicalize()
        .expect("run through canonical Cargo guard");
    assert!(
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .starts_with(&guard_temp),
        "test TEMP must be inside canonical repository artifact root"
    );
    let (_image_pin, image_bytes) = read_pinned(&image, 8 * 1024 * 1024).unwrap();
    let (_manifest_pin, manifest_bytes) = read_pinned(&manifest, 64 * 1024).unwrap();
    let manifest_json: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let expected_hash = format!("{:x}", Sha256::digest(&image_bytes));
    let (mut report, status) = execute(Options {
        manifest,
        image,
        samples: 3,
        cpu_only: false,
        cpu_two_thread: true,
        candidate_root: None,
    })
    .expect("real CPU-two probe input failed");
    report["proof_policy"] = json!(match_acceleration::POLICY);
    println!("{report}");
    assert_eq!(status, 0, "supervised CPU-two probe failed: {report}");
    assert_eq!(report["cpu_usable"], json!(true));
    assert_eq!(report["candidate_runtime"], json!("cpu_two_thread"));
    assert_eq!(report["generation"], manifest_json["generation"]);
    assert_eq!(report["input_sha256"], json!(expected_hash));
    assert_eq!(
        report["candidate_parity"],
        json!(true),
        "frozen parity rejected: {report}"
    );
    assert_eq!(report["candidate_only"], json!(true));
    assert_eq!(report["promoted"], json!(false));
    assert_eq!(
        report["candidate_resource_promotion_eligible"],
        json!(false)
    );
    assert_eq!(report["selected_runtime"], json!("cpu"));
    assert_eq!(report["cpu_exit_confirmed"], json!(true));
    assert!(report["cpu_memory_error"].is_null());
    let candidate = &report["cpu_two_thread"];
    assert_eq!(candidate["configured_cpu_units"], json!(2));
    assert_eq!(candidate["private_pool_threads"], json!(2));
    assert!(candidate["executor_init_micros"]
        .as_u64()
        .is_some_and(|value| value < 2_000_000));
    assert_eq!(candidate["exit_confirmed"], json!(true));
    assert!(candidate["error"].is_null());
    assert!(candidate["memory_error"].is_null());
    assert_eq!(candidate["prepared"]["generation"], report["generation"]);
    assert_eq!(
        candidate["prepared"]["detector_sha256"],
        report["detector_sha256"]
    );
    assert_eq!(
        candidate["prepared"]["embedder_sha256"],
        report["embedder_sha256"]
    );
    // execute calls the unchanged frozen parity_for gate for each same-input pair:
    // detections/landmarks, face geometry, vectors/cosine, and failure partition.
    let comparisons = candidate["samples"].as_array().unwrap();
    assert_eq!(comparisons.len(), 3);
    assert!(comparisons
        .iter()
        .all(|sample| sample["candidate_parity"] == json!(true) && sample["reason"].is_null()));
    for times in [
        &report["cpu_inference_micros"],
        &candidate["inference_micros"],
    ] {
        let times = times.as_array().unwrap();
        assert_eq!(times.len(), 3);
        assert!(times
            .iter()
            .all(|time| time.as_u64().is_some_and(|value| value < 2_000_000)));
    }
    for peak in [
        &report["cpu_peak_process_bytes"],
        &report["cpu_peak_job_bytes"],
        &candidate["peak_process_bytes"],
        &candidate["peak_job_bytes"],
    ] {
        assert!(
            peak.as_u64().is_some_and(
                |value| value > 0 && value <= crate::match_store::WORKER_MEMORY_LIMIT_BYTES
            ),
            "resident cap proof missing: {report}"
        );
    }
    // Fresh admission needs two CPU units, 240 MiB preparation bytes, and a
    // resident cap. Re-admission proves those reservations were released.
    // Active rejection also sees preparation pressure, so it is not an
    // independent measurement of the two CPU units.
    for _ in 0..3 {
        let mut fresh = IsolatedMatchWorker::spawn_cpu_two_thread_candidate()
            .expect("confirmed probe exit must release CPU and resident admission");
        let blocked = IsolatedMatchWorker::spawn_diagnostic_preparation();
        assert!(
            matches!(blocked, Err(error) if error.code == "resource_pressure"),
            "active CPU-two worker must retain shared admission"
        );
        assert!(fresh.shutdown_and_confirm());
        fresh.release_dead_preparation_resources();
        drop(fresh);
    }
    println!(
        "{}",
        json!({"proof_policy": match_acceleration::POLICY,
        "active_shared_admission_blocks_baseline": true, "confirmed_exit_readmission_cycles": 3,
        "resident_limit_bytes": crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
        "promoted": false})
    );
}
