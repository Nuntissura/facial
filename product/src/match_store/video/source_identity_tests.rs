use super::*;
use crate::match_video::VideoDetection;
use crate::media_io::RootKind;

const GENERATION: &str = "copied-video-fixture";

fn frame(pts: i64) -> VideoFrame {
    VideoFrame {
        stream_index: 0,
        playback_origin: VideoTime::default(),
        time: VideoTime {
            pts,
            numerator: 1,
            denominator: 1000,
        },
        frame_sha256: format!("{:064x}", pts + 1),
        scene_score: 0.0,
        detections: vec![VideoDetection {
            source_index: 0,
            bounds: [0.1, 0.1, 0.3, 0.3],
            quality: 0.9,
            pose_bucket: "frontal".into(),
            detector_generation: GENERATION.into(),
        }],
    }
}

fn permit(store: &MatchStore, fence: &RevisionFence, stage: JobStage) -> MatchStagePermit {
    store
        .acquire_background_stage(
            &MediaIoCoordinator::new(),
            RootIdentity::new("copied-video-test", 1, RootKind::Local),
            fence,
            stage,
            ResourceRequest {
                worker_memory_bytes: 0,
                admitted_items: 1,
                queued_items: 1,
                queued_bytes: 1024 * 1024,
                cpu_inference: u64::from(stage == JobStage::Detect),
                decoded_bytes: 1024 * 1024,
                gpu_vram_bytes: 0,
                surreal_writes: 1,
                vector_index_builds: 0,
            },
        )
        .unwrap()
}

fn fence(store: &MatchStore, root: &Path, key: &str, fingerprint: &str) -> RevisionFence {
    let configured = store.configure_index_root(root, Vec::new()).unwrap();
    let queued = store.create_job(&configured.root_id, GENERATION).unwrap();
    let job = store
        .set_job_lifecycle(&queued.job_id, JobLifecycle::Running)
        .unwrap();
    let mut asset = store.enqueue_asset(&job.job_id, key, fingerprint).unwrap();
    asset.source_path = Some(root.join(key).to_string_lossy().into_owned());
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
        .unwrap();
    let fence = RevisionFence {
        job_id: job.job_id,
        media_key: key.into(),
        media_fingerprint: fingerprint.into(),
        schema_generation: job.schema_generation,
        model_generation: job.model_generation,
        identity_revision: job.identity_revision,
        catalog_revision: job.catalog_revision,
    };
    let permit = permit(store, &fence, JobStage::Discover);
    store
        .commit_asset_stage(&asset.asset_id, JobStage::Discover, &fence, &permit)
        .unwrap();
    fence
}

fn sample(store: &MatchStore, fence: &RevisionFence, pts: i64) -> Result<VideoCheckpoint, String> {
    sample_stream(store, fence, pts, 0)
}

fn sample_stream(
    store: &MatchStore,
    fence: &RevisionFence,
    pts: i64,
    stream: u32,
) -> Result<VideoCheckpoint, String> {
    let permit = permit(store, fence, JobStage::Detect);
    let mut frame = frame(pts);
    frame.stream_index = stream;
    store.commit_video_frame(
        fence,
        &permit,
        store.external_admission_epoch(),
        &VideoPolicy::default(),
        frame,
    )
}

fn index(store: &MatchStore, root: &Path, key: &str, fingerprint: &str) -> VideoTrackSnapshot {
    let fence = fence(store, root, key, fingerprint);
    for pts in [0, 500, 1000] {
        sample(store, &fence, pts).unwrap();
    }
    let permit = permit(store, &fence, JobStage::Detect);
    store
        .finalize_video_tracks(&fence, &permit, store.external_admission_epoch(), 0)
        .unwrap();
    store.video_media_tracks(key).unwrap().remove(0)
}

fn remove_checkpoint(store: &MatchStore, key: &str) {
    remove_stream_checkpoint(store, key, 0);
}

