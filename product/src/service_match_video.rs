//! Match video work runs off-render and publishes only through store-owned fences.
use super::*;
use crate::match_store::{IndexJob, JobAsset, JobStage, MatchStore, RevisionFence};
use crate::match_video::{VideoDetection, VideoFrame, VideoPolicy};
use crate::match_video_decode::{DecodedVideoSample, FIRST_VIDEO_STREAM};
use crate::match_worker::{IsolatedMatchWorker, WorkerError, WorkerFence};
use crate::media_io::{MediaIoCoordinator, PermitOutcome, RootIdentity};

const OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const PIXEL_BYTES: u64 = 512 * 1024 * 1024;

struct VideoRun<'a> {
    store: &'a MatchStore,
    job: &'a IndexJob,
    asset: Option<&'a JobAsset>,
    coordinator: &'a MediaIoCoordinator,
    root: &'a RootIdentity,
    cancelled: &'a AtomicBool,
}
impl VideoRun<'_> {
    fn compute<T>(
        &self,
        worker: &mut IsolatedMatchWorker,
        action: impl FnOnce(&mut IsolatedMatchWorker, &WorkerFence) -> Result<T, WorkerError>,
    ) -> Result<(T, u64), String> {
        self.compute_with_units(worker, 1, action)
    }
    fn infer<T>(
        &self,
        worker: &mut IsolatedMatchWorker,
        action: impl FnOnce(&mut IsolatedMatchWorker, &WorkerFence) -> Result<T, WorkerError>,
    ) -> Result<(T, u64), String> {
        let units = worker.cpu_policy().active_units();
        self.compute_with_units(worker, units, action)
    }
    fn compute_with_units<T>(
        &self,
        worker: &mut IsolatedMatchWorker,
        cpu_units: u64,
        action: impl FnOnce(&mut IsolatedMatchWorker, &WorkerFence) -> Result<T, WorkerError>,
    ) -> Result<(T, u64), String> {
        let revision = self
            .asset
            .map(|asset| match_revision_fence(self.job, asset));
        let lease = retry_match_resource_pressure(
            self.cancelled,
            || match_worker_can_continue(self.store, &self.job.job_id, self.cancelled),
            || {
                self.store.acquire_worker_compute(
                    self.coordinator,
                    self.root.clone(),
                    &self.job.job_id,
                    revision.as_ref(),
                    {
                        let mut request = match_compute_request(OUTPUT_BYTES, PIXEL_BYTES);
                        request.cpu_inference = cpu_units;
                        request
                    },
                    &mut None,
                )
            },
        )?;
        let epoch = lease.admission_epoch();
        let fence = match_worker_fence(self.job, self.asset, epoch);
        let result = action(worker, &fence);
        lease.finish_after_worker(
            if result.is_ok() {
                PermitOutcome::Success
            } else {
                PermitOutcome::Error
            },
            worker,
        );
        let output = match result {
            Ok(value) => value,
            Err(error) if error.quarantined => {
                settle_match_worker_failure(
                    self.store,
                    self.job,
                    revision.as_ref(),
                    worker,
                    &error,
                )?;
                return Err("quarantined video worker".into());
            }
            Err(error) => return Err(format!("{}: {}", error.code, error.message)),
        };
        if self.cancelled.load(Ordering::Acquire)
            || self.store.external_admission_epoch() != epoch
            || !match_worker_can_continue(self.store, &self.job.job_id, self.cancelled)?
        {
            return Err("video worker admission changed before publication".into());
        }
        Ok((output, epoch))
    }
    fn publication(
        &self,
        fence: &RevisionFence,
        stage: JobStage,
    ) -> Result<crate::match_store::MatchStagePermit, String> {
        retry_match_resource_pressure(
            self.cancelled,
            || match_worker_can_continue(self.store, &self.job.job_id, self.cancelled),
            || {
                self.store.acquire_background_stage(
                    self.coordinator,
                    self.root.clone(),
                    fence,
                    stage,
                    match_stage_request(stage, OUTPUT_BYTES, 1),
                )
            },
        )
    }
    fn snapshot(
        &self,
        worker: &mut IsolatedMatchWorker,
        path: &Path,
        expected_root: &Path,
    ) -> Result<MatchFileSnapshot, String> {
        self.compute(worker, |worker, fence| {
            worker.begin_source(path, expected_root, fence)
        })?;
        let mut previous = 0;
        let mut identity = None;
        loop {
            let (progress, _) =
                self.compute(worker, |worker, fence| worker.hash_source_step(fence))?;
            let observed = (progress.final_path.clone(), progress.size);
            if identity
                .as_ref()
                .is_some_and(|expected| expected != &observed)
                || !progress.final_path.starts_with(expected_root)
                || progress.bytes_hashed < previous
                || progress.bytes_hashed.saturating_sub(previous) > 4 * 1024 * 1024
                || (progress.bytes_hashed == previous && progress.fingerprint.is_none())
            {
                return Err("video source hashing failed its progress or identity fence".into());
            }
            identity = Some(observed);
            previous = progress.bytes_hashed;
            if let Some(fingerprint) = progress.fingerprint {
                if progress.bytes_hashed != progress.size
                    || crate::match_store::canonical_media_sha256(&fingerprint)
                        != Some(fingerprint.as_str())
                {
                    return Err("video source returned invalid completed fingerprint".into());
                }
                return Ok(MatchFileSnapshot {
                    fingerprint,
                    final_path: progress.final_path,
                    bytes: Vec::new(),
                });
            }
        }
    }
}

