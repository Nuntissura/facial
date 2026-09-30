//! Synthetic canonical-store boundary proof; not a raw-media inference claim.
use super::*;
use crate::match_video::{VideoDetection, VideoFrame, VideoPolicy, VideoTime, VideoTracker};

fn truth(store: &MatchStore) -> Vec<Vec<Value>> {
    [
        FACE_TABLE,
        EMBEDDING_TABLE,
        ASSIGNMENT_TABLE,
        CONSTRAINT_TABLE,
        PERSON_TABLE,
        TRUSTED_MEMBER_TABLE,
        TRUSTED_SEARCH_TABLE,
    ]
    .into_iter()
    .map(|table| {
        let mut rows = store.list::<Value>(table).unwrap();
        rows.sort_by_key(Value::to_string);
        rows
    })
    .collect()
}
#[test]
fn wp086_explicit_review_uses_canonical_track_density_and_never_assigns() {
    let root = std::env::temp_dir().join(format!(
        "facial-wp086-cluster-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = MatchStore::open(&root).unwrap();
    let generation = "review-model";
    store.register_model_generation(generation, true).unwrap();
    store.activate_model_generation(generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let job = store.create_job("fixture-root", generation).unwrap();
    let job = store
        .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
        .unwrap();
    let media_key = "fixture/clip.mp4";
    let hash = "a".repeat(64);
    store.enqueue_asset(&job.job_id, media_key, &hash).unwrap();
    let mut tracker = VideoTracker::new(hash.clone(), 0, VideoPolicy::default()).unwrap();
    let mut observations = Vec::new();
    for (index, pose) in ["frontal", "profile", "frontal"].into_iter().enumerate() {
        observations.extend(
            tracker
                .ingest(VideoFrame {
                    stream_index: 0,
                    playback_origin: VideoTime::default(),
                    time: VideoTime {
                        pts: index as i64 * 500,
                        numerator: 1,
                        denominator: 1000,
                    },
                    frame_sha256: format!("{:064x}", index + 1),
                    scene_score: if index == 2 { 1.0 } else { 0.0 },
                    detections: vec![VideoDetection {
                        source_index: 0,
                        bounds: [0.1, 0.1, 0.3, 0.3],
                        quality: 0.9,
                        pose_bucket: pose.into(),
                        detector_generation: "detector".into(),
                    }],
                })
                .unwrap()
                .observations,
        );
    }
    let mut ids = Vec::new();
    for (index, observation) in observations.iter().enumerate() {
        let id = format!("video-face-{}", observation.observation_id);
        ids.push(id.clone());
        let face = FaceObservation {
            face_id: id.clone(),
            media_key: media_key.into(),
            media_fingerprint: hash.clone(),
            source_index: index as u32,
            source_width: None,
            source_height: None,
            exif_orientation: None,
            bounds_normalized: observation.detection.bounds.to_vec(),
            landmarks_normalized: Vec::new(),
            alignment_valid: true,
            quality: 0.9,
            pose_bucket: observation.detection.pose_bucket.clone(),
            operator_owned: false,
            schema_generation: MATCH_SCHEMA_GENERATION.into(),
            face_revision: 1,
            created_at: now(),
            updated_at: now(),
        };
        let mut vector = vec![0.0; 512];
        vector[0] = 1.0;
        let embedding = FaceEmbedding {
            embedding_id: embedding_id(&id, generation),
            face_id: id.clone(),
            vector,
            model_generation: generation.into(),
            schema_generation: MATCH_SCHEMA_GENERATION.into(),
            media_fingerprint: hash.clone(),
            face_revision: 1,
            job_id: job.job_id.clone(),
            active: true,
            created_at: now(),
        };
        let row = StoredVideoObservation {
            observation_id: observation.observation_id.clone(),
            face_id: id.clone(),
            track_id: observation.track_id.clone(),
            media_key: media_key.into(),
            media_fingerprint: hash.clone(),
            revision: 1,
            closed: index > 0,
            exemplar: true,
            payload: serde_json::to_string(observation).unwrap(),
        };
        store.upsert_json(FACE_TABLE, &id, &face).unwrap();
        store
            .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
            .unwrap();
        store
            .upsert_json(video::VIDEO_OBSERVATION_TABLE, &row.observation_id, &row)
            .unwrap();
    }
    let mut request = UnnamedClusterReviewRequest {
        face_ids: ids[..2].to_vec(),
        model_generation: generation.into(),
        similarity_threshold: 0.8,
        minimum_quality: 0.7,
        minimum_independent_families: 1,
    };
    let before = truth(&store);
    let one = store.review_unnamed_clusters(&request).unwrap();
    assert!(one
        .rows
        .iter()
        .all(|r| r.cluster_id.is_some() && r.independent_families == 1));
    assert_eq!(
        one.rows[0].duplicate_family_id,
        one.rows[1].duplicate_family_id
    );
    request.minimum_independent_families = 2;
    let insufficient = store.review_unnamed_clusters(&request).unwrap();
    assert!(insufficient.rows.iter().all(|r| r.cluster_id.is_none()
        && r.exclusion_reason.as_deref() == Some("insufficient_independent_families")));
    request.face_ids = ids.clone();
    let grouped = store.review_unnamed_clusters(&request).unwrap();
    assert!(grouped
        .rows
        .iter()
        .all(|r| r.independent_families == 2 && r.cluster_id == grouped.rows[0].cluster_id));
    assert_eq!(before, truth(&store));
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    let store = MatchStore::open(&root).unwrap();
    assert_eq!(grouped, store.review_unnamed_clusters(&request).unwrap());
    let person = store.create_person("Excluded Person", Vec::new()).unwrap();
    let assignment = Assignment {
        assignment_id: ids[0].clone(),
        face_id: ids[0].clone(),
        person_id: person.person_id.clone(),
        media_key: media_key.into(),
        look_id: None,
        placement: "unsorted".into(),
        state: "operator_confirmed".into(),
        provenance: "fixture".into(),
        locked: true,
        model_generation: None,
        calibration_generation: None,
        envelope_hash: None,
        face_revision: 1,
        person_revision: person.revision,
        operation_id: "fixture-operation".into(),
        created_at: now(),
        updated_at: now(),
    };
    store
        .upsert_json(ASSIGNMENT_TABLE, &assignment.assignment_id, &assignment)
        .unwrap();
    let constraint = CannotLinkConstraint {
        constraint_id: cannot_link_id(&ids[2], &person.person_id),
        face_id: ids[2].clone(),
        person_id: person.person_id,
        operation_id: "fixture-operation".into(),
        operator_owned: true,
        created_at: now(),
    };
    store
        .upsert_json(CONSTRAINT_TABLE, &constraint.constraint_id, &constraint)
        .unwrap();
    let before = truth(&store);
    let excluded = store.review_unnamed_clusters(&request).unwrap();
    assert_eq!(
        excluded
            .rows
            .iter()
            .find(|r| r.face_id == ids[0])
            .unwrap()
            .exclusion_reason
            .as_deref(),
        Some("already_assigned")
    );
    assert_eq!(
        excluded
            .rows
            .iter()
            .find(|r| r.face_id == ids[2])
            .unwrap()
            .exclusion_reason
            .as_deref(),
        Some("person_cannot_link")
    );
    assert!(excluded.rows.iter().all(|r| r.cluster_id.is_none()));
    assert_eq!(before, truth(&store));
    let mut embedding: FaceEmbedding = store
        .require(
            EMBEDDING_TABLE,
            &embedding_id(&ids[1], generation),
            "embedding",
        )
        .unwrap();
    embedding.face_revision += 1;
    store
        .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
        .unwrap();
    assert_eq!(
        store
            .review_unnamed_clusters(&request)
            .unwrap()
            .rows
            .iter()
            .find(|r| r.face_id == ids[1])
            .unwrap()
            .exclusion_reason
            .as_deref(),
        Some("stale_embedding_or_source")
    );
    request.similarity_threshold = f32::NAN;
    assert!(store.review_unnamed_clusters(&request).is_err());
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