fn remove_stream_checkpoint(store: &MatchStore, key: &str, stream: u32) {
    let _guard = store
        .mutation_write_guard("checkpoint-less recovery fixture")
        .unwrap();
    store
        .commit_owned_unlocked(
            &[],
            &[(VIDEO_CHECKPOINT_TABLE.into(), format!("{key}:{stream}"))],
        )
        .unwrap();
}

fn rekey_state(store: &MatchStore) -> (Value, u64, u64, usize, bool, u64) {
    let execution = serde_json::to_value(store.execution_state_unlocked().unwrap()).unwrap();
    let operations = store.count(OPERATION_TABLE).unwrap();
    let caches = store.caches.read().unwrap();
    (
        execution,
        caches.identity_revision,
        caches.catalog_revision,
        caches.projections.len(),
        caches.autocomplete.valid,
        operations,
    )
}

fn warm_rekey_projection(store: &MatchStore, media: &str, fingerprint: &str) -> PeopleProjection {
    let execution = store.execution_state_unlocked().unwrap();
    let projection = PeopleProjection {
        media_key: media.into(),
        media_fingerprint: fingerprint.into(),
        schema_generation: MATCH_SCHEMA_GENERATION.into(),
        model_generation: GENERATION.into(),
        identity_revision: execution.identity_revision,
        catalog_revision: execution.catalog_revision,
        person_ids: Vec::new(),
        published_at: now(),
    };
    store
        .upsert_json(PROJECTION_TABLE, media, &projection)
        .unwrap();
    assert_eq!(
        store.warm_projection(media).unwrap(),
        Some(projection.clone())
    );
    projection
}

#[test]
fn wp086_checkpoint_encoded_bounds_precede_deserialization() {
    let mut row = StoredVideoCheckpoint {
        checkpoint_id: "bounded.mkv:0".into(),
        media_key: "bounded.mkv".into(),
        media_fingerprint: "a".repeat(64),
        policy_json: serde_json::to_string(&VideoPolicy::default()).unwrap(),
        payload: "[".repeat(CHECKPOINT_PAYLOAD_BYTES_LIMIT + 1),
    };
    assert!(row
        .tracker_for("bounded.mkv", 0)
        .err()
        .unwrap()
        .contains("encoded payload exceeds bound"));
    row.payload = "{".into();
    row.policy_json = "[".repeat(4097);
    assert!(row
        .tracker_for("bounded.mkv", 0)
        .err()
        .unwrap()
        .contains("encoded payload exceeds bound"));
}

