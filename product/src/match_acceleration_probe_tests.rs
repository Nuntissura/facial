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