pub(super) fn video_discovery_snapshot(
    store: &MatchStore,
    job: &IndexJob,
    worker: &mut IsolatedMatchWorker,
    path: &Path,
    expected_root: &Path,
    coordinator: &MediaIoCoordinator,
    root: &RootIdentity,
    cancelled: &AtomicBool,
) -> Result<MatchFileSnapshot, String> {
    VideoRun {
        store,
        job,
        asset: None,
        coordinator,
        root,
        cancelled,
    }
    .snapshot(worker, path, expected_root)
}

pub(super) fn image_asset_snapshot(
    store: &MatchStore,
    job: &IndexJob,
    asset: &JobAsset,
    worker: &mut IsolatedMatchWorker,
    path: &Path,
    expected_root: &Path,
    coordinator: &MediaIoCoordinator,
    root: &RootIdentity,
    cancelled: &AtomicBool,
) -> Result<MatchFileSnapshot, String> {
    VideoRun {
        store,
        job,
        asset: Some(asset),
        coordinator,
        root,
        cancelled,
    }
    .snapshot(worker, path, expected_root)
}

fn frame_detections(
    batch: crate::match_worker::WorkerDetectionBatch,
) -> Result<Vec<VideoDetection>, String> {
    if batch.image_w == 0 || batch.image_h == 0 || batch.faces.len() > 128 {
        return Err("video detection dimensions or count invalid".into());
    }
    let w = batch.image_w as f32;
    let h = batch.image_h as f32;
    batch
        .faces
        .into_iter()
        .map(|face| {
            let [x, y, bw, bh] = face.bbox;
            let x = (x / w).clamp(0., 1.);
            let y = (y / h).clamp(0., 1.);
            let bounds = [x, y, (bw / w).clamp(0., 1. - x), (bh / h).clamp(0., 1. - y)];
            Ok(VideoDetection {
                source_index: u32::try_from(face.source_index)
                    .map_err(|_| "video detection index overflow")?,
                bounds,
                quality: face.score,
                pose_bucket: crate::identity::yaw_bucket(&face.landmarks).0.to_string(),
                detector_generation: String::new(),
            })
        })
        .collect()
}

