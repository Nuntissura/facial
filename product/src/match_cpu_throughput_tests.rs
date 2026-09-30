//! Opt-in bounded real-fixture indexing throughput, not whole-library evidence.
#![cfg(windows)]
use super::*;
use crate::match_store::{
    DesiredMode, FaceEmbedding, FaceObservation, JobLifecycle, MatchStore, ResourceUsage,
};
use crate::match_worker::CpuExecutionPolicy;
use serde_json::Value;
use std::{ffi::OsString, time::Instant};

struct OwnedFixture(PathBuf);
impl Drop for OwnedFixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retained throughput fixture: {}", self.0.display());
        } else {
            fs::remove_dir_all(&self.0).expect("remove owned throughput fixture");
        }
    }
}

fn pinned_bytes(path: &Path, limit: u64) -> (fs::File, Vec<u8>) {
    use std::os::windows::fs::OpenOptionsExt;
    let pin = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(path)
        .unwrap();
    let mut bytes = Vec::new();
    (&pin).take(limit + 1).read_to_end(&mut bytes).unwrap();
    assert!(!bytes.is_empty() && bytes.len() as u64 <= limit);
    (pin, bytes)
}

fn fixture_manifest(inputs: &BTreeMap<String, Vec<u8>>) -> Value {
    json!(inputs
        .iter()
        .map(|(name, bytes)| json!({"name": name,
        "bytes": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))}))
        .collect::<Vec<_>>())
}

fn clone_inputs(inputs: &BTreeMap<String, Vec<u8>>, root: &Path, expected: &Value) {
    fs::create_dir(root).unwrap();
    for (name, bytes) in inputs {
        fs::write(root.join(name), bytes).unwrap();
    }
    let reread: BTreeMap<_, _> = inputs
        .keys()
        .map(|name| (name.clone(), fs::read(root.join(name)).unwrap()))
        .collect();
    assert_eq!(fixture_manifest(&reread), *expected);
}

