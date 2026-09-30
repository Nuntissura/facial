//! Synthetic storage-boundary proof; these rows do not claim raw-media inference.
use super::*;

fn identity_rows(store: &MatchStore) -> Vec<Value> {
    [
        PERSON_TABLE,
        FACE_TABLE,
        EMBEDDING_TABLE,
        SUGGESTION_TABLE,
        ASSIGNMENT_TABLE,
        CONSTRAINT_TABLE,
        LOOK_TABLE,
        TEMPLATE_SET_TABLE,
        TRUSTED_MEMBER_TABLE,
        TRUSTED_SEARCH_TABLE,
    ]
    .into_iter()
    .map(|table| {
        let mut rows = store.list::<Value>(table).unwrap();
        rows.sort_by_key(Value::to_string);
        serde_json::json!({"table":table,"rows":rows})
    })
    .collect()
}

#[test]
fn context_review_persists_actual_relative_names_and_ablation_preserves_identity() {
    let root = std::env::temp_dir().join(format!(
        "facial-context-source-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let media = root.join("media");
    let folder = media.join("Alice");
    std::fs::create_dir_all(&folder).unwrap();
    let source = folder.join("Alice-video.mkv");
    std::fs::write(&source, b"synthetic source metadata fixture").unwrap();
    let store = MatchStore::open(&root).unwrap();
    let person = store.create_person("Alice", Vec::new()).unwrap();
    let outsider = store.create_person("Bob", Vec::new()).unwrap();
    let generation = "context-model";
    store.register_model_generation(generation, true).unwrap();
    store.activate_model_generation(generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let configured = store.configure_index_root(&media, Vec::new()).unwrap();
    let job = store
        .start_index_job(&configured.root_id, generation)
        .unwrap();
    let job = store
        .set_job_lifecycle(&job.job_id, JobLifecycle::Running)
        .unwrap();
    // Mirror the real opaque key shape, so using its digest as a filename fails.
    let media_key = format!("{}/{}.mkv", configured.root_id, "b".repeat(64));
    let hash = "a".repeat(64);
    let mut asset = store.enqueue_asset(&job.job_id, &media_key, &hash).unwrap();
    asset.source_path = Some(
        source
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
        .unwrap();
    let face = FaceObservation {
        face_id: derived_face_id(&media_key, &hash, 0, MATCH_SCHEMA_GENERATION),
        media_key: media_key.clone(),
        media_fingerprint: hash.clone(),
        source_index: 0,
        source_width: Some(100),
        source_height: Some(100),
        exif_orientation: Some(1),
        bounds_normalized: vec![0.1, 0.1, 0.5, 0.5],
        landmarks_normalized: vec![vec![0.2, 0.2]; 5],
        alignment_valid: true,
        quality: 0.9,
        pose_bucket: "frontal".into(),
        operator_owned: false,
        schema_generation: MATCH_SCHEMA_GENERATION.into(),
        face_revision: 1,
        created_at: now(),
        updated_at: now(),
    };
    store.upsert_json(FACE_TABLE, &face.face_id, &face).unwrap();
    let mut vector = vec![0.0; 512];
    vector[0] = 1.0;
    let embedding = FaceEmbedding {
        embedding_id: embedding_id(&face.face_id, generation),
        face_id: face.face_id.clone(),
        vector,
        model_generation: generation.into(),
        schema_generation: MATCH_SCHEMA_GENERATION.into(),
        media_fingerprint: hash.clone(),
        face_revision: 1,
        job_id: job.job_id.clone(),
        active: true,
        created_at: now(),
    };
    store
        .upsert_json(EMBEDDING_TABLE, &embedding.embedding_id, &embedding)
        .unwrap();
    let suggestion = Suggestion {
        suggestion_id: suggestion_id(&face.face_id, &person.person_id),
        face_id: face.face_id.clone(),
        candidate_person_id: person.person_id.clone(),
        similarity: 0.8,
        model_generation: generation.into(),
        calibration_generation: None,
        envelope_hash: None,
        media_fingerprint: hash.clone(),
        face_revision: 1,
        person_revision: person.revision,
        job_id: job.job_id.clone(),
        created_at: now(),
    };
    store
        .upsert_json(SUGGESTION_TABLE, &suggestion.suggestion_id, &suggestion)
        .unwrap();
    let assignment = Assignment {
        assignment_id: "assignment-sentinel".into(),
        face_id: face.face_id.clone(),
        person_id: person.person_id.clone(),
        media_key: media_key.clone(),
        look_id: None,
        placement: "catalog".into(),
        state: "operator_confirmed".into(),
        provenance: "operator".into(),
        locked: true,
        model_generation: None,
        calibration_generation: None,
        envelope_hash: None,
        face_revision: 1,
        person_revision: person.revision,
        operation_id: "operator-sentinel".into(),
        created_at: now(),
        updated_at: now(),
    };
    store
        .upsert_json(ASSIGNMENT_TABLE, &assignment.assignment_id, &assignment)
        .unwrap();
    let constraint = CannotLinkConstraint {
        constraint_id: "constraint-sentinel".into(),
        face_id: face.face_id.clone(),
        person_id: outsider.person_id.clone(),
        operation_id: "operator-sentinel".into(),
        operator_owned: true,
        created_at: now(),
    };
    store
        .upsert_json(CONSTRAINT_TABLE, &constraint.constraint_id, &constraint)
        .unwrap();
    let look = Look {
        look_id: "look-sentinel".into(),
        person_id: person.person_id.clone(),
        name: "fixture look".into(),
        revision: 1,
        created_at: now(),
        updated_at: now(),
    };
    store.upsert_json(LOOK_TABLE, &look.look_id, &look).unwrap();
    let set = TrustedTemplateSet {
        set_id: "set-sentinel".into(),
        look_id: look.look_id.clone(),
        name: "fixture set".into(),
        revision: 1,
        created_at: now(),
        updated_at: now(),
    };
    store
        .upsert_json(TEMPLATE_SET_TABLE, &set.set_id, &set)
        .unwrap();
    let member = TrustedTemplateMembership {
        membership_id: "member-sentinel".into(),
        set_id: set.set_id.clone(),
        look_id: look.look_id.clone(),
        face_id: face.face_id.clone(),
        authorized: true,
        alignment_valid: true,
        quality_passed: true,
        pose_passed: true,
        diversity_passed: true,
        provenance: "operator".into(),
        model_generation: generation.into(),
        embedding_id: embedding.embedding_id.clone(),
        media_fingerprint: hash,
        face_revision: 1,
        quality_score: 0.9,
        quality_threshold: 0.5,
        pose_bucket: "frontal".into(),
        policy_version: "fixture".into(),
        operation_id: "operator-sentinel".into(),
        created_at: now(),
    };
    store
        .upsert_json(TRUSTED_MEMBER_TABLE, &member.membership_id, &member)
        .unwrap();

    let before = identity_rows(&store);
    let ranked = store.context_review(&face.face_id, true).unwrap();
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].person_id, person.person_id);
    assert_eq!(ranked[0].visual_similarity, 0.8);
    assert_eq!(ranked[0].context.len(), 2);
    assert!(ranked[0].context.iter().any(|e| e.source
        == ContextSource::Folder {
            path: "Alice".into()
        }));
    assert!(ranked[0].context.iter().any(|e| e.source
        == ContextSource::Filename {
            name: "Alice-video.mkv".into()
        }));
    let persisted = store.list::<StoredReviewContext>(CONTEXT_TABLE).unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        serde_json::from_str::<Vec<ContextEvidence>>(&persisted[0].payload).unwrap(),
        ranked[0].context
    );
    assert_eq!(identity_rows(&store), before);
    let disabled = store.context_review(&face.face_id, false).unwrap();
    assert_eq!(disabled, ranked);
    assert_eq!(identity_rows(&store), before);

    // Context for an existing Person without current visual evidence cannot inject it.
    let injected = context_evidence(
        &face,
        &outsider,
        ContextSource::Album {
            album_id: "operator-album".into(),
        },
    )
    .unwrap();
    store
        .set_context_evidence(&face.face_id, &outsider.person_id, vec![injected])
        .unwrap_err();
    assert_eq!(
        store
            .context_review(&face.face_id, true)
            .unwrap()
            .iter()
            .map(|c| c.person_id.clone())
            .collect::<Vec<_>>(),
        vec![person.person_id.clone()]
    );
    assert_eq!(identity_rows(&store), before);
    let mut moved_asset = asset.clone();
    moved_asset.source_path = Some(
        root.join("outside-root-Alice.mkv")
            .to_string_lossy()
            .into_owned(),
    );
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &moved_asset)
        .unwrap();
    let outside = store.context_review(&face.face_id, true).unwrap();
    assert_eq!(outside.len(), 1);
    assert!(
        outside[0].context.is_empty(),
        "outside-root paths cannot supply folder/name evidence"
    );
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
        .unwrap();
    let mut changed_person = person.clone();
    changed_person.revision += 1;
    store
        .upsert_json(PERSON_TABLE, &person.person_id, &changed_person)
        .unwrap();
    assert!(store
        .set_context_evidence(&face.face_id, &person.person_id, ranked[0].context.clone())
        .is_err());
    assert!(
        store
            .context_review(&face.face_id, true)
            .unwrap()
            .is_empty(),
        "stale visual Person revision excluded"
    );
    store
        .upsert_json(PERSON_TABLE, &person.person_id, &person)
        .unwrap();
    let mut changed_face = face.clone();
    changed_face.face_revision += 1;
    store
        .upsert_json(FACE_TABLE, &face.face_id, &changed_face)
        .unwrap();
    assert!(store
        .set_context_evidence(&face.face_id, &person.person_id, ranked[0].context.clone())
        .is_err());
    assert!(
        store
            .context_review(&face.face_id, true)
            .unwrap()
            .is_empty(),
        "stale visual Face revision excluded"
    );
    // Canonical Time/Album acquisition: exact current reference, CAS tombstone,
    // ablation, assignment revision and fingerprint invalidation.
    store.upsert_json(FACE_TABLE, &face.face_id, &face).unwrap();
    let reference_key = format!("{}/{}.mkv", configured.root_id, "d".repeat(64));
    let reference_hash = "e".repeat(64);
    store
        .enqueue_asset(&job.job_id, &reference_key, &reference_hash)
        .unwrap();
    let mut reference_face = face.clone();
    reference_face.face_id =
        derived_face_id(&reference_key, &reference_hash, 0, MATCH_SCHEMA_GENERATION);
    reference_face.media_key = reference_key.clone();
    reference_face.media_fingerprint = reference_hash.clone();
    store
        .upsert_json(FACE_TABLE, &reference_face.face_id, &reference_face)
        .unwrap();
    let mut reference_assignment = assignment.clone();
    reference_assignment.assignment_id = "context-reference".into();
    reference_assignment.face_id = reference_face.face_id.clone();
    reference_assignment.media_key = reference_key.clone();
    store
        .upsert_json(
            ASSIGNMENT_TABLE,
            &reference_assignment.assignment_id,
            &reference_assignment,
        )
        .unwrap();
    let mut request = MediaContextRequest {
        media_key: face.media_key.clone(),
        media_fingerprint: face.media_fingerprint.clone(),
        expected_revision: 0,
        capture_unix_millis: Some(1000),
        time_window_millis: Some(10),
        album_ids: vec!["album-1".into()],
    };
    let mut prefixed_asset = asset.clone();
    prefixed_asset.media_fingerprint = format!("sha256:{}", face.media_fingerprint);
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &prefixed_asset)
        .unwrap();
    store.replace_media_context(&request).unwrap();
    store
        .upsert_json(JOB_ASSET_TABLE, &asset.asset_id, &asset)
        .unwrap();
    let mut reference_request = MediaContextRequest {
        media_key: reference_key,
        media_fingerprint: reference_hash,
        expected_revision: 0,
        capture_unix_millis: Some(1005),
        time_window_millis: None,
        album_ids: vec!["album-1".into()],
    };
    store.replace_media_context(&reference_request).unwrap();
    assert!(store.replace_media_context(&request).is_err());
    let identity_before = identity_rows(&store);
    let with_context = store.context_review(&face.face_id, true).unwrap();
    let canonical: Vec<_> = with_context[0]
        .context
        .iter()
        .filter(|e| {
            matches!(
                e.source,
                ContextSource::Time { .. } | ContextSource::Album { .. }
            )
        })
        .cloned()
        .collect();
    assert_eq!(canonical.len(), 2);
    let mut exhausted_budget = 0;
    assert!(store
        .canonical_context_evidence_bounded_unlocked(&face, &person, &mut exhausted_budget)
        .unwrap_err()
        .contains("reference bound exceeded"));
    assert_eq!(exhausted_budget, 0);
    let db = store.store.db();
    let person_id = person.person_id.clone();
    let plan:Vec<Value>=surreal_store::run(async move {
        let mut result=db.query("SELECT * OMIT id FROM match_assignment WITH INDEX match_assignment_person WHERE person_id=$person LIMIT 2 EXPLAIN;").bind(("person",person_id)).await.map_err(|e|e.to_string())?.check().map_err(|e|e.to_string())?;
        result.take(0).map_err(|e|e.to_string())
    }).unwrap();
    let plan = serde_json::to_string(&plan).unwrap();
    assert!(
        plan.contains("match_assignment_person"),
        "context reference query must use Person index: {plan}"
    );
    assert!(
        !plan.contains("Iterate Table"),
        "context reference query must not scan catalog: {plan}"
    );
    store.context_review(&face.face_id, false).unwrap();
    assert_eq!(identity_rows(&store), identity_before);
    reference_request.expected_revision = 1;
    reference_request.capture_unix_millis = None;
    reference_request.album_ids.clear();
    assert_eq!(
        store
            .replace_media_context(&reference_request)
            .unwrap()
            .revision,
        2
    );
    assert!(store
        .set_context_evidence(&face.face_id, &person.person_id, canonical)
        .is_err());
    assert!(store
        .canonical_context_evidence_unlocked(&face, &person)
        .unwrap()
        .is_empty());
    reference_request.expected_revision = 2;
    reference_request.capture_unix_millis = Some(1005);
    reference_request.album_ids = vec!["album-1".into()];
    store.replace_media_context(&reference_request).unwrap();
    reference_assignment.face_revision += 1;
    store
        .upsert_json(
            ASSIGNMENT_TABLE,
            &reference_assignment.assignment_id,
            &reference_assignment,
        )
        .unwrap();
    assert!(store
        .canonical_context_evidence_unlocked(&face, &person)
        .unwrap()
        .is_empty());
    request.expected_revision = 1;
    request.media_fingerprint = "f".repeat(64);
    assert!(store.replace_media_context(&request).is_err());
    assert_eq!(
        store
            .media_context(&face.media_key)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let unchanged_identity = identity_rows(&store);
    request.media_fingerprint = face.media_fingerprint.clone();
    request.capture_unix_millis = None;
    request.time_window_millis = None;
    request.album_ids.clear();
    let tombstone = store.replace_media_context(&request).unwrap();
    assert_eq!(tombstone.revision, 2);
    assert!(store
        .canonical_context_evidence_bounded_unlocked(&face, &person, &mut exhausted_budget)
        .unwrap()
        .is_empty());
    assert_eq!(exhausted_budget, 0);
    request.expected_revision = tombstone.revision;
    request.capture_unix_millis = Some(1000);
    assert_eq!(store.replace_media_context(&request).unwrap().revision, 3);
    assert!(store
        .canonical_context_evidence_bounded_unlocked(&face, &person, &mut exhausted_budget)
        .unwrap()
        .is_empty());
    assert_eq!(exhausted_budget, 0);
    assert_eq!(identity_rows(&store), unchanged_identity);
    drop(store);
    surreal_store::wait_until_closed(&MediaDb::db_path(&root)).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