fn publish_pending(
    run: &VideoRun<'_>,
    worker: &mut IsolatedMatchWorker,
    path: &Path,
    stream: u32,
    fence: &RevisionFence,
    cached: Option<&DecodedVideoSample>,
) -> Result<(), String> {
    loop {
        let pending =
            run.store
                .pending_video_exemplars(&fence.media_key, stream, &fence.model_generation)?;
        if pending.is_empty() {
            return Ok(());
        }
        for row in pending {
            let observation = row.observation()?;
            let decoded;
            let sample = if let Some(sample) = cached.filter(|s| {
                s.time == observation.time && s.frame_sha256 == observation.frame_sha256
            }) {
                sample
            } else {
                decoded = run
                    .compute(worker, |worker, fence| {
                        worker.decode_exact(path, observation.time, stream, fence)
                    })?
                    .0
                    .ok_or("pending video exemplar disappeared")?;
                &decoded
            };
            if sample.time != observation.time
                || sample.frame_sha256 != observation.frame_sha256
                || sample.stream_index != stream
            {
                return Err("pending video exemplar source/PTS changed".into());
            }
            let (batch, epoch) = run.infer(worker, |worker, fence| {
                let mut exact = fence.clone();
                exact.track_id = Some(observation.track_id.clone());
                exact.timestamp_ms = Some(observation.time.milliseconds().unwrap_or(0));
                worker.embed_video_exemplar(
                    sample.encoded.clone(),
                    path,
                    observation.detection.source_index as usize,
                    &exact,
                )
            })?;
            if batch.faces.len() != 1 || !batch.failures.is_empty() {
                return Err("video exemplar embedding failed".into());
            }
            let face = &batch.faces[0];
            let value = crate::match_store::VideoExemplarEmbedding {
                observation_id: observation.observation_id,
                frame_sha256: sample.frame_sha256.clone(),
                time: sample.time,
                stream_index: stream,
                model_generation: fence.model_generation.clone(),
                vector: face.embedding.clone(),
                bounds: face.bbox_normalized,
                landmarks: face.landmarks_normalized,
                source_width: batch.image_w,
                source_height: batch.image_h,
            };
            let permit = run.publication(fence, JobStage::Detect)?;
            run.store
                .publish_video_exemplar(fence, &permit, epoch, &value)?;
        }
    }
}

