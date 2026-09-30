//! Actual isolated production pipeline; constant models prove plumbing, not model quality.
use super::*;
use crate::match_store::{DesiredMode, JobLifecycle, MatchStore};
use crate::match_worker::CpuExecutionPolicy;
use sha2::{Digest, Sha256};

#[test]
fn wp086_production_cpu_two_mixed_pipeline_and_later_asset() {
    mixed_pipeline(false);
}

#[test]
#[ignore = "requires bundled real identity models, face fixture and built isolated worker"]
fn wp086_production_cpu_two_real_model_mixed_pipeline_and_later_asset() {
    mixed_pipeline(true);
}

fn mixed_pipeline(real_model: bool) {
    let root = std::env::temp_dir().join(format!(
        "facial-cpu-two-pipeline-{}",
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&root).unwrap();
    let media = root.join("media");
    fs::create_dir(&media).unwrap();
    let source = media.join("face-a.png");
    let (manifest, generation) = if real_model {
        let product = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture = product
            .parent()
            .unwrap()
            .join("_source_checks/eDifFIQA/example_images/Aaron_Eckhart_0001.jpg");
        let manifest = PathBuf::from(
            std::env::var_os("FACIAL_WP086_MANIFEST").expect("explicit existing manifest required"),
        );
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        let pin = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&fixture)
            .unwrap();
        let mut encoded = Vec::new();
        (&pin)
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut encoded)
            .unwrap();
        assert!(
            encoded.len() <= 8 * 1024 * 1024,
            "real image exceeds pinned fixture bound"
        );
        image::load_from_memory(&encoded)
            .unwrap()
            .to_rgb8()
            .save(&source)
            .unwrap();
        let session = crate::identity::PreparationSession::begin(&manifest, "cpu").unwrap();
        let generation = session.generation().to_owned();
        drop(session);
        (manifest, generation)
    } else {
        image::RgbImage::from_pixel(640, 640, image::Rgb([127, 127, 127]))
            .save(&source)
            .unwrap();
        provision_models(&root)
    };
    fs::copy(&source, media.join("face-b.png")).unwrap();
    let video = media.join("face-video.mkv");
    let executable = crate::media_thumbs::resolve_ffmpeg().expect("FFmpeg required");
    let mut args: Vec<std::ffi::OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-loop",
        "1",
        "-i",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(source.as_os_str().to_owned());
    args.extend(
        [
            "-t", "1.5", "-r", "4", "-an", "-c:v", "ffv1", "-threads", "1", "-f", "matroska",
        ]
        .into_iter()
        .map(std::ffi::OsString::from),
    );
    args.push(video.as_os_str().to_owned());
    let (ok, _, stderr) =
        crate::match_decoder_process::run(&executable, &args, 1024, 65536).unwrap();
    assert!(
        ok,
        "fixture generation: {}",
        String::from_utf8_lossy(&stderr)
    );
    let original_image = Sha256::digest(fs::read(&source).unwrap());
    let original_video = Sha256::digest(fs::read(&video).unwrap());
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(&generation, true).unwrap();
    store.activate_model_generation(&generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let indexed = store.configure_index_root(&media, vec![]).unwrap();
    let coordinator = Arc::new(crate::media_io::MediaIoCoordinator::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut worker = None;
    let queued = store
        .start_index_job(&indexed.root_id, &generation)
        .unwrap();
    let job = store
        .set_job_lifecycle(&queued.job_id, JobLifecycle::Running)
        .unwrap();
    run_match_index_job_with_cpu_policy(
        store.clone(),
        job.job_id.clone(),
        &mut worker,
        &manifest,
        coordinator.clone(),
        cancelled.clone(),
        CpuExecutionPolicy::PrivateTwoThread,
    )
    .unwrap();
    let completed = store.job(&job.job_id).unwrap();
    assert_eq!(completed.lifecycle().unwrap(), JobLifecycle::Completed);
    assert_eq!((completed.completed, completed.failed), (3, 0));
    let assets = store.job_assets(&job.job_id).unwrap();
    assert_eq!(assets.len(), 3);
    let mut image_ids = std::collections::BTreeSet::new();
    let mut video_ids = std::collections::BTreeSet::new();
    let mut original_tracks = std::collections::BTreeSet::new();
    for asset in &assets {
        let faces = store.derived_faces_for_asset(asset).unwrap();
        assert!(
            !faces.is_empty(),
            "actual Face evidence required: {}",
            asset.media_key
        );
        let projection = store.warm_projection(&asset.media_key).unwrap().unwrap();
        assert_eq!(projection.media_fingerprint, asset.media_fingerprint);
        assert!(
            projection.person_ids.is_empty(),
            "no implicit Person assignment"
        );
        if asset
            .source_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".mkv"))
        {
            let tracks = store.video_media_tracks(&asset.media_key).unwrap();
            assert!(!tracks.is_empty());
            video_ids.extend(faces.iter().map(|face| face.face_id.clone()));
            original_tracks.extend(tracks.iter().map(|track| track.track_id.clone()));
            assert!(tracks.iter().all(|track| track.closed
                && track.observation_count > 0
                && track.exemplar_count > 0));
            for track in &tracks {
                assert!(store
                    .pending_video_exemplars(&asset.media_key, track.stream_index, &generation)
                    .unwrap()
                    .is_empty());
            }
        } else {
            assert!(faces.iter().all(|face| face.alignment_valid));
            for face in faces {
                assert!(image_ids.insert(face.face_id));
            }
        }
    }
    assert_eq!(store.governor().usage().unwrap().cpu_inference, 0);
    let worker_id = worker.as_ref().unwrap().worker_id().to_owned();
    assert_eq!(
        worker.as_ref().unwrap().cpu_policy(),
        CpuExecutionPolicy::PrivateTwoThread
    );
    // A new job and asset must use new full request fences while reusing the pool.
    let later_root = root.join("later-media");
    fs::create_dir(&later_root).unwrap();
    fs::copy(&source, later_root.join("face-later.png")).unwrap();
    let copied_video = later_root.join("face-video-copy.mkv");
    fs::copy(&video, &copied_video).unwrap();
    assert_eq!(
        Sha256::digest(fs::read(&copied_video).unwrap()),
        original_video
    );
    let later_index = store.configure_index_root(&later_root, vec![]).unwrap();
    let later = store
        .start_index_job(&later_index.root_id, &generation)
        .unwrap();
    let later = store
        .set_job_lifecycle(&later.job_id, JobLifecycle::Running)
        .unwrap();
    run_match_index_job_with_cpu_policy(
        store.clone(),
        later.job_id.clone(),
        &mut worker,
        &manifest,
        coordinator,
        cancelled.clone(),
        CpuExecutionPolicy::PrivateTwoThread,
    )
    .unwrap();
    assert_eq!(worker.as_ref().unwrap().worker_id(), worker_id);
    assert_eq!(
        store.job(&later.job_id).unwrap().lifecycle().unwrap(),
        JobLifecycle::Completed
    );
    let later_assets = store.job_assets(&later.job_id).unwrap();
    assert_eq!(later_assets.len(), 2);
    for asset in &later_assets {
        let later_faces = store.derived_faces_for_asset(asset).unwrap();
        assert!(!later_faces.is_empty());
        assert!(later_faces
            .iter()
            .all(|face| !image_ids.contains(&face.face_id) && !video_ids.contains(&face.face_id)));
        if asset
            .source_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".mkv"))
        {
            let copied_tracks = store.video_media_tracks(&asset.media_key).unwrap();
            assert!(!copied_tracks.is_empty());
            assert!(copied_tracks.iter().all(|track| track.closed
                && !original_tracks.contains(&track.track_id)
                && track
                    .observations
                    .iter()
                    .all(|observation| !video_ids.contains(&observation.face_id))));
            for track in &copied_tracks {
                for observation in track.observations.iter().filter(|row| row.exemplar) {
                    assert!(later_faces
                        .iter()
                        .any(|face| face.face_id == observation.face_id && face.alignment_valid));
                }
                assert!(store
                    .pending_video_exemplars(&asset.media_key, track.stream_index, &generation)
                    .unwrap()
                    .is_empty());
            }
        } else {
            assert!(later_faces.iter().all(|face| face.alignment_valid));
        }
    }
    assert_eq!(store.governor().usage().unwrap().cpu_inference, 0);
    if !real_model {
        let changed_embedder = root.join("fixture-embedder-changed.onnx");
        let mut changed_vector = vec![0.0; 512];
        changed_vector[1] = 1.0;
        fs::write(
            &changed_embedder,
            constant_model(112, vec![(vec![1, 512], changed_vector)]),
        )
        .unwrap();
        let changed_manifest = root
            .join("changed-model")
            .join("match-inference-manifest-v1.json");
        let changed_engine = IdentityEngine::provision(
            &changed_embedder,
            Some(&root.join("fixture-detector.onnx")),
            &changed_manifest,
        )
        .expect("changed constant embedder must pass secure provisioning");
        let changed_generation = changed_engine.generation().to_owned();
        drop(changed_engine);
        assert_ne!(changed_generation, generation);
        store
            .register_model_generation(&changed_generation, true)
            .unwrap();
        store
            .activate_model_generation(&changed_generation)
            .unwrap();

        let prior_worker_id = worker.as_ref().unwrap().worker_id().to_owned();
        let changed_root = root.join("changed-generation-media");
        fs::create_dir(&changed_root).unwrap();
        fs::copy(&source, changed_root.join("face-changed-generation.png")).unwrap();
        let changed_index = store.configure_index_root(&changed_root, vec![]).unwrap();
        let changed_job = store
            .start_index_job(&changed_index.root_id, &changed_generation)
            .unwrap();
        let changed_job = store
            .set_job_lifecycle(&changed_job.job_id, JobLifecycle::Running)
            .unwrap();
        run_match_index_job_with_cpu_policy(
            store.clone(),
            changed_job.job_id.clone(),
            &mut worker,
            &changed_manifest,
            Arc::new(crate::media_io::MediaIoCoordinator::new()),
            cancelled.clone(),
            CpuExecutionPolicy::PrivateTwoThread,
        )
        .unwrap();

        let changed_worker = worker.as_ref().unwrap();
        assert_ne!(changed_worker.worker_id(), prior_worker_id);
        assert_eq!(
            changed_worker.cpu_policy(),
            CpuExecutionPolicy::PrivateTwoThread
        );
        assert!(changed_worker.is_prepared_for(&changed_generation));
        let completed_changed_job = store.job(&changed_job.job_id).unwrap();
        assert_eq!(completed_changed_job.model_generation, changed_generation);
        assert_eq!(
            completed_changed_job.lifecycle().unwrap(),
            JobLifecycle::Completed
        );
        assert_eq!(
            (
                completed_changed_job.completed,
                completed_changed_job.failed
            ),
            (1, 0)
        );
        let changed_assets = store.job_assets(&changed_job.job_id).unwrap();
        assert_eq!(changed_assets.len(), 1);
        assert_eq!(changed_assets[0].model_generation, changed_generation);
        let changed_faces = store.derived_faces_for_asset(&changed_assets[0]).unwrap();
        assert!(!changed_faces.is_empty());
        let database =
            crate::surreal_store::open(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        let db = database.db();
        let generation_query = changed_generation.clone();
        let job_query = changed_job.job_id.clone();
        let changed_embeddings: Vec<crate::match_store::FaceEmbedding> =
            crate::surreal_store::run(async move {
                let mut response = db
                    .query("SELECT * OMIT id FROM match_face_embedding WHERE model_generation = $generation AND job_id = $job_id LIMIT 16;")
                    .bind(("generation", generation_query))
                    .bind(("job_id", job_query))
                    .await
                    .map_err(|error| error.to_string())?
                    .check()
                    .map_err(|error| error.to_string())?;
                response.take(0).map_err(|error| error.to_string())
            })
            .unwrap();
        assert!(!changed_embeddings.is_empty());
        assert!(changed_embeddings.iter().all(|embedding| {
            embedding.model_generation == changed_generation
                && changed_faces
                    .iter()
                    .any(|face| face.face_id == embedding.face_id)
                && embedding.active
                && embedding.vector[1] == 1.0
                && embedding.vector[0] == 0.0
                && embedding.vector[2..].iter().all(|value| *value == 0.0)
        }));
        assert!(changed_faces.iter().all(|face| {
            changed_embeddings
                .iter()
                .any(|embedding| embedding.face_id == face.face_id)
        }));
        drop(database);
        assert_eq!(store.governor().usage().unwrap().cpu_inference, 0);
        assert_eq!(
            store.governor().usage().unwrap().worker_memory_bytes,
            crate::match_store::WORKER_MEMORY_LIMIT_BYTES
        );
        // A successful generation-change run implies the prior worker's exit
        // was confirmed before the service admitted its replacement.
    }
    assert_eq!(Sha256::digest(fs::read(&source).unwrap()), original_image);
    assert_eq!(Sha256::digest(fs::read(&video).unwrap()), original_video);
    assert!(worker.as_mut().unwrap().shutdown_and_confirm());
    drop(worker);
    let path = crate::media_db::MediaDb::db_path(&root);
    drop(store);
    crate::surreal_store::wait_until_closed(&path).unwrap();
    fs::remove_dir_all(root).unwrap();
}