struct UsageSampler {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<ResourceUsage>>,
}
impl UsageSampler {
    fn start(governor: crate::match_store::MatchResourceGovernor) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut peak = ResourceUsage::default();
            loop {
                let usage = governor.usage().unwrap();
                macro_rules! maximum { ($($field:ident),+) => {$(peak.$field = peak.$field.max(usage.$field);)+}; }
                maximum!(
                    admitted_items,
                    queued_items,
                    queued_bytes,
                    cpu_inference,
                    decoded_bytes,
                    gpu_vram_bytes,
                    worker_memory_bytes,
                    surreal_writes,
                    vector_index_builds
                );
                if thread_stop.load(Ordering::Acquire) {
                    return peak;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
    fn finish(mut self) -> ResourceUsage {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap()
    }
}
impl Drop for UsageSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct FaceEvidence {
    time_ms: u64,
    face: FaceObservation,
    vector: Option<Vec<f32>>,
}
struct JobEvidence {
    faces: BTreeMap<String, Vec<FaceEvidence>>,
    tracks: BTreeMap<String, Vec<Value>>,
}

fn committed_evidence(
    store: &MatchStore,
    root: &Path,
    job_id: &str,
    generation: &str,
) -> JobEvidence {
    let completed = store.job(job_id).unwrap();
    assert_eq!(completed.lifecycle().unwrap(), JobLifecycle::Completed);
    assert_eq!((completed.completed, completed.failed), (4, 0));
    let assets = store.job_assets(job_id).unwrap();
    assert_eq!(assets.len(), 4);
    let database = crate::surreal_store::open(&crate::media_db::MediaDb::db_path(root)).unwrap();
    let db = database.db();
    let job_query = job_id.to_owned();
    let embeddings: Vec<FaceEmbedding> = crate::surreal_store::run(async move {
        let mut response = db.query("SELECT * OMIT id FROM match_face_embedding WHERE job_id = $job AND active = true LIMIT 4096;")
            .bind(("job", job_query)).await.map_err(|error| error.to_string())?
            .check().map_err(|error| error.to_string())?;
        response.take(0).map_err(|error| error.to_string())
    }).unwrap();
    assert!(!embeddings.is_empty() && embeddings.len() < 4096);
    let mut evidence = JobEvidence {
        faces: BTreeMap::new(),
        tracks: BTreeMap::new(),
    };
    let mut consumed_embeddings = BTreeSet::new();
    for asset in assets {
        assert_eq!(asset.model_generation, generation);
        let name = Path::new(asset.source_path.as_deref().unwrap())
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let projection = store.warm_projection(&asset.media_key).unwrap().unwrap();
        assert_eq!(projection.media_fingerprint, asset.media_fingerprint);
        assert!(
            projection.person_ids.is_empty(),
            "throughput must not assign a Person"
        );
        let tracks = store
            .video_media_tracks_page(&asset.media_key, "", 256)
            .unwrap();
        assert!(tracks.len() < 256);
        let mut time_by_face = BTreeMap::new();
        let mut exemplar_faces = BTreeSet::new();
        let mut normalized_tracks = Vec::new();
        for track in &tracks {
            assert!(track.closed && track.observation_count > 0 && track.exemplar_count > 0);
            assert!(store
                .pending_video_exemplars(&asset.media_key, track.stream_index, generation)
                .unwrap()
                .is_empty());
            for observation in &track.observations {
                if observation.exemplar {
                    assert!(exemplar_faces.insert(observation.face_id.clone()));
                }
                time_by_face.insert(
                    observation.face_id.clone(),
                    observation.time.milliseconds().unwrap(),
                );
            }
            normalized_tracks.push(json!({"stream_index": track.stream_index,
                "playback_origin": track.playback_origin, "start": track.start, "end": track.end,
                "observation_count": track.observation_count, "exemplar_count": track.exemplar_count,
                "timestamps": track.timestamps, "closed": track.closed,
                "observations": track.observations.iter().map(|row|
                    json!({"time": row.time, "exemplar": row.exemplar})).collect::<Vec<_>>() }));
        }
        if name.ends_with(".mkv") {
            assert!(!tracks.is_empty());
        } else {
            assert!(tracks.is_empty());
        }
        normalized_tracks.sort_by_key(Value::to_string);
        let faces = store.derived_faces_for_asset(&asset).unwrap();
        assert!(
            !faces.is_empty() && faces.len() < 4096,
            "real face evidence required for {name}"
        );
        let mut normalized_faces = Vec::new();
        for face in faces {
            let requires_embedding = tracks.is_empty() || exemplar_faces.contains(&face.face_id);
            let matches: Vec<_> = embeddings
                .iter()
                .filter(|embedding| embedding.face_id == face.face_id)
                .collect();
            assert_eq!(
                matches.len(),
                usize::from(requires_embedding),
                "one active embedding for still/current exemplar Faces; zero for nonexemplar observations"
            );
            let vector = matches.first().map(|embedding| {
                assert_eq!(embedding.model_generation, generation);
                assert_eq!(embedding.media_fingerprint, face.media_fingerprint);
                assert_eq!(embedding.face_revision, face.face_revision);
                assert_eq!(embedding.schema_generation, face.schema_generation);
                assert!(consumed_embeddings.insert(embedding.embedding_id.clone()));
                if !tracks.is_empty() {
                    assert!(face.alignment_valid);
                }
                embedding.vector.clone()
            });
            assert!(!face.operator_owned);
            let time_ms = if name.ends_with(".mkv") {
                *time_by_face.get(&face.face_id).unwrap()
            } else {
                0
            };
            normalized_faces.push(FaceEvidence {
                time_ms,
                face,
                vector,
            });
        }
        normalized_faces.sort_by(|a, b| {
            a.time_ms
                .cmp(&b.time_ms)
                .then(a.face.source_index.cmp(&b.face.source_index))
                .then(a.face.bounds_normalized[0].total_cmp(&b.face.bounds_normalized[0]))
        });
        assert!(evidence
            .faces
            .insert(name.clone(), normalized_faces)
            .is_none());
        evidence.tracks.insert(name, normalized_tracks);
    }
    assert_eq!(consumed_embeddings.len(), embeddings.len());
    evidence
}

fn near(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    assert!(a
        .iter()
        .zip(b)
        .all(|(a, b)| a.is_finite() && b.is_finite() && (a - b).abs() <= 0.0001));
}
fn evidence_parity(baseline: &JobEvidence, candidate: &JobEvidence) {
    assert_eq!(
        baseline.tracks, candidate.tracks,
        "canonical track/timestamp/exemplar parity"
    );
    assert_eq!(
        baseline.faces.keys().collect::<Vec<_>>(),
        candidate.faces.keys().collect::<Vec<_>>()
    );
    for (name, faces) in &baseline.faces {
        let other = &candidate.faces[name];
        assert_eq!(
            faces.len(),
            other.len(),
            "canonical face-count parity for {name}"
        );
        for (a, b) in faces.iter().zip(other) {
            assert_eq!(a.time_ms, b.time_ms);
            assert_eq!(a.face.source_index, b.face.source_index);
            assert_eq!(
                (
                    a.face.source_width,
                    a.face.source_height,
                    a.face.exif_orientation
                ),
                (
                    b.face.source_width,
                    b.face.source_height,
                    b.face.exif_orientation
                )
            );
            assert_eq!(a.face.alignment_valid, b.face.alignment_valid);
            assert_eq!(a.face.pose_bucket, b.face.pose_bucket);
            near(&a.face.bounds_normalized, &b.face.bounds_normalized);
            assert_eq!(
                a.face.landmarks_normalized.len(),
                b.face.landmarks_normalized.len()
            );
            for (a, b) in a
                .face
                .landmarks_normalized
                .iter()
                .zip(&b.face.landmarks_normalized)
            {
                near(a, b);
            }
            near(&[a.face.quality], &[b.face.quality]);
            assert_eq!(
                a.vector.is_some(),
                b.vector.is_some(),
                "current exemplar vector presence parity"
            );
            let (Some(a_vector), Some(b_vector)) = (&a.vector, &b.vector) else {
                continue;
            };
            assert_eq!(a_vector.len(), crate::identity::EMBEDDING_DIM);
            near(a_vector, b_vector);
            let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
            let (na, nb) = (norm(a_vector), norm(b_vector));
            assert!((na - 1.0).abs() <= 0.001 && (nb - 1.0).abs() <= 0.001);
            let dot: f64 = a_vector
                .iter()
                .zip(b_vector)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            assert!((1.0 - dot / (na * nb)).abs() <= 0.00001);
        }
    }
}

#[test]
#[ignore = "explicit existing FACIAL_WP086_RETAINED_THROUGHPUT_ROOT; SELECT-only canonical reconciliation"]
fn wp086_retained_throughput_fixture_exemplar_embedding_reconciliation() {
    use crate::match_store::{IndexJob, JobAsset, StoredVideoObservation};
    let root = std::env::var_os("FACIAL_WP086_RETAINED_THROUGHPUT_ROOT")
        .map(PathBuf::from)
        .expect("explicit retained throughput workspace required");
    assert!(root.is_absolute() && root.is_dir());
    let database_root = crate::media_db::MediaDb::db_path(&root);
    assert!(
        database_root.is_dir(),
        "existing canonical database required"
    );
    assert!(
        database_root
            .parent()
            .unwrap()
            .join("engine.json")
            .is_file(),
        "existing engine marker required; never initialize a new database"
    );
    assert!(std::fs::read_dir(&database_root).unwrap().next().is_some());
    // Open the existing engine directly: no MatchStore schema/recovery or indexing calls.
    let database = crate::surreal_store::open(&database_root).unwrap();
    let guard = database.transaction_lock().read().unwrap();
    let db = database.db();
    let (jobs, assets, faces, embeddings, observations): (
        Vec<IndexJob>,
        Vec<JobAsset>,
        Vec<FaceObservation>,
        Vec<FaceEmbedding>,
        Vec<StoredVideoObservation>,
    ) = crate::surreal_store::run(async move {
        let mut response = db
            .query(
                "SELECT * OMIT id FROM match_index_job LIMIT 2;\
             SELECT * OMIT id FROM match_job_asset LIMIT 4096;\
             SELECT * OMIT id FROM match_face_observation LIMIT 4096;\
             SELECT * OMIT id FROM match_face_embedding WHERE active=true LIMIT 4096;\
             SELECT * OMIT id FROM match_video_observation LIMIT 4096;",
            )
            .await
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        Ok((
            response.take(0).map_err(|error| error.to_string())?,
            response.take(1).map_err(|error| error.to_string())?,
            response.take(2).map_err(|error| error.to_string())?,
            response.take(3).map_err(|error| error.to_string())?,
            response.take(4).map_err(|error| error.to_string())?,
        ))
    })
    .unwrap();
    assert_eq!(
        jobs.len(),
        1,
        "the exact retained first cold trial has one job"
    );
    assert_eq!(jobs[0].lifecycle().unwrap(), JobLifecycle::Completed);
    assert_eq!((jobs[0].completed, jobs[0].failed), (4, 0));
    assert_eq!(assets.len(), 4);
    assert!(!faces.is_empty() && faces.len() < 4096);
    assert!(!embeddings.is_empty() && embeddings.len() < 4096);
    assert!(!observations.is_empty() && observations.len() < 4096);
    let mut consumed = BTreeSet::new();
    let mut nonexemplar_count = 0;
    let mut unaligned_nonexemplar_count = 0;
    for observation in &observations {
        assert!(observations
            .iter()
            .any(|row| row.track_id == observation.track_id && row.closed));
        let face = faces
            .iter()
            .find(|face| face.face_id == observation.face_id)
            .unwrap();
        assert_eq!(observation.media_key, face.media_key);
        assert_eq!(observation.media_fingerprint, face.media_fingerprint);
    }
    for face in &faces {
        assert!(!face.operator_owned);
        let asset = assets
            .iter()
            .find(|asset| asset.media_key == face.media_key)
            .unwrap();
        assert_eq!(asset.job_id, jobs[0].job_id);
        assert_eq!(asset.model_generation, jobs[0].model_generation);
        assert_eq!(asset.media_fingerprint, face.media_fingerprint);
        assert_eq!(asset.schema_generation, face.schema_generation);
        let is_video = Path::new(asset.source_path.as_deref().unwrap())
            .extension()
            .and_then(|value| value.to_str())
            == Some("mkv");
        let observation = observations.iter().find(|row| row.face_id == face.face_id);
        assert_eq!(observation.is_some(), is_video);
        let requires_embedding = !is_video || observation.unwrap().exemplar;
        let matches = embeddings
            .iter()
            .filter(|embedding| embedding.face_id == face.face_id)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), usize::from(requires_embedding));
        if let Some(embedding) = matches.first() {
            if is_video {
                assert!(face.alignment_valid);
            }
            assert_eq!(embedding.job_id, jobs[0].job_id);
            assert_eq!(embedding.model_generation, jobs[0].model_generation);
            assert_eq!(embedding.media_fingerprint, face.media_fingerprint);
            assert_eq!(embedding.face_revision, face.face_revision);
            assert_eq!(embedding.schema_generation, face.schema_generation);
            assert_eq!(embedding.vector.len(), crate::identity::EMBEDDING_DIM);
            assert!(embedding.vector.iter().all(|value| value.is_finite()));
            assert!(consumed.insert(&embedding.embedding_id));
        } else {
            nonexemplar_count += 1;
            unaligned_nonexemplar_count += usize::from(!face.alignment_valid);
        }
    }
    assert_eq!(consumed.len(), embeddings.len());
    assert!(
        nonexemplar_count > 0,
        "retained fixture must exercise the original invalid all-Faces active embedding assumption"
    );
    println!("retained_canonical_reconciliation jobs={} assets={} faces={} active_embeddings={} video_observations={} nonexemplar_faces={} unaligned_nonexemplar_faces={}",
        jobs.len(), assets.len(), faces.len(), embeddings.len(), observations.len(), nonexemplar_count, unaligned_nonexemplar_count);
    drop(guard);
    drop(database);
    crate::surreal_store::wait_until_closed(&database_root).unwrap();
}