#[test]
fn wp086_long_video_checkpoint_loss_pages_all_observations_and_rejects_late_conflict() {
    const SAMPLES: usize = 4097;
    let root = std::env::temp_dir().join(format!(
        "facial-long-video-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let media = "long.mkv";
    let fingerprint = "a".repeat(64);
    let policy = VideoPolicy::default();
    let mut tracker = VideoTracker::new(fingerprint.clone(), 0, policy.clone()).unwrap();
    let checkpoint = StoredVideoCheckpoint {
        checkpoint_id: format!("{media}:0"),
        media_key: media.into(),
        media_fingerprint: fingerprint.clone(),
        policy_json: serde_json::to_string(&policy).unwrap(),
        payload: serde_json::to_string(&tracker.checkpoint()).unwrap(),
    };
    store
        .upsert_json(
            VIDEO_CHECKPOINT_TABLE,
            &checkpoint.checkpoint_id,
            &checkpoint,
        )
        .unwrap();
    let original = index(&store, &root, media, &fingerprint);
    let template: FaceObservation = store
        .require(FACE_TABLE, &original.face_ids[0], "long-video Face")
        .unwrap();
    let mut observations = Vec::new();
    let mut closed = BTreeSet::new();
    let mut exemplars = BTreeSet::new();
    for sample in 0..SAMPLES {
        let update = tracker.ingest(frame(sample as i64 * 500)).unwrap();
        observations.extend(update.observations);
        for track in update.closed_tracks {
            assert!(track.observation_count <= policy.max_observations_per_track);
            closed.insert(track.last.observation_id);
            exemplars.extend(track.exemplars.into_iter().map(|row| row.observation_id));
        }
    }
    for track in tracker.finish() {
        assert!(track.observation_count <= policy.max_observations_per_track);
        closed.insert(track.last.observation_id);
        exemplars.extend(track.exemplars.into_iter().map(|row| row.observation_id));
    }
    let mut upserts = Vec::new();
    for (sample, observation) in observations.iter().enumerate() {
        let mut face = template.clone();
        face.face_id = format!("video-face-{}", observation.observation_id);
        face.source_index = sample as u32 * 128;
        let row = StoredVideoObservation {
            observation_id: observation.observation_id.clone(),
            face_id: face.face_id.clone(),
            track_id: observation.track_id.clone(),
            media_key: media.into(),
            media_fingerprint: fingerprint.clone(),
            revision: 1,
            closed: closed.contains(&observation.observation_id),
            exemplar: exemplars.contains(&observation.observation_id),
            payload: serde_json::to_string(observation).unwrap(),
        };
        row.observation().unwrap();
        upserts.push((
            FACE_TABLE.into(),
            face.face_id.clone(),
            serde_json::to_value(face).unwrap(),
        ));
        upserts.push((
            VIDEO_OBSERVATION_TABLE.into(),
            row.observation_id.clone(),
            serde_json::to_value(row).unwrap(),
        ));
    }
    {
        let _guard = store
            .mutation_write_guard("bounded long-video canonical fixture")
            .unwrap();
        for batch in upserts.chunks(256) {
            store.commit_owned_unlocked(batch, &[]).unwrap();
        }
    }
    assert_eq!(store.count(FACE_TABLE).unwrap(), SAMPLES as u64);
    remove_checkpoint(&store, media);
    let replay_fence = fence(&store, &root, media, &fingerprint);
    for pts in [0, 500, 1000] {
        assert!(sample(&store, &replay_fence, pts)
            .unwrap()
            .identity_namespace
            .is_none());
    }
    assert_eq!(store.count(FACE_TABLE).unwrap(), SAMPLES as u64);
    let replay = store.video_track_snapshot(&original.track_id).unwrap();
    assert_eq!(&replay.face_ids[..3], original.face_ids.as_slice());
    assert_eq!(
        replay.observation_count,
        policy.max_observations_per_track as usize
    );

    // Place valid conflicting evidence beyond sixteen cursor pages. A single
    // seed row or a truncated scan cannot establish the source namespace.
    let mut ids = observations
        .iter()
        .map(|row| row.observation_id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let divergent = (1u64..=256)
        .find_map(|seed| {
            let mut tracker = VideoTracker::new_for_source(
                fingerprint.clone(),
                0,
                policy.clone(),
                format!("{seed:064x}"),
            )
            .unwrap();
            let observation = tracker
                .ingest(frame(SAMPLES as i64 * 500))
                .unwrap()
                .observations
                .remove(0);
            (observation.observation_id.as_str() > ids[2048]).then_some(observation)
        })
        .unwrap();
    assert!(
        ids.iter()
            .filter(|id| **id < divergent.observation_id.as_str())
            .count()
            > 2048
    );
    let mut face = template;
    face.face_id = format!("video-face-{}", divergent.observation_id);
    face.source_index = SAMPLES as u32 * 128;
    let row = StoredVideoObservation {
        observation_id: divergent.observation_id.clone(),
        face_id: face.face_id.clone(),
        track_id: divergent.track_id.clone(),
        media_key: media.into(),
        media_fingerprint: fingerprint.clone(),
        revision: 1,
        closed: true,
        exemplar: true,
        payload: serde_json::to_string(&divergent).unwrap(),
    };
    store.upsert_json(FACE_TABLE, &face.face_id, &face).unwrap();
    store
        .upsert_json(VIDEO_OBSERVATION_TABLE, &row.observation_id, &row)
        .unwrap();
    remove_checkpoint(&store, media);
    assert!(sample(&store, &replay_fence, 0)
        .unwrap_err()
        .contains("inconsistent identity namespaces"));
    assert!(store.video_checkpoint(media, 0).unwrap().is_none());
    assert_eq!(store.count(FACE_TABLE).unwrap(), SAMPLES as u64 + 1);
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wp086_two_stream_checkpoint_loss_preserves_legacy_and_namespaced_faces() {
    let root = std::env::temp_dir().join(format!(
        "facial-two-stream-video-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let media = "two-stream.mkv";
    let fingerprint = "a".repeat(64);
    let policy = VideoPolicy::default();
    let checkpoint = StoredVideoCheckpoint {
        checkpoint_id: format!("{media}:0"),
        media_key: media.into(),
        media_fingerprint: fingerprint.clone(),
        policy_json: serde_json::to_string(&policy).unwrap(),
        payload: serde_json::to_string(
            &VideoTracker::new(fingerprint.clone(), 0, policy.clone())
                .unwrap()
                .checkpoint(),
        )
        .unwrap(),
    };
    store
        .upsert_json(
            VIDEO_CHECKPOINT_TABLE,
            &checkpoint.checkpoint_id,
            &checkpoint,
        )
        .unwrap();
    let legacy_track = index(&store, &root, media, &fingerprint);
    let template: FaceObservation = store
        .require(FACE_TABLE, &legacy_track.face_ids[0], "legacy Face")
        .unwrap();
    let mut second =
        VideoTracker::new_for_source(fingerprint.clone(), 1, policy.clone(), "b".repeat(64))
            .unwrap();
    let mut second_face_ids = Vec::new();
    for (index, pts) in [0, 500, 1000].into_iter().enumerate() {
        let mut next = frame(pts);
        next.stream_index = 1;
        let observation = second.ingest(next).unwrap().observations.remove(0);
        let mut face = template.clone();
        face.face_id = format!("video-face-{}", observation.observation_id);
        // Historical stream rows already occupy distinct media source slots.
        face.source_index = 1000 + index as u32;
        let row = StoredVideoObservation {
            observation_id: observation.observation_id.clone(),
            face_id: face.face_id.clone(),
            track_id: observation.track_id.clone(),
            media_key: media.into(),
            media_fingerprint: fingerprint.clone(),
            revision: 1,
            closed: index == 2,
            exemplar: index == 0,
            payload: serde_json::to_string(&observation).unwrap(),
        };
        row.observation().unwrap();
        store.upsert_json(FACE_TABLE, &face.face_id, &face).unwrap();
        store
            .upsert_json(VIDEO_OBSERVATION_TABLE, &row.observation_id, &row)
            .unwrap();
        second_face_ids.push(face.face_id);
    }
    let second_track_id = second.checkpoint().active[0].track_id.clone();
    remove_stream_checkpoint(&store, media, 0);
    remove_stream_checkpoint(&store, media, 1);
    let replay_fence = fence(&store, &root, media, &fingerprint);
    for stream in [0, 1] {
        for pts in [0, 500, 1000] {
            let checkpoint = sample_stream(&store, &replay_fence, pts, stream).unwrap();
            assert_eq!(
                checkpoint.identity_namespace,
                (stream == 1).then(|| "b".repeat(64))
            );
        }
        let permit = permit(&store, &replay_fence, JobStage::Detect);
        store
            .finalize_video_tracks(
                &replay_fence,
                &permit,
                store.external_admission_epoch(),
                stream,
            )
            .unwrap();
    }
    assert_eq!(store.count(FACE_TABLE).unwrap(), 6);
    assert_eq!(
        store
            .video_track_snapshot(&legacy_track.track_id)
            .unwrap()
            .face_ids,
        legacy_track.face_ids
    );
    assert_eq!(
        store
            .video_track_snapshot(&second_track_id)
            .unwrap()
            .face_ids,
        second_face_ids
    );
    store
        .export_identity_bundle(&root.join("two-stream-valid.json"))
        .unwrap();

    // Valid evidence with another seed in the same stream must fail closed.
    let mut divergent =
        VideoTracker::new_for_source(fingerprint.clone(), 1, policy, "c".repeat(64)).unwrap();
    let mut next = frame(1500);
    next.stream_index = 1;
    let observation = divergent.ingest(next).unwrap().observations.remove(0);
    let mut face = template;
    face.face_id = format!("video-face-{}", observation.observation_id);
    face.source_index = 3333;
    let row = StoredVideoObservation {
        observation_id: observation.observation_id.clone(),
        face_id: face.face_id.clone(),
        track_id: observation.track_id.clone(),
        media_key: media.into(),
        media_fingerprint: fingerprint.clone(),
        revision: 1,
        closed: true,
        exemplar: true,
        payload: serde_json::to_string(&observation).unwrap(),
    };
    store.upsert_json(FACE_TABLE, &face.face_id, &face).unwrap();
    store
        .upsert_json(VIDEO_OBSERVATION_TABLE, &row.observation_id, &row)
        .unwrap();
    let rejected_export = root.join("two-stream-inconsistent.json");
    assert!(store
        .export_identity_bundle(&rejected_export)
        .unwrap_err()
        .contains("inconsistent identity namespaces"));
    assert!(!rejected_export.exists());
    remove_stream_checkpoint(&store, media, 1);
    assert!(sample_stream(&store, &replay_fence, 0, 1)
        .unwrap_err()
        .contains("inconsistent identity namespaces"));
    assert!(store.video_checkpoint(media, 1).unwrap().is_none());
    assert_eq!(store.count(FACE_TABLE).unwrap(), 7);
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wp086_checkpoint_transplant_rejects_getter_finalizer_commit_and_rekey_without_mutation() {
    let root = std::env::temp_dir().join(format!(
        "facial-checkpoint-transplant-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let media = "transplant.mkv";
    let fingerprint = "a".repeat(64);
    let fence = fence(&store, &root, media, &fingerprint);
    let source_checkpoint = sample_stream(&store, &fence, 0, 1).unwrap();
    assert_eq!(source_checkpoint.stream_index, 1);
    let face_id = format!(
        "video-face-{}",
        source_checkpoint.active[0].first.observation_id
    );
    let row_id = source_checkpoint.active[0].first.observation_id.clone();
    let before_face: FaceObservation = store
        .require(FACE_TABLE, &face_id, "stream-one Face")
        .unwrap();
    let before_row: StoredVideoObservation = store
        .require(VIDEO_OBSERVATION_TABLE, &row_id, "stream-one observation")
        .unwrap();
    let mut transplanted: StoredVideoCheckpoint = store
        .require(
            VIDEO_CHECKPOINT_TABLE,
            &format!("{media}:1"),
            "stream-one checkpoint",
        )
        .unwrap();
    let warmed = warm_rekey_projection(&store, media, &fingerprint);
    transplanted.checkpoint_id = format!("{media}:0");
    store
        .upsert_json(
            VIDEO_CHECKPOINT_TABLE,
            &transplanted.checkpoint_id,
            &transplanted,
        )
        .unwrap();
    let before_checkpoint = serde_json::to_value(&transplanted).unwrap();
    assert!(store
        .video_checkpoint(media, 0)
        .unwrap_err()
        .contains("source or stream mismatch"));
    let permit = permit(&store, &fence, JobStage::Detect);
    assert!(store
        .finalize_video_tracks(&fence, &permit, store.external_admission_epoch(), 0)
        .unwrap_err()
        .contains("source or stream mismatch"));
    drop(permit);
    assert!(sample_stream(&store, &fence, 500, 0)
        .unwrap_err()
        .contains("source or stream mismatch"));
    let before_rekey = rekey_state(&store);
    assert!(store
        .rekey_media(media, "rejected-move.mkv", &fingerprint)
        .unwrap_err()
        .contains("source or stream mismatch"));
    assert_eq!(rekey_state(&store), before_rekey);
    assert_eq!(store.cached_projection(media).unwrap(), Some(warmed));
    assert!(store
        .video_checkpoint("rejected-move.mkv", 1)
        .unwrap()
        .is_none());
    let after_checkpoint: StoredVideoCheckpoint = store
        .require(
            VIDEO_CHECKPOINT_TABLE,
            &transplanted.checkpoint_id,
            "rejected transplanted checkpoint",
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(after_checkpoint).unwrap(),
        before_checkpoint
    );
    assert_eq!(
        store
            .require::<FaceObservation>(FACE_TABLE, &face_id, "unmodified Face")
            .unwrap(),
        before_face
    );
    assert_eq!(
        store
            .require::<StoredVideoObservation>(
                VIDEO_OBSERVATION_TABLE,
                &row_id,
                "unmodified observation"
            )
            .unwrap(),
        before_row
    );
    assert_eq!(
        store.video_checkpoint(media, 1).unwrap(),
        Some(source_checkpoint)
    );
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wp086_identical_video_copies_restart_split_undo_rekey_and_recovery() {
    let root = std::env::temp_dir().join(format!(
        "facial-copied-video-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(root.join("clips")).unwrap();
    let bytes = b"identical-video-source-fixture";
    let fingerprint = format!("{:x}", Sha256::digest(bytes));
    for key in ["clips/a.mkv", "clips/b.mkv", "clips/moved.mkv"] {
        std::fs::write(root.join(key), bytes).unwrap();
    }
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let person = store
        .create_person("Copied Track Person", Vec::new())
        .unwrap();
    let a = index(&store, &root, "clips/a.mkv", &fingerprint);
    let b = index(&store, &root, "clips/b.mkv", &fingerprint);
    assert_ne!(a.track_id, b.track_id);
    assert!(a.face_ids.iter().all(|id| !b.face_ids.contains(id)));
    assert_eq!(a.timestamps, b.timestamps);
    assert_eq!(a.observation_count, 3);
    assert_eq!(
        store.video_density_family_unlocked(&a.track_id).unwrap(),
        store.video_density_family_unlocked(&b.track_id).unwrap()
    );
    for track in [&a, &b] {
        for id in &track.face_ids {
            let face: FaceObservation = store.require(FACE_TABLE, id, "copied-video Face").unwrap();
            assert_eq!(face.media_key, track.media_key);
            assert_eq!(face.media_fingerprint, fingerprint);
        }
    }
    let assign = store
        .preview_video_track_correction(
            &a.track_id,
            TrackCorrectionAction::Assign,
            Some(&person.person_id),
        )
        .unwrap();
    let assigned = store.apply_video_track_correction(&assign).unwrap();
    let split = store
        .preview_video_track_split(&a.track_id, &[a.observations[1].observation_id.clone()])
        .unwrap();
    let split_receipt = store.apply_video_track_split(&split).unwrap();
    assert_eq!(store.video_media_tracks("clips/a.mkv").unwrap().len(), 2);
    assert_eq!(store.video_track_snapshot(&b.track_id).unwrap(), b);
    assert_ne!(
        store.video_density_family_unlocked(&a.track_id).unwrap(),
        store
            .video_density_family_unlocked(&split.new_track_id)
            .unwrap()
    );
    for id in &b.face_ids {
        assert!(store
            .get_one::<Assignment>(ASSIGNMENT_TABLE, id)
            .unwrap()
            .is_none());
    }
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    let store = MatchStore::open(&root).unwrap();
    store.undo_correction(&split_receipt.operation_id).unwrap();
    store.undo_correction(&assigned.operation_id).unwrap();
    assert_eq!(store.video_track_snapshot(&b.track_id).unwrap(), b);
    assert_eq!(
        store.video_track_snapshot(&a.track_id).unwrap().face_ids,
        a.face_ids
    );
    let pre_move_path = root.join("copied-video-before-move.json");
    store.export_identity_bundle(&pre_move_path).unwrap();
    let history_ids = [
        split_receipt.operation_id.clone(),
        format!("undo-{}", split_receipt.operation_id),
    ];
    let before_history = history_ids
        .iter()
        .map(|id| {
            let operation: MatchOperation = store
                .require(OPERATION_TABLE, id, "pre-move video history")
                .unwrap();
            serde_json::from_str::<super::super::corrections::CorrectionDeltaEnvelope>(
                &operation.after_json,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store
            .rekey_media("clips/a.mkv", "clips/moved.mkv", &fingerprint)
            .unwrap(),
        3
    );
    for (id, original_history) in history_ids.iter().zip(&before_history) {
        let operation: MatchOperation = store
            .require(OPERATION_TABLE, id, "moved video history")
            .unwrap();
        let moved_history: super::super::corrections::CorrectionDeltaEnvelope =
            serde_json::from_str(&operation.after_json).unwrap();
        for (original_row, moved_row) in original_history.rows.iter().zip(&moved_history.rows) {
            if original_row.table != CorrectionTable::VideoObservation {
                continue;
            }
            assert_eq!(original_row.stable_id, moved_row.stable_id);
            for (original, moved) in [
                (&original_row.before, &moved_row.before),
                (&original_row.after, &moved_row.after),
            ] {
                let (Some(original), Some(moved)) = (original, moved) else {
                    continue;
                };
                let mut original: StoredVideoObservation =
                    serde_json::from_value(original.clone()).unwrap();
                let moved: StoredVideoObservation = serde_json::from_value(moved.clone()).unwrap();
                original.media_key = "clips/moved.mkv".into();
                assert_eq!(moved, original, "rekey must translate every video journal snapshot without replacing its evidence");
            }
        }
    }
    let post_move_path = root.join("copied-video-after-move.json");
    store.export_identity_bundle(&post_move_path).unwrap();
    let moved = store.video_track_snapshot(&a.track_id).unwrap();
    assert_eq!(moved.media_key, "clips/moved.mkv");
    assert_eq!(moved.face_ids, a.face_ids);
    let reused = index(&store, &root, "clips/a.mkv", &fingerprint);
    assert_ne!(
        reused.track_id, moved.track_id,
        "reusing a moved source path must allocate a new source identity"
    );
    assert!(reused
        .face_ids
        .iter()
        .all(|id| !moved.face_ids.contains(id)));

    // Exercise the real recovery bundle and canonical restore, which excludes checkpoints.
    let recovery_path = root.join("copied-video-recovery.json");
    let before_path = root.join("copied-video-before.json");
    store.export_identity_bundle(&before_path).unwrap();
    let before = MatchStore::read_identity_bundle(&before_path)
        .unwrap()
        .graph;
    let preview = store.preview_clear_all_match_data(&recovery_path).unwrap();
    let exported = MatchStore::read_identity_bundle(&recovery_path).unwrap();
    assert_eq!(exported.graph.video_observations, before.video_observations);
    let clear = store.clear_all_match_data(&preview).unwrap();
    let relocations = before
        .roots
        .iter()
        .map(|indexed_root| {
            (
                indexed_root.root_id.clone(),
                root.to_string_lossy().into_owned(),
            )
        })
        .collect();
    store
        .restore_clear_recovery_bundle(
            &recovery_path,
            &relocations,
            &clear.recovery_bundle.restore_token,
        )
        .unwrap();
    let after_path = root.join("copied-video-after.json");
    store.export_identity_bundle(&after_path).unwrap();
    assert_eq!(
        MatchStore::read_identity_bundle(&after_path)
            .unwrap()
            .graph
            .video_observations,
        before.video_observations
    );
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    assert!(store
        .video_checkpoint("clips/moved.mkv", 0)
        .unwrap()
        .is_none());
    let replay = index(&store, &root, "clips/moved.mkv", &fingerprint);
    assert_eq!(replay.track_id, moved.track_id);
    assert_eq!(replay.face_ids, moved.face_ids);
    assert_eq!(store.video_track_snapshot(&b.track_id).unwrap(), b);
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wp086_legacy_video_ids_survive_rekey_checkpoint_loss_and_tampering_rejects() {
    let root = std::env::temp_dir().join(format!(
        "facial-legacy-video-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let fingerprint = "a".repeat(64);
    let store = MatchStore::open(&root).unwrap();
    store.register_model_generation(GENERATION, true).unwrap();
    store.activate_model_generation(GENERATION).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let person = store
        .create_person("Legacy Track Person", Vec::new())
        .unwrap();
    let policy = VideoPolicy::default();
    let legacy = VideoTracker::new(fingerprint.clone(), 0, policy.clone())
        .unwrap()
        .checkpoint();
    let checkpoint = StoredVideoCheckpoint {
        checkpoint_id: "old.mkv:0".into(),
        media_key: "old.mkv".into(),
        media_fingerprint: fingerprint.clone(),
        policy_json: serde_json::to_string(&policy).unwrap(),
        payload: serde_json::to_string(&legacy).unwrap(),
    };
    assert!(!checkpoint.payload.contains("identity_namespace"));
    store
        .upsert_json(
            VIDEO_CHECKPOINT_TABLE,
            &checkpoint.checkpoint_id,
            &checkpoint,
        )
        .unwrap();
    let original = index(&store, &root, "old.mkv", &fingerprint);
    let assigned = store
        .apply_video_track_correction(
            &store
                .preview_video_track_correction(
                    &original.track_id,
                    TrackCorrectionAction::Assign,
                    Some(&person.person_id),
                )
                .unwrap(),
        )
        .unwrap();
    store
        .rekey_media("old.mkv", "new.mkv", &fingerprint)
        .unwrap();
    remove_checkpoint(&store, "new.mkv");
    let rebuilt = store.preview_rebuild_match_analysis().unwrap();
    store.rebuild_match_analysis(&rebuilt).unwrap();
    let replay = index(&store, &root, "new.mkv", &fingerprint);
    assert_eq!(replay.face_ids, original.face_ids);
    assert_eq!(replay.track_id, original.track_id);
    for id in &original.face_ids {
        let assignment: Assignment = store
            .require(ASSIGNMENT_TABLE, id, "preserved legacy assignment")
            .unwrap();
        assert_eq!(assignment.operation_id, assigned.operation_id);
    }
    let id = &original.observations[0].observation_id;
    let mut stored: StoredVideoObservation = store
        .require(VIDEO_OBSERVATION_TABLE, id, "legacy observation")
        .unwrap();
    let warmed = warm_rekey_projection(&store, "new.mkv", &fingerprint);
    assert!(stored.observation().unwrap().identity_namespace.is_none());
    let mut tampered = stored.observation().unwrap();
    tampered.identity_namespace = Some("f".repeat(64));
    stored.payload = serde_json::to_string(&tampered).unwrap();
    assert!(stored.observation().is_err());
    store
        .upsert_json(VIDEO_OBSERVATION_TABLE, id, &stored)
        .unwrap();
    remove_checkpoint(&store, "new.mkv");
    let fence = fence(&store, &root, "new.mkv", &fingerprint);
    assert!(sample(&store, &fence, 0).is_err());
    let before_faces = store.list::<FaceObservation>(FACE_TABLE).unwrap();
    let before_rekey = rekey_state(&store);
    assert!(store
        .rekey_media("new.mkv", "rejected-move.mkv", &fingerprint)
        .is_err());
    assert_eq!(rekey_state(&store), before_rekey);
    assert_eq!(store.cached_projection("new.mkv").unwrap(), Some(warmed));
    assert_eq!(
        store.list::<FaceObservation>(FACE_TABLE).unwrap(),
        before_faces
    );
    assert_eq!(
        store
            .require::<StoredVideoObservation>(
                VIDEO_OBSERVATION_TABLE,
                id,
                "unmodified invalid observation"
            )
            .unwrap(),
        stored
    );
    assert!(store.video_checkpoint("new.mkv", 0).unwrap().is_none());
    assert_eq!(
        store.count(FACE_TABLE).unwrap(),
        original.face_ids.len() as u64
    );
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
