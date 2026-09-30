//! Real isolated-worker pipeline with deterministic, explicitly provisioned test models.
#![cfg(windows)]
use super::*;
use prost::Message;
use tract_onnx::pb::{
    self, GraphProto, ModelProto, OperatorSetIdProto, TensorProto, ValueInfoProto,
};

fn value_info(name: &str, dimensions: &[i64]) -> ValueInfoProto {
    ValueInfoProto {
        name: name.into(),
        r#type: Some(pb::TypeProto {
            value: Some(pb::type_proto::Value::TensorType(pb::type_proto::Tensor {
                elem_type: pb::tensor_proto::DataType::Float as i32,
                shape: Some(pb::TensorShapeProto {
                    dim: dimensions
                        .iter()
                        .map(|dimension| pb::tensor_shape_proto::Dimension {
                            value: Some(pb::tensor_shape_proto::dimension::Value::DimValue(
                                *dimension,
                            )),
                            ..Default::default()
                        })
                        .collect(),
                }),
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn constant_model(input_edge: i64, outputs: Vec<(Vec<i64>, Vec<f32>)>) -> Vec<u8> {
    let mut graph = GraphProto {
        name: "wp086-deterministic-pipeline-fixture".into(),
        input: vec![value_info("image", &[1, 3, input_edge, input_edge])],
        ..Default::default()
    };
    for (index, (dimensions, values)) in outputs.into_iter().enumerate() {
        let name = format!("output_{index}");
        assert_eq!(dimensions.iter().product::<i64>() as usize, values.len());
        graph.initializer.push(TensorProto {
            name: name.clone(),
            dims: dimensions.clone(),
            data_type: pb::tensor_proto::DataType::Float as i32,
            float_data: values,
            ..Default::default()
        });
        graph.output.push(value_info(&name, &dimensions));
    }
    ModelProto {
        ir_version: 8,
        graph: Some(graph),
        opset_import: vec![OperatorSetIdProto {
            domain: String::new(),
            version: 13,
        }],
        ..Default::default()
    }
    .encode_to_vec()
}

fn provision_models(root: &Path) -> (PathBuf, String) {
    let mut planes = Vec::new();
    let anchor = 40 * 80 + 40;
    for group in 0..4 {
        for (stride_index, cells) in [6400usize, 1600, 400].into_iter().enumerate() {
            let width = [1, 1, 4, 10][group];
            let mut values = vec![0f32; cells * width];
            if stride_index == 0 {
                match group {
                    0 | 1 => values[anchor] = 0.99,
                    2 => values[anchor * 4..anchor * 4 + 4].copy_from_slice(&[
                        0.,
                        0.,
                        40f32.ln(),
                        40f32.ln(),
                    ]),
                    3 => {
                        // One frontal, nondegenerate landmark set inside the 320px box.
                        for (index, [x, y]) in [
                            [276., 292.],
                            [364., 292.],
                            [320., 340.],
                            [284., 384.],
                            [356., 384.],
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            values[anchor * 10 + index * 2] = x / 8. - 40.;
                            values[anchor * 10 + index * 2 + 1] = y / 8. - 40.;
                        }
                    }
                    _ => unreachable!(),
                }
            }
            planes.push((vec![1, cells as i64, width as i64], values));
        }
    }
    let detector = root.join("fixture-detector.onnx");
    let embedder = root.join("fixture-embedder.onnx");
    fs::write(&detector, constant_model(640, planes)).unwrap();
    let mut vector = vec![0.; 512];
    vector[0] = 1.;
    fs::write(&embedder, constant_model(112, vec![(vec![1, 512], vector)])).unwrap();
    let manifest = root.join("models").join("match-inference-manifest-v1.json");
    let engine = IdentityEngine::provision(&embedder, Some(&detector), &manifest).expect(
        "constant ONNX fixtures must pass normal secure provisioning and startup inference",
    );
    let generation = engine.generation().to_string();
    drop(engine);
    (manifest, generation)
}

#[test]
fn wp086_actual_video_pipeline_constant_models_complete_and_reopen() {
    pipeline_fixture(false);
}

#[test]
fn wp086_actual_image_pipeline_pinned_source_and_decode_failure_reopen() {
    use crate::match_store::{DesiredMode, JobLifecycle, MatchStore};
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!(
        "facial-image-pipeline-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir(&root).unwrap();
    let media = root.join("media");
    fs::create_dir(&media).unwrap();
    let source = media.join("valid.png");
    image::RgbImage::from_pixel(640, 640, image::Rgb([127, 127, 127]))
        .save(&source)
        .unwrap();
    let corrupt = media.join("corrupt.png");
    fs::write(&corrupt, b"invalid image header").unwrap();
    let before = Sha256::digest(fs::read(&source).unwrap());
    let (manifest, generation) = provision_models(&root);
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(&generation, true).unwrap();
    store.activate_model_generation(&generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let indexed = store.configure_index_root(&media, Vec::new()).unwrap();
    let queued = store
        .start_index_job(&indexed.root_id, &generation)
        .unwrap();
    let job = store
        .set_job_lifecycle(&queued.job_id, JobLifecycle::Running)
        .unwrap();
    let coordinator = Arc::new(crate::media_io::MediaIoCoordinator::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut worker = None;
    run_match_index_job(
        store.clone(),
        job.job_id.clone(),
        &mut worker,
        &manifest,
        coordinator,
        cancelled,
    )
    .unwrap();
    let completed = store.job(&job.job_id).unwrap();
    assert_eq!(completed.lifecycle().unwrap(), JobLifecycle::Partial);
    assert_eq!((completed.completed, completed.failed), (1, 1));
    let assets = store.job_assets(&job.job_id).unwrap();
    assert_eq!(assets.len(), 2);
    let valid_key = match_media_key(&indexed.root_id, Path::new("valid.png"));
    let valid = assets.iter().find(|a| a.media_key == valid_key).unwrap();
    let invalid = assets.iter().find(|a| a.media_key != valid_key).unwrap();
    assert_eq!(invalid.failure_code.as_deref(), Some("decode"));
    assert!(valid.failure_code.is_none());
    let faces = store.derived_faces_for_asset(valid).unwrap();
    assert_eq!(faces.len(), 1);
    assert_eq!(
        (
            faces[0].source_width,
            faces[0].source_height,
            faces[0].exif_orientation
        ),
        (Some(640), Some(640), Some(1))
    );
    assert!(faces[0].alignment_valid);
    assert!(store.derived_faces_for_asset(invalid).unwrap().is_empty());
    let projection = store
        .warm_projection(&valid.media_key)
        .unwrap()
        .expect("image Persist publishes projection");
    assert_eq!(projection.media_fingerprint, valid.media_fingerprint);
    assert!(projection.person_ids.is_empty());
    assert!(worker.as_mut().unwrap().shutdown_and_confirm());
    drop(worker);
    drop(store);
    crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
    let reopened = MatchStore::open(&root).unwrap();
    assert_eq!(
        reopened.derived_faces_for_asset(valid).unwrap()[0].face_id,
        faces[0].face_id
    );
    assert_eq!(
        reopened.warm_projection(&valid.media_key).unwrap().unwrap(),
        projection
    );
    assert_eq!(
        reopened.job(&job.job_id).unwrap().lifecycle().unwrap(),
        JobLifecycle::Partial
    );
    assert_eq!(Sha256::digest(fs::read(&source).unwrap()), before);
    assert_eq!(fs::read(&corrupt).unwrap(), b"invalid image header");
    drop(reopened);
    crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wp086_actual_video_pipeline_playback_hold_checkpoint_and_fresh_worker_resume() {
    pipeline_fixture(true);
}

fn pipeline_fixture(pause_after_frame: bool) {
    use crate::match_store::{DesiredMode, JobLifecycle, MatchStore};
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!(
        "facial-video-pipeline-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir(&root).unwrap();
    let media = root.join("media");
    fs::create_dir(&media).unwrap();
    let source = media.join("static-video.mkv");
    let executable =
        crate::media_thumbs::resolve_ffmpeg().expect("FFmpeg required for video pipeline fixture");
    let mut args: Vec<std::ffi::OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-f",
        "lavfi",
        "-i",
        "color=c=gray:size=32x32:rate=4:duration=1.5",
        "-map",
        "0:v:0",
        "-frames:v",
        "6",
        "-c:v",
        "ffv1",
        "-threads",
        "1",
        "-f",
        "matroska",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(source.as_os_str().to_owned());
    let (success, _, stderr) =
        crate::match_decoder_process::run(&executable, &args, 1024, 65536).unwrap();
    assert!(
        success,
        "fixture video encode: {}",
        String::from_utf8_lossy(&stderr)
    );
    let before = Sha256::digest(fs::read(&source).unwrap());
    let (manifest, generation) = provision_models(&root);
    let holds = Arc::new(crate::match_store::MatchExternalHolds::default());
    let mut store = MatchStore::open(&root)
        .unwrap()
        .with_external_holds(Arc::clone(&holds));
    store.register_model_generation(&generation, true).unwrap();
    store.activate_model_generation(&generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let indexed = store.configure_index_root(&media, Vec::new()).unwrap();
    let queued = store
        .start_index_job(&indexed.root_id, &generation)
        .unwrap();
    let job = store
        .set_job_lifecycle(&queued.job_id, JobLifecycle::Running)
        .unwrap();
    let coordinator = Arc::new(crate::media_io::MediaIoCoordinator::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut worker = None;
    let mut first_observation = None;
    if pause_after_frame {
        use crate::match_store::JobStage;
        use crate::media_io::{PermitOutcome, RootIdentity, RootKind};
        let root_identity = RootIdentity::new(
            indexed.root_id.clone(),
            job.identity_revision,
            RootKind::Unknown,
        );
        let mut child = crate::match_worker::IsolatedMatchWorker::spawn().unwrap();
        let lease = store
            .acquire_worker_compute(
                &coordinator,
                root_identity.clone(),
                &job.job_id,
                None,
                match_compute_request(65536, 1),
                &mut None,
            )
            .unwrap();
        child
            .prepare(
                &manifest,
                &match_worker_fence(&job, None, lease.admission_epoch()),
            )
            .unwrap();
        lease.finish(PermitOutcome::Success);
        let snapshot = match_video_jobs::video_discovery_snapshot(
            &store,
            &job,
            &mut child,
            &source,
            Path::new(&indexed.path),
            &coordinator,
            &root_identity,
            &cancelled,
        )
        .unwrap();
        let media_key = match_media_key(&indexed.root_id, Path::new("static-video.mkv"));
        let discovery = store
            .acquire_discovery_write(
                &job.job_id,
                &media_key,
                match_stage_request(JobStage::Discover, 65536, 0),
            )
            .unwrap();
        let asset = store
            .enqueue_asset_with_source_path(
                &job.job_id,
                &media_key,
                &snapshot.fingerprint,
                Some(&source),
                &discovery,
            )
            .unwrap();
        drop(discovery);
        let fence = match_revision_fence(&job, &asset);
        let permit = store
            .acquire_background_stage(
                &coordinator,
                root_identity.clone(),
                &fence,
                JobStage::Discover,
                match_stage_request(JobStage::Discover, 65536, 0),
            )
            .unwrap();
        store
            .commit_asset_stage(&asset.asset_id, JobStage::Discover, &fence, &permit)
            .unwrap();
        drop(permit);
        let lease = store
            .acquire_worker_compute(
                &coordinator,
                root_identity.clone(),
                &job.job_id,
                Some(&fence),
                match_compute_request(16 * 1024 * 1024, 512 * 1024 * 1024),
                &mut None,
            )
            .unwrap();
        let first = child
            .decode(
                &snapshot.final_path,
                0,
                crate::match_video_decode::FIRST_VIDEO_STREAM,
                &match_worker_fence(&job, Some(&asset), lease.admission_epoch()),
            )
            .unwrap()
            .unwrap();
        lease.finish(PermitOutcome::Success);
        let lease = store
            .acquire_worker_compute(
                &coordinator,
                root_identity.clone(),
                &job.job_id,
                Some(&fence),
                match_compute_request(16 * 1024 * 1024, 512 * 1024 * 1024),
                &mut None,
            )
            .unwrap();
        let epoch = lease.admission_epoch();
        let detected = child
            .detect(
                first.encoded.clone(),
                &snapshot.final_path,
                &match_worker_fence(&job, Some(&asset), epoch),
            )
            .unwrap();
        lease.finish(PermitOutcome::Success);
        let detections = detected
            .faces
            .into_iter()
            .map(|face| crate::match_video::VideoDetection {
                source_index: face.source_index as u32,
                bounds: [
                    face.bbox[0] / detected.image_w as f32,
                    face.bbox[1] / detected.image_h as f32,
                    face.bbox[2] / detected.image_w as f32,
                    face.bbox[3] / detected.image_h as f32,
                ],
                quality: face.score,
                pose_bucket: crate::identity::yaw_bucket(&face.landmarks).0.into(),
                detector_generation: generation.clone(),
            })
            .collect();
        let permit = store
            .acquire_background_stage(
                &coordinator,
                root_identity,
                &fence,
                JobStage::Detect,
                match_stage_request(JobStage::Detect, 16 * 1024 * 1024, 1),
            )
            .unwrap();
        store
            .commit_video_frame(
                &fence,
                &permit,
                epoch,
                &crate::match_video::VideoPolicy::default(),
                crate::match_video::VideoFrame {
                    stream_index: first.stream_index,
                    time: first.time,
                    playback_origin: first.playback_origin,
                    frame_sha256: first.frame_sha256,
                    scene_score: 1.0,
                    detections,
                },
            )
            .unwrap();
        drop(permit);
        let pending = store
            .pending_video_exemplars(&media_key, first.stream_index, &generation)
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "interrupt between durable frame and embedding publication"
        );
        first_observation = Some((
            pending[0].observation_id.clone(),
            pending[0].face_id.clone(),
        ));
        let checkpoint = serde_json::to_value(
            store
                .video_checkpoint(&media_key, first.stream_index)
                .unwrap(),
        )
        .unwrap();
        let prior_worker_id = child.worker_id().to_string();
        worker = Some(child);
        holds.set_playback(true);
        run_match_index_job(
            store.clone(),
            job.job_id.clone(),
            &mut worker,
            &manifest,
            Arc::clone(&coordinator),
            Arc::clone(&cancelled),
        )
        .unwrap();
        assert_eq!(store.desired_mode().unwrap(), DesiredMode::Running);
        assert_eq!(
            store
                .job_asset(&asset.asset_id)
                .unwrap()
                .next_stage()
                .unwrap(),
            JobStage::Detect
        );
        assert_eq!(
            serde_json::to_value(
                store
                    .video_checkpoint(&media_key, first.stream_index)
                    .unwrap()
            )
            .unwrap(),
            checkpoint
        );
        drop(worker.take());
        drop(store);
        crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
        store = MatchStore::open(&root)
            .unwrap()
            .with_external_holds(Arc::clone(&holds));
        assert_eq!(
            serde_json::to_value(
                store
                    .video_checkpoint(&media_key, first.stream_index)
                    .unwrap()
            )
            .unwrap(),
            checkpoint
        );
        assert_eq!(
            store
                .pending_video_exemplars(&media_key, first.stream_index, &generation)
                .unwrap()
                .len(),
            1
        );
        holds.set_playback(false);
        run_match_index_job(
            store.clone(),
            job.job_id.clone(),
            &mut worker,
            &manifest,
            Arc::clone(&coordinator),
            Arc::clone(&cancelled),
        )
        .unwrap();
        assert_ne!(worker.as_ref().unwrap().worker_id(), prior_worker_id);
    }
    run_match_index_job(
        store.clone(),
        job.job_id.clone(),
        &mut worker,
        &manifest,
        Arc::clone(&coordinator),
        cancelled,
    )
    .unwrap();
    let completed = store.job(&job.job_id).unwrap();
    assert_eq!(
        completed.lifecycle().unwrap(),
        JobLifecycle::Completed,
        "{:?}",
        store.job_assets(&job.job_id).unwrap()
    );
    assert_eq!(completed.completed, 1);
    assert_eq!(completed.failed, 0);
    let assets = store.job_assets(&job.job_id).unwrap();
    assert_eq!(assets.len(), 1);
    let asset = &assets[0];
    let tracks = store.video_media_tracks(&asset.media_key).unwrap();
    assert_eq!(
        tracks.len(),
        1,
        "static geometry and no scene cut form one track"
    );
    if let Some((observation_id, face_id)) = first_observation {
        assert_eq!(
            tracks[0]
                .observations
                .iter()
                .filter(|row| row.observation_id == observation_id && row.face_id == face_id)
                .count(),
            1,
            "fresh-worker resume preserves exact persisted observation and Face IDs"
        );
    }
    assert!(tracks[0].closed);
    assert_eq!(
        tracks[0].observation_count, 3,
        "0/500/1000 ms scene/time samples"
    );
    assert!(tracks[0].exemplar_count > 0 && tracks[0].exemplar_count <= 5);
    assert!(
        tracks[0].exemplar_count < tracks[0].observation_count,
        "do not embed every frame"
    );
    assert!(store
        .pending_video_exemplars(&asset.media_key, 0, &generation)
        .unwrap()
        .is_empty());
    let faces = store.derived_faces_for_asset(asset).unwrap();
    assert_eq!(faces.len(), 3);
    let current_exemplar_ids = tracks[0]
        .observations
        .iter()
        .filter(|row| row.exemplar)
        .map(|row| row.face_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(current_exemplar_ids.len(), tracks[0].exemplar_count);
    for face in &faces {
        if current_exemplar_ids.contains(face.face_id.as_str()) {
            assert!(
                face.alignment_valid,
                "every current exemplar has canonical alignment"
            );
        }
    }
    let database = crate::surreal_store::open(&crate::media_db::MediaDb::db_path(&root)).unwrap();
    let db = database.db();
    let generation_query = generation.clone();
    let vectors: Vec<crate::match_store::FaceEmbedding> = crate::surreal_store::run(async move {
        let mut response = db.query("SELECT * OMIT id FROM match_face_embedding WHERE model_generation=$generation LIMIT 16;")
            .bind(("generation",generation_query)).await.map_err(|error|error.to_string())?.check().map_err(|error|error.to_string())?;
        response.take(0).map_err(|error|error.to_string())
    }).unwrap();
    assert_eq!(
        vectors
            .iter()
            .filter(|vector| vector.active)
            .map(|vector| vector.face_id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        current_exemplar_ids
    );
    assert_eq!(
        vectors.iter().filter(|vector| vector.active).count(),
        tracks[0].exemplar_count
    );
    for face in faces.iter().filter(|face| face.alignment_valid) {
        let vector = vectors
            .iter()
            .find(|vector| vector.face_id == face.face_id)
            .expect("completed exemplar alignment retains its vector record");
        assert_eq!(
            vector.active,
            current_exemplar_ids.contains(face.face_id.as_str()),
            "superseded exemplar vectors must be inactive"
        );
        assert_eq!(vector.face_revision, face.face_revision);
        assert_eq!(vector.schema_generation, face.schema_generation);
    }
    for vector in &vectors {
        assert_eq!(vector.vector.len(), 512);
        assert_eq!(vector.vector[0], 1.0);
        assert!(vector.vector[1..].iter().all(|value| *value == 0.0));
        assert_eq!(vector.media_fingerprint, asset.media_fingerprint);
    }
    drop(database);
    let checkpoint = store
        .video_checkpoint(&asset.media_key, 0)
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.sample_count, 3);
    assert_eq!(checkpoint.last_time.unwrap().milliseconds().unwrap(), 1000);
    let projection = store
        .warm_projection(&asset.media_key)
        .unwrap()
        .expect("video Persist must publish People projection");
    assert_eq!(projection.media_fingerprint, asset.media_fingerprint);
    assert_eq!(projection.model_generation, generation);
    assert!(
        projection.person_ids.is_empty(),
        "fixture model must not invent Person assignments"
    );
    drop(worker.take());
    drop(store);
    crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
    let reopened = MatchStore::open(&root).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.video_media_tracks(&asset.media_key).unwrap()).unwrap(),
        serde_json::to_value(&tracks).unwrap()
    );
    assert!(reopened
        .pending_video_exemplars(&asset.media_key, 0, &generation)
        .unwrap()
        .is_empty());
    assert_eq!(
        reopened.job(&job.job_id).unwrap().lifecycle().unwrap(),
        JobLifecycle::Completed
    );
    assert_eq!(
        reopened.warm_projection(&asset.media_key).unwrap().unwrap(),
        projection
    );
    assert_eq!(Sha256::digest(fs::read(&source).unwrap()), before);
    if !pause_after_frame {
        // Exercise the actual inspection service against independently reopened
        // canonical video state; the Viewer paint fixture is separate.
        reopened
            .set_desired_mode(DesiredMode::OperatorPaused)
            .unwrap();
        let request: crate::api::MatchVideoRequest = serde_json::from_value(serde_json::json!({
            "action":"inspect_appearance", "media_key":asset.media_key,
            "track_id":tracks[0].track_id, "track_revision":tracks[0].revision,
            "timestamp":tracks[0].timestamps[0]
        }))
        .unwrap();
        let frame = FacialService::match_inspect_appearance(
            reopened.clone(),
            &request,
            &coordinator,
            &AtomicBool::new(false),
        )
        .unwrap();
        let canonical = reopened
            .video_inspection_observation(
                &tracks[0].track_id,
                tracks[0].revision,
                tracks[0].timestamps[0],
            )
            .unwrap()
            .observation()
            .unwrap();
        assert_eq!(frame.sample.frame_sha256, canonical.frame_sha256);
        assert_eq!(frame.sample.time, canonical.time);
        assert_eq!(frame.sample.stream_index, canonical.stream_index);
        assert_eq!(frame.sample.playback_origin, canonical.playback_origin);
        drop(frame);
        assert_eq!(
            reopened.governor().usage().unwrap(),
            crate::match_store::ResourceUsage::default()
        );
        let mut seek_request = request.clone();
        seek_request.action = crate::api::MatchVideoAction::SeekAppearance;
        let prepared = FacialService::match_inspect_appearance(
            reopened.clone(),
            &seek_request,
            &coordinator,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut player = crate::video_player::VideoPlayer::default();
        player.set_appearance_profile(Some(
            crate::video_player::AppearancePlaybackProfile::for_container(
                prepared.sample.container,
            )
            .unwrap(),
        ));
        player.set_appearance_source(Arc::clone(prepared.playback_pin.as_ref().unwrap()));
        drop(prepared); // worker is confirmed dead; only the native-owner RAII pin remains.
        assert!(fs::OpenOptions::new().write(true).open(&source).is_err());
        let renamed_media = root.join("media-renamed-for-pin-proof");
        assert!(
            fs::rename(&media, &renamed_media).is_err(),
            "parent directory replacement must be denied"
        );
        player.stop();
        assert!(fs::OpenOptions::new().write(true).open(&source).is_ok());
        fs::rename(&media, &renamed_media).unwrap();
        fs::rename(&renamed_media, &media).unwrap();
        assert_eq!(
            reopened.governor().usage().unwrap(),
            crate::match_store::ResourceUsage::default()
        );
        let mut stale = request.clone();
        stale.track_revision = Some(tracks[0].revision + 1);
        assert!(FacialService::match_inspect_appearance(
            reopened.clone(),
            &stale,
            &coordinator,
            &AtomicBool::new(false)
        )
        .err()
        .unwrap()
        .contains("revision"));
        assert!(FacialService::match_inspect_appearance(
            reopened.clone(),
            &request,
            &coordinator,
            &AtomicBool::new(true)
        )
        .err()
        .unwrap()
        .contains("cancelled"));
        assert_eq!(
            reopened.video_media_tracks(&asset.media_key).unwrap(),
            tracks
        );
        assert_eq!(
            reopened.warm_projection(&asset.media_key).unwrap().unwrap(),
            projection
        );
        let original = fs::read(&source).unwrap();
        let mut changed = original.clone();
        changed.push(0);
        fs::write(&source, changed).unwrap();
        assert!(FacialService::match_inspect_appearance(
            reopened.clone(),
            &request,
            &coordinator,
            &AtomicBool::new(false)
        )
        .err()
        .unwrap()
        .contains("fingerprint changed"));
        fs::write(&source, original).unwrap();
        assert_eq!(
            reopened.governor().usage().unwrap(),
            crate::match_store::ResourceUsage::default()
        );
        assert_eq!(Sha256::digest(fs::read(&source).unwrap()), before);
    }
    drop(reopened);
    crate::surreal_store::wait_until_closed(&crate::media_db::MediaDb::db_path(&root)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match fs::remove_dir_all(&root) {
            Ok(()) => break,
            Err(error)
                if matches!(error.raw_os_error(), Some(32 | 33))
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(error) => panic!("owned pipeline fixture cleanup: {error}"),
        }
    }
}

#[path = "match_pipeline_cpu_two_tests.rs"]
mod production_cpu_two_tests;