struct Trial {
    cold: JobEvidence,
    warm: JobEvidence,
    cold_micros: u64,
    warm_micros: u64,
}

fn trial(
    root: &Path,
    manifest: &Path,
    generation: &str,
    cold_inputs: &BTreeMap<String, Vec<u8>>,
    warm_inputs: &BTreeMap<String, Vec<u8>>,
    policy: CpuExecutionPolicy,
    round: usize,
) -> Trial {
    fs::create_dir(root).unwrap();
    let store = MatchStore::open(root).unwrap();
    store.register_model_generation(generation, true).unwrap();
    store.activate_model_generation(generation).unwrap();
    store.set_desired_mode(DesiredMode::Running).unwrap();
    let coordinator = Arc::new(crate::media_io::MediaIoCoordinator::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut worker = None;
    let sampler = UsageSampler::start(store.governor().clone());
    let mut job_evidence = Vec::new();
    let mut timings = Vec::new();
    let mut cold_worker_id = String::new();
    for stage in ["cold", "warm"] {
        let inputs = if stage == "cold" {
            cold_inputs
        } else {
            warm_inputs
        };
        let input_manifest = fixture_manifest(inputs);
        let media = root.join(stage);
        clone_inputs(inputs, &media, &input_manifest);
        let configured = store.configure_index_root(&media, vec![]).unwrap();
        let queued = store
            .start_index_job(&configured.root_id, generation)
            .unwrap();
        let running = store
            .set_job_lifecycle(&queued.job_id, JobLifecycle::Running)
            .unwrap();
        let started = Instant::now();
        run_match_index_job_with_cpu_policy(
            store.clone(),
            running.job_id.clone(),
            &mut worker,
            manifest,
            coordinator.clone(),
            cancelled.clone(),
            policy,
        )
        .unwrap();
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap();
        assert!(micros > 0);
        timings.push(micros);
        let child = worker.as_ref().unwrap();
        assert!(child.is_prepared_for(generation));
        assert_eq!(child.cpu_policy(), policy);
        if stage == "cold" {
            cold_worker_id = child.worker_id().to_owned();
        } else {
            assert_eq!(
                child.worker_id(),
                cold_worker_id,
                "warm job must reuse prepared worker"
            );
        }
        assert_eq!(store.governor().usage().unwrap().cpu_inference, 0);
        assert_eq!(
            store.governor().usage().unwrap().worker_memory_bytes,
            crate::match_store::WORKER_MEMORY_LIMIT_BYTES
        );
        job_evidence.push(committed_evidence(
            &store,
            root,
            &running.job_id,
            generation,
        ));
        let reread: BTreeMap<_, _> = inputs
            .keys()
            .map(|name| (name.clone(), fs::read(media.join(name)).unwrap()))
            .collect();
        assert_eq!(
            fixture_manifest(&reread),
            input_manifest,
            "indexing mutated fixture inputs"
        );
    }
    evidence_parity(&job_evidence[0], &job_evidence[1]);
    let peak = sampler.finish();
    let budget = store.governor().budget();
    assert!(
        peak.cpu_inference <= policy.active_units() && peak.cpu_inference <= budget.cpu_inference
    );
    assert!(
        peak.admitted_items <= budget.admitted_items
            && peak.queued_items <= budget.queued_items
            && peak.queued_bytes <= budget.queued_bytes
            && peak.decoded_bytes <= budget.decoded_bytes
            && peak.gpu_vram_bytes <= budget.gpu_vram_bytes
            && peak.worker_memory_bytes <= budget.worker_memory_bytes
            && peak.surreal_writes <= budget.surreal_writes
            && peak.vector_index_builds <= budget.vector_index_builds
    );
    let child = worker.as_mut().unwrap();
    let before = child.memory_peaks().unwrap();
    let phases = child.phase_arrivals();
    assert!(child.shutdown_and_confirm());
    let after = child.memory_peaks().unwrap();
    let process_peak = before.0.max(after.0);
    let job_peak = before.1.max(after.1);
    assert!(process_peak > 0 && process_peak <= crate::match_store::WORKER_MEMORY_LIMIT_BYTES);
    assert!(job_peak > 0 && job_peak <= crate::match_store::WORKER_MEMORY_LIMIT_BYTES);
    drop(worker);
    assert_eq!(store.governor().usage().unwrap(), ResourceUsage::default());
    println!(
        "{}",
        json!({"proof": "bounded-real-fixture-indexing-throughput", "round": round,
        "policy": format!("{policy:?}"), "cold_full_indexing_including_preparation_micros": timings[0],
        "warm_full_indexing_micros": timings[1], "last_operation_phase_arrivals": phases,
        "canonical_cold_counts": {"completed": 4, "failed": 0},
        "canonical_warm_counts": {"completed": 4, "failed": 0},
        "canonical_cold_faces": job_evidence[0].faces.values().map(Vec::len).sum::<usize>(),
        "canonical_warm_faces": job_evidence[1].faces.values().map(Vec::len).sum::<usize>(),
        "canonical_cold_tracks": job_evidence[0].tracks.values().map(Vec::len).sum::<usize>(),
        "canonical_warm_tracks": job_evidence[1].tracks.values().map(Vec::len).sum::<usize>(),
        "sampled_resource_peak": peak, "resource_sampling_ms": 5, "resource_budget": budget,
        "peak_process_bytes": process_peak, "peak_job_bytes": job_peak,
        "worker_memory_limit_bytes": crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
        "confirmed_exit": true, "terminal_usage": store.governor().usage().unwrap(), "promoted": false})
    );
    let db_path = crate::media_db::MediaDb::db_path(root);
    drop(store);
    crate::surreal_store::wait_until_closed(&db_path).unwrap();
    Trial {
        cold: job_evidence.remove(0),
        warm: job_evidence.remove(0),
        cold_micros: timings[0],
        warm_micros: timings[1],
    }
}

#[test]
#[ignore = "requires FACIAL_WP086_MANIFEST, three exact real images, FFmpeg, built facial-cli, canonical Cargo guard TEMP"]
fn wp086_real_fixture_paired_full_indexing_cpu_throughput() {
    let product = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = product.parent().unwrap();
    let guard_temp = repo.join("build-artifacts/tmp").canonicalize().unwrap();
    let actual_temp = std::env::temp_dir().canonicalize().unwrap();
    assert!(actual_temp.starts_with(&guard_temp));
    let owned =
        OwnedFixture(actual_temp.join(format!("wp086-cpu-throughput-{}", Uuid::new_v4().simple())));
    fs::create_dir(&owned.0).unwrap();
    let manifest = PathBuf::from(
        std::env::var_os("FACIAL_WP086_MANIFEST")
            .expect("explicit verified real manifest required"),
    );
    let (_manifest_pin, manifest_bytes) = pinned_bytes(&manifest, 64 * 1024);
    let manifest_json: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let generation = manifest_json["generation"].as_str().unwrap().to_owned();
    assert_eq!(generation.len(), 64);
    assert!(generation.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let frames = owned.0.join("video-frames");
    fs::create_dir(&frames).unwrap();
    let mut pins = Vec::new();
    let mut inputs = BTreeMap::new();
    let fixture_names = [
        "Aaron_Eckhart_0001.jpg",
        "Aaron_Guiel_2.jpg",
        "Abdullah_Gul_0003.jpg",
    ];
    for (index, name) in fixture_names.iter().enumerate() {
        let path = repo
            .join("_source_checks/eDifFIQA/example_images")
            .join(name);
        let (pin, bytes) = pinned_bytes(&path, 8 * 1024 * 1024);
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
        let frame =
            image::imageops::resize(&decoded, 640, 640, image::imageops::FilterType::Triangle);
        frame
            .save(frames.join(format!("frame-{:02}.png", index + 1)))
            .unwrap();
        pins.push(pin);
        inputs.insert(format!("face-{:02}.jpg", index + 1), bytes);
    }
    let video = owned.0.join("three-scene-video.mkv");
    let executable = crate::media_thumbs::resolve_ffmpeg().expect("existing FFmpeg required");
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-framerate",
        "1",
        "-start_number",
        "1",
        "-i",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(frames.join("frame-%02d.png").into_os_string());
    args.extend(
        [
            "-t",
            "3",
            "-r",
            "4",
            "-an",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
            "-fflags",
            "+bitexact",
            "-flags:v",
            "+bitexact",
            "-f",
            "matroska",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(video.as_os_str().to_owned());
    let (ok, _, stderr) =
        crate::match_decoder_process::run(&executable, &args, 1024, 65536).unwrap();
    assert!(
        ok,
        "three-scene fixture generation failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    let video_bytes = fs::read(&video).unwrap();
    assert!(!video_bytes.is_empty() && video_bytes.len() <= 16 * 1024 * 1024);
    inputs.insert("three-scene-video.mkv".into(), video_bytes);
    // Canonical video observation IDs are content-bound; identical bytes under
    // another MediaKey are a replay conflict. Remux once with distinct metadata
    // so the later job indexes fresh video evidence with identical decoded frames.
    let warm_video = owned.0.join("three-scene-warm-video.mkv");
    let mut warm_args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-i",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    warm_args.push(video.as_os_str().to_owned());
    warm_args.extend(
        [
            "-map",
            "0",
            "-c",
            "copy",
            "-map_metadata",
            "-1",
            "-metadata",
            "title=wp086-warm-fixture",
            "-fflags",
            "+bitexact",
            "-f",
            "matroska",
        ]
        .into_iter()
        .map(OsString::from),
    );
    warm_args.push(warm_video.as_os_str().to_owned());
    let (ok, _, stderr) =
        crate::match_decoder_process::run(&executable, &warm_args, 1024, 65536).unwrap();
    assert!(
        ok,
        "warm fixture remux failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    let warm_video_bytes = fs::read(&warm_video).unwrap();
    assert!(!warm_video_bytes.is_empty() && warm_video_bytes.len() <= 16 * 1024 * 1024);
    assert_ne!(
        Sha256::digest(&warm_video_bytes),
        Sha256::digest(&inputs["three-scene-video.mkv"])
    );
    let mut warm_inputs = inputs.clone();
    warm_inputs.insert("three-scene-video.mkv".into(), warm_video_bytes);
    let input_manifest =
        json!({"cold": fixture_manifest(&inputs), "warm": fixture_manifest(&warm_inputs)});
    let fixture_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&input_manifest).unwrap())
    );
    let mut rows = Vec::new();
    for round in 0..2 {
        let policies = if round == 0 {
            [
                CpuExecutionPolicy::Baseline,
                CpuExecutionPolicy::PrivateTwoThread,
            ]
        } else {
            [
                CpuExecutionPolicy::PrivateTwoThread,
                CpuExecutionPolicy::Baseline,
            ]
        };
        let mut trials = Vec::new();
        for policy in policies {
            trials.push((
                policy,
                trial(
                    &owned.0.join(format!("round-{round}-{policy:?}")),
                    &manifest,
                    &generation,
                    &inputs,
                    &warm_inputs,
                    policy,
                    round,
                ),
            ));
        }
        let baseline = &trials
            .iter()
            .find(|(policy, _)| *policy == CpuExecutionPolicy::Baseline)
            .unwrap()
            .1;
        let candidate = &trials
            .iter()
            .find(|(policy, _)| *policy == CpuExecutionPolicy::PrivateTwoThread)
            .unwrap()
            .1;
        evidence_parity(&baseline.cold, &candidate.cold);
        evidence_parity(&baseline.warm, &candidate.warm);
        rows.push(json!({"round": round, "order": policies.iter().map(|policy| format!("{policy:?}")).collect::<Vec<_>>(),
            "cold_baseline_micros": baseline.cold_micros, "cold_cpu_two_micros": candidate.cold_micros,
            "warm_baseline_micros": baseline.warm_micros, "warm_cpu_two_micros": candidate.warm_micros,
            "cold_speed_ratio": baseline.cold_micros as f64 / candidate.cold_micros as f64,
            "warm_speed_ratio": baseline.warm_micros as f64 / candidate.warm_micros as f64,
            "canonical_persisted_parity": true}));
    }
    assert_eq!(
        json!({"cold": fixture_manifest(&inputs), "warm": fixture_manifest(&warm_inputs)}),
        input_manifest
    );
    for (index, name) in fixture_names.iter().enumerate() {
        assert_eq!(
            fs::read(
                repo.join("_source_checks/eDifFIQA/example_images")
                    .join(name)
            )
            .unwrap(),
            inputs[&format!("face-{:02}.jpg", index + 1)],
            "original source changed"
        );
    }
    println!(
        "{}",
        json!({"proof": "bounded-real-fixture-indexing-throughput", "fixture_manifest": input_manifest,
        "fixture_manifest_sha256": fixture_hash, "generation": generation,
        "source_fixtures": fixture_names,
        "model_manifest_sha256": format!("{:x}", Sha256::digest(&manifest_bytes)),
        "fixture_limits": {"real_images": 3, "video_duration_seconds": 3, "video_fps": 4,
            "video_frame_dimensions": [640, 640], "video_max_bytes": 16777216, "rounds": 2,
            "warm_video_changes": "physically byte-distinct container metadata; stream copy",
            "assets_per_cold_job": 4, "assets_per_warm_job": 4}, "paired_trials": rows,
        "performance_threshold": null, "duplicate_video_correctness_claim": false,
        "whole_library_claim": false, "promoted": false})
    );
    drop(pins);
}