pub(super) fn run_match_video_asset(
    store: &MatchStore,
    job: &IndexJob,
    asset: &JobAsset,
    source_path: &Path,
    expected_root: &Path,
    worker: &mut IsolatedMatchWorker,
    coordinator: &MediaIoCoordinator,
    root_identity: &RootIdentity,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let run = VideoRun {
        store,
        job,
        asset: Some(asset),
        coordinator,
        root: root_identity,
        cancelled,
    };
    let fence = match_revision_fence(job, asset);
    let mut stage = asset.next_stage()?;
    if stage == JobStage::Discover {
        let permit = run.publication(&fence, stage)?;
        store.commit_asset_stage(&asset.asset_id, stage, &fence, &permit)?;
        stage = JobStage::Detect;
    }
    if stage == JobStage::Detect {
        let snapshot = run.snapshot(worker, source_path, expected_root)?;
        if !match_source_fingerprint_matches(&snapshot.fingerprint, &asset.media_fingerprint) {
            return Err("video source fingerprint changed".into());
        }
        let path = &snapshot.final_path;
        let (first, _) = run.compute(worker, |worker, fence| {
            worker.decode(path, 0, FIRST_VIDEO_STREAM, fence)
        })?;
        let first = first.ok_or("video contains no decodable frame")?;
        let stream = first.stream_index;
        let playback_origin = first.playback_origin;
        let policy = VideoPolicy::default();
        let checkpoint = store.video_checkpoint(&asset.media_key, stream)?;
        publish_pending(&run, worker, path, stream, &fence, Some(&first))?;
        let mut sample = Some(first);
        let mut previous_probe = None;
        if checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.playback_origin != playback_origin)
        {
            return Err("video playback origin changed".into());
        }
        if let Some(last) = checkpoint.and_then(|checkpoint| checkpoint.last_time) {
            let (previous, _) = run.compute(worker, |worker, fence| {
                worker.decode_exact(path, last, stream, fence)
            })?;
            let previous = previous.ok_or("video checkpoint frame disappeared")?;
            if previous.time != last {
                return Err("video checkpoint timestamp changed".into());
            }
            previous_probe = Some(previous.scene_probe);
            sample = run
                .compute(worker, |worker, fence| {
                    worker.decode(
                        path,
                        last.milliseconds().unwrap_or(0) + u64::from(policy.sample_interval_ms),
                        stream,
                        fence,
                    )
                })?
                .0;
        }
        while let Some(current) = sample {
            let scene_score = previous_probe
                .as_deref()
                .map(|previous| {
                    crate::match_video_decode::scene_change(previous, &current.scene_probe)
                })
                .transpose()?
                .unwrap_or(1.0);
            let (detected, epoch) = run.infer(worker, |worker, fence| {
                worker.detect(current.encoded.clone(), path, fence)
            })?;
            let mut detections = frame_detections(detected)?;
            for detection in &mut detections {
                detection.detector_generation = job.model_generation.clone();
            }
            let frame = VideoFrame {
                stream_index: stream,
                time: current.time,
                playback_origin,
                frame_sha256: current.frame_sha256.clone(),
                scene_score,
                detections,
            };
            let permit = run.publication(&fence, JobStage::Detect)?;
            store.commit_video_frame(&fence, &permit, epoch, &policy, frame)?;
            publish_pending(&run, worker, path, stream, &fence, Some(&current))?;
            let next = current
                .time
                .milliseconds()?
                .checked_add(u64::from(policy.sample_interval_ms))
                .ok_or("video cursor overflow")?;
            previous_probe = Some(current.scene_probe);
            sample = run
                .compute(worker, |worker, fence| {
                    worker.decode(path, next, stream, fence)
                })?
                .0;
        }
        let epoch = store.external_admission_epoch();
        let permit = run.publication(&fence, JobStage::Detect)?;
        store.finalize_video_tracks(&fence, &permit, epoch, stream)?;
        let permit = run.publication(&fence, JobStage::Detect)?;
        store.commit_asset_stage(&asset.asset_id, JobStage::Detect, &fence, &permit)?;
        stage = JobStage::Align;
    }
    // Detection/exemplar writes and resumable checkpoint precede these cursors.
    for next in [
        JobStage::Align,
        JobStage::Embed,
        JobStage::Persist,
        JobStage::Suggest,
        JobStage::Complete,
    ] {
        if stage == next {
            if next == JobStage::Persist {
                let permit = run.publication(&fence, JobStage::Persist)?;
                store.publish_projection(
                    crate::match_store::PeopleProjection {
                        media_key: asset.media_key.clone(),
                        media_fingerprint: asset.media_fingerprint.clone(),
                        schema_generation: job.schema_generation.clone(),
                        model_generation: job.model_generation.clone(),
                        identity_revision: job.identity_revision,
                        catalog_revision: job.catalog_revision,
                        person_ids: Vec::new(),
                        published_at: crate::match_store::now(),
                    },
                    &fence,
                    &permit,
                )?;
                // Projection and Persist cursor now share one atomic checkpoint.
                stage = JobStage::Suggest;
                continue;
            }
            if next == JobStage::Suggest
                && store.has_active_strict_calibration(&job.model_generation)?
            {
                let mut cursor = String::new();
                loop {
                    let faces = store.video_exemplar_face_ids_page(
                        &asset.media_key,
                        &job.model_generation,
                        &cursor,
                        128,
                    )?;
                    if faces.is_empty() {
                        break;
                    }
                    cursor = faces
                        .last()
                        .ok_or("video exemplar page missing cursor")?
                        .clone();
                    for face_id in faces {
                        let permit = run.publication(&fence, JobStage::Suggest)?;
                        store.recognize_and_persist_strict(&face_id, &fence, &permit)?;
                    }
                }
            }
            let permit = run.publication(&fence, next)?;
            let updated = store.commit_asset_stage(&asset.asset_id, next, &fence, &permit)?;
            stage = updated.next_stage()?;
        }
    }
    Ok(())
}
