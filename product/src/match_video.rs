//! Bounded, decoder-independent video appearance tracking. Tracking never assigns People.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoPolicy {
    pub version: u32,
    pub sample_interval_ms: u32,
    pub maximum_gap_ms: u32,
    pub scene_threshold: f32,
    pub association_iou: f32,
    pub minimum_quality: f32,
    pub max_detections: usize,
    pub max_active_tracks: usize,
    pub max_exemplars: usize,
    pub max_samples: u32,
    pub max_observations_per_track: u32,
}

impl Default for VideoPolicy {
    fn default() -> Self {
        // Time subsampling and content cuts follow the refinement's scene-detector
        // pattern. Conservative IoU association is geometry evidence, not identity.
        // Five bounded pose winners prevent repeated frames dominating the gallery.
        Self {
            version: VERSION,
            sample_interval_ms: 500,
            maximum_gap_ms: 1_000,
            scene_threshold: 0.35,
            association_iou: 0.5,
            minimum_quality: 0.5,
            max_detections: 64,
            max_active_tracks: 64,
            max_exemplars: 5,
            max_samples: 100_000,
            max_observations_per_track: 600,
        }
    }
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}
fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn digest(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .map_err(|e| e.to_string())
}
impl VideoPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION
            || !(1..=60_000).contains(&self.sample_interval_ms)
            || self.maximum_gap_ms < self.sample_interval_ms
            || self.maximum_gap_ms > 120_000
            || !unit(self.scene_threshold)
            || self.scene_threshold == 0.0
            || !unit(self.association_iou)
            || self.association_iou == 0.0
            || !unit(self.minimum_quality)
            || !(1..=128).contains(&self.max_detections)
            || !(1..=128).contains(&self.max_active_tracks)
            || !(1..=16).contains(&self.max_exemplars)
            || !(1..=100_000).contains(&self.max_samples)
            || !(1..=2048).contains(&self.max_observations_per_track)
        {
            return Err("invalid bounded video policy".into());
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String, String> {
        self.validate()?;
        digest(self)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VideoTime {
    pub pts: i64,
    pub numerator: u32,
    pub denominator: u32,
}
impl Default for VideoTime {
    fn default() -> Self {
        Self {
            pts: 0,
            numerator: 1,
            denominator: 1000,
        }
    }
}
impl VideoTime {
    pub fn playback_milliseconds(self, origin: Self) -> Result<u64, String> {
        self.validate()?;
        origin.validate()?;
        let pts =
            i128::from(self.pts) * i128::from(self.numerator) * i128::from(origin.denominator);
        let start =
            i128::from(origin.pts) * i128::from(origin.numerator) * i128::from(self.denominator);
        let delta = pts
            .checked_sub(start)
            .and_then(|v| v.checked_mul(1000))
            .ok_or("video relative timestamp overflow")?;
        if delta < 0 {
            return Err("video frame precedes playback origin".into());
        }
        u64::try_from(delta / (i128::from(self.denominator) * i128::from(origin.denominator)))
            .map_err(|_| "video frame precedes playback origin".into())
    }
    pub fn validate(self) -> Result<(), String> {
        if self.pts < 0 || self.numerator == 0 || self.denominator == 0 {
            return Err(
                "video timestamp requires nonnegative actual PTS and positive timebase".into(),
            );
        }
        Ok(())
    }
    pub fn milliseconds(self) -> Result<u64, String> {
        self.validate()?;
        u64::try_from(
            i128::from(self.pts) * i128::from(self.numerator) * 1000 / i128::from(self.denominator),
        )
        .map_err(|_| "video timestamp overflow".into())
    }
    fn after(self, other: Self) -> bool {
        i128::from(self.pts) * i128::from(self.numerator) * i128::from(other.denominator)
            > i128::from(other.pts) * i128::from(other.numerator) * i128::from(self.denominator)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoDetection {
    pub source_index: u32,
    pub bounds: [f32; 4],
    pub quality: f32,
    pub pose_bucket: String,
    pub detector_generation: String,
}
impl VideoDetection {
    fn validate(&self) -> Result<(), String> {
        let [x, y, w, h] = self.bounds;
        if !self.bounds.into_iter().all(unit)
            || w == 0.0
            || h == 0.0
            || x + w > 1.0
            || y + h > 1.0
            || !unit(self.quality)
            || self.pose_bucket.is_empty()
            || self.pose_bucket.len() > 64
            || self.detector_generation.is_empty()
            || self.detector_generation.len() > 256
            || self.pose_bucket.chars().any(char::is_control)
            || self.detector_generation.chars().any(char::is_control)
        {
            return Err("invalid video detection evidence".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoFrame {
    pub stream_index: u32,
    #[serde(default)]
    pub playback_origin: VideoTime,
    pub time: VideoTime,
    pub frame_sha256: String,
    /// Bounded normalized content change from the decoder's sampled scene probe.
    pub scene_score: f32,
    pub detections: Vec<VideoDetection>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoObservation {
    pub observation_id: String,
    pub track_id: String,
    pub policy_sha256: String,
    pub shot_anchor: String,
    pub stream_index: u32,
    #[serde(default)]
    pub playback_origin: VideoTime,
    pub time: VideoTime,
    pub frame_sha256: String,
    pub detection: VideoDetection,
}
impl VideoObservation {
    pub fn validate(&self) -> Result<(), String> {
        self.detection.validate()?;
        self.time.playback_milliseconds(self.playback_origin)?;
        if !hash(&self.observation_id)
            || !hash(&self.track_id)
            || !hash(&self.frame_sha256)
            || !hash(&self.policy_sha256)
            || !hash(&self.shot_anchor)
        {
            return Err("invalid video observation identity or frame hash".into());
        }
        Ok(())
    }
    pub fn validate_for_media(&self, media_sha256: &str) -> Result<(), String> {
        self.validate()?;
        if !hash(media_sha256)
            || self.observation_id
                != digest(&(
                    media_sha256,
                    self.stream_index,
                    &self.policy_sha256,
                    &self.shot_anchor,
                    self.playback_origin,
                    self.time,
                    &self.frame_sha256,
                    &self.detection,
                ))?
        {
            return Err("video observation canonical evidence digest mismatch".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoTrack {
    pub track_id: String,
    pub shot_anchor: String,
    pub first: VideoObservation,
    pub last: VideoObservation,
    pub observation_count: u32,
    pub exemplars: Vec<VideoObservation>,
}
impl VideoTrack {
    pub fn density_weight(&self) -> u32 {
        1
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoCheckpoint {
    pub version: u32,
    pub media_sha256: String,
    pub stream_index: u32,
    #[serde(default)]
    pub playback_origin: VideoTime,
    pub policy_sha256: String,
    pub shot_anchor: String,
    pub last_time: Option<VideoTime>,
    pub sample_count: u32,
    pub active: Vec<VideoTrack>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoUpdate {
    pub observations: Vec<VideoObservation>,
    pub closed_tracks: Vec<VideoTrack>,
}

pub struct VideoTracker {
    policy: VideoPolicy,
    state: VideoCheckpoint,
}
impl VideoTracker {
    pub fn new(
        media_sha256: String,
        stream_index: u32,
        policy: VideoPolicy,
    ) -> Result<Self, String> {
        if !hash(&media_sha256) {
            return Err("invalid video media SHA256".into());
        }
        let policy_sha256 = policy.digest()?;
        Ok(Self {
            policy,
            state: VideoCheckpoint {
                version: VERSION,
                media_sha256,
                stream_index,
                playback_origin: VideoTime::default(),
                policy_sha256,
                shot_anchor: String::new(),
                last_time: None,
                sample_count: 0,
                active: Vec::new(),
            },
        })
    }
    pub fn checkpoint(&self) -> VideoCheckpoint {
        self.state.clone()
    }
    pub fn restore(policy: VideoPolicy, state: VideoCheckpoint) -> Result<Self, String> {
        if state.version != VERSION
            || state.policy_sha256 != policy.digest()?
            || !hash(&state.media_sha256)
            || state.sample_count > policy.max_samples
            || state.active.len() > policy.max_active_tracks
            || (state.sample_count == 0) != state.last_time.is_none()
            || (state.sample_count > 0 && !hash(&state.shot_anchor))
        {
            return Err("invalid video checkpoint authority or bounds".into());
        }
        if let Some(time) = state.last_time {
            time.playback_milliseconds(state.playback_origin)?;
        }
        let mut ids = BTreeSet::new();
        for track in &state.active {
            if !ids.insert(&track.track_id)
                || track.shot_anchor != state.shot_anchor
                || track.observation_count == 0
                || track.observation_count > state.sample_count
                || track.observation_count > policy.max_observations_per_track
                || track.exemplars.len() > policy.max_exemplars
            {
                return Err("invalid video checkpoint track".into());
            }
            let expected_id = digest(&(
                &state.media_sha256,
                state.stream_index,
                &state.policy_sha256,
                &track.shot_anchor,
                track.first.time,
                track.first.detection.source_index,
            ))?;
            if expected_id != track.track_id {
                return Err("video checkpoint stable track mismatch".into());
            }
            let mut poses = BTreeSet::new();
            for observation in std::iter::once(&track.first)
                .chain(std::iter::once(&track.last))
                .chain(&track.exemplars)
            {
                observation.detection.validate()?;
                observation.time.milliseconds()?;
                if observation.track_id != track.track_id
                    || observation.playback_origin != state.playback_origin
                    || observation.policy_sha256 != state.policy_sha256
                    || observation.shot_anchor != track.shot_anchor
                    || observation.stream_index != state.stream_index
                    || !hash(&observation.frame_sha256)
                    || observation.observation_id
                        != observation_id(
                            &state,
                            observation.time,
                            &observation.frame_sha256,
                            &observation.detection,
                        )?
                    || track.first.time.after(observation.time)
                    || observation.time.after(track.last.time)
                    || state
                        .last_time
                        .is_none_or(|time| observation.time.after(time))
                {
                    return Err("invalid video checkpoint observation".into());
                }
            }
            for exemplar in &track.exemplars {
                if exemplar.detection.quality < policy.minimum_quality
                    || !poses.insert(&exemplar.detection.pose_bucket)
                {
                    return Err("invalid video checkpoint exemplars".into());
                }
            }
        }
        Ok(Self { policy, state })
    }
    pub fn should_sample(&self, time: VideoTime, scene_score: f32) -> Result<bool, String> {
        let ms = time.milliseconds()?;
        if !unit(scene_score) {
            return Err("invalid video scene score".into());
        }
        match self.state.last_time {
            None => Ok(true),
            Some(last) => {
                if !time.after(last) {
                    return Err("video PTS must advance".into());
                }
                Ok(scene_score >= self.policy.scene_threshold
                    || ms.saturating_sub(last.milliseconds()?)
                        >= u64::from(self.policy.sample_interval_ms))
            }
        }
    }
    pub fn ingest(&mut self, mut frame: VideoFrame) -> Result<VideoUpdate, String> {
        if frame.stream_index != self.state.stream_index
            || !hash(&frame.frame_sha256)
            || frame.detections.len() > self.policy.max_detections
            || frame.detections.len() > self.policy.max_active_tracks
            || self.state.sample_count >= self.policy.max_samples
        {
            return Err("video frame authority or resource bound exceeded".into());
        }
        if !self.should_sample(frame.time, frame.scene_score)? {
            return Err("video frame not admitted by sampling policy".into());
        }
        frame.detections.sort_by_key(|d| d.source_index);
        let mut indices = BTreeSet::new();
        for detection in &frame.detections {
            detection.validate()?;
            if !indices.insert(detection.source_index) {
                return Err("duplicate video detection index".into());
            }
        }
        // Work on a copy so a rejected sample never advances its durable cursor.
        let mut next = self.state.clone();
        if next.last_time.is_none() {
            next.playback_origin = frame.playback_origin;
        } else if next.playback_origin != frame.playback_origin {
            return Err("video playback origin changed".into());
        }
        frame.time.playback_milliseconds(frame.playback_origin)?;
        let mut closed_tracks = Vec::new();
        if next.last_time.is_none() || frame.scene_score >= self.policy.scene_threshold {
            closed_tracks.append(&mut next.active);
            next.shot_anchor = digest(&(frame.time, &frame.frame_sha256))?;
        }
        let ms = frame.time.milliseconds()?;
        let old = std::mem::take(&mut next.active);
        let mut edges = vec![Vec::new(); frame.detections.len()];
        let mut degrees = vec![0usize; old.len()];
        for (d, detection) in frame.detections.iter().enumerate() {
            for (t, track) in old.iter().enumerate() {
                if ms.saturating_sub(track.last.time.milliseconds()?)
                    <= u64::from(self.policy.maximum_gap_ms)
                    && track.observation_count < self.policy.max_observations_per_track
                    && track.last.detection.detector_generation == detection.detector_generation
                    && iou(track.last.detection.bounds, detection.bounds)
                        >= self.policy.association_iou
                {
                    edges[d].push(t);
                    degrees[t] += 1;
                }
            }
        }
        let mut used = BTreeSet::new();
        let mut observations = Vec::new();
        for (d, detection) in frame.detections.into_iter().enumerate() {
            let matching = edges[d]
                .first()
                .copied()
                .filter(|t| edges[d].len() == 1 && degrees[*t] == 1);
            let id = match matching {
                Some(t) => old[t].track_id.clone(),
                None => digest(&(
                    &next.media_sha256,
                    next.stream_index,
                    &next.policy_sha256,
                    &next.shot_anchor,
                    frame.time,
                    detection.source_index,
                ))?,
            };
            let observation = VideoObservation {
                observation_id: observation_id(&next, frame.time, &frame.frame_sha256, &detection)?,
                track_id: id.clone(),
                policy_sha256: next.policy_sha256.clone(),
                shot_anchor: next.shot_anchor.clone(),
                stream_index: frame.stream_index,
                playback_origin: frame.playback_origin,
                time: frame.time,
                frame_sha256: frame.frame_sha256.clone(),
                detection,
            };
            let mut track = match matching {
                Some(t) => {
                    used.insert(t);
                    old[t].clone()
                }
                None => VideoTrack {
                    track_id: id,
                    shot_anchor: next.shot_anchor.clone(),
                    first: observation.clone(),
                    last: observation.clone(),
                    observation_count: 0,
                    exemplars: Vec::new(),
                },
            };
            track.last = observation.clone();
            track.observation_count += 1;
            select_exemplar(&self.policy, &mut track, &observation);
            next.active.push(track);
            observations.push(observation);
        }
        // A missing or ambiguous track closes immediately: re-entry starts anew.
        for (index, track) in old.into_iter().enumerate() {
            if !used.contains(&index) {
                closed_tracks.push(track);
            }
        }
        next.last_time = Some(frame.time);
        next.sample_count += 1;
        self.state = next;
        Ok(VideoUpdate {
            observations,
            closed_tracks,
        })
    }
    pub fn finish(&mut self) -> Vec<VideoTrack> {
        std::mem::take(&mut self.state.active)
    }
}

fn observation_id(
    state: &VideoCheckpoint,
    time: VideoTime,
    frame: &str,
    detection: &VideoDetection,
) -> Result<String, String> {
    digest(&(
        &state.media_sha256,
        state.stream_index,
        &state.policy_sha256,
        &state.shot_anchor,
        state.playback_origin,
        time,
        frame,
        detection,
    ))
}
fn select_exemplar(policy: &VideoPolicy, track: &mut VideoTrack, observation: &VideoObservation) {
    if observation.detection.quality < policy.minimum_quality {
        return;
    }
    track.exemplars.push(observation.clone());
    track.exemplars.sort_by(|a, b| {
        b.detection
            .quality
            .total_cmp(&a.detection.quality)
            .then_with(|| {
                if a.time.after(b.time) {
                    std::cmp::Ordering::Greater
                } else if b.time.after(a.time) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| a.observation_id.cmp(&b.observation_id))
    });
    let mut poses = BTreeSet::new();
    track
        .exemplars
        .retain(|e| poses.insert(e.detection.pose_bucket.clone()));
    track.exemplars.truncate(policy.max_exemplars);
}
fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let intersection = ((a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0])).max(0.0)
        * ((a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1])).max(0.0);
    intersection / (a[2] * a[3] + b[2] * b[3] - intersection)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_negative_playback_delta_is_rejected_before_rounding() {
        let origin = VideoTime {
            pts: 10,
            numerator: 1,
            denominator: 10_000,
        };
        assert!(VideoTime { pts: 9, ..origin }
            .playback_milliseconds(origin)
            .is_err());
        assert_eq!(origin.playback_milliseconds(origin).unwrap(), 0);
    }
    fn frame(pts: i64) -> VideoFrame {
        VideoFrame {
            stream_index: 0,
            playback_origin: VideoTime::default(),
            time: VideoTime {
                pts,
                numerator: 1,
                denominator: 1000,
            },
            frame_sha256: format!("{:064x}", pts),
            scene_score: 0.0,
            detections: vec![VideoDetection {
                source_index: 0,
                bounds: [0.1, 0.1, 0.3, 0.3],
                quality: 0.8,
                pose_bucket: "front".into(),
                detector_generation: "yunet-v1".into(),
            }],
        }
    }
    fn tracker() -> VideoTracker {
        VideoTracker::new("a".repeat(64), 0, VideoPolicy::default()).unwrap()
    }
    #[test]
    fn wp086_equal_quality_pose_keeps_earliest_exemplar_across_restart() {
        let mut original = tracker();
        let first = original.ingest(frame(0)).unwrap().observations.remove(0);
        let mut restored = VideoTracker::restore(
            VideoPolicy::default(),
            serde_json::from_slice(&serde_json::to_vec(&original.checkpoint()).unwrap()).unwrap(),
        )
        .unwrap();
        let mut later_smaller_hash = false;
        for sample in 1..=32 {
            let update = original.ingest(frame(sample * 500)).unwrap();
            later_smaller_hash |= update.observations[0].observation_id < first.observation_id;
            assert_eq!(update, restored.ingest(frame(sample * 500)).unwrap());
            assert_eq!(original.checkpoint(), restored.checkpoint());
            assert_eq!(
                original.checkpoint().active[0].exemplars,
                vec![first.clone()]
            );
        }
        assert!(
            later_smaller_hash,
            "fixture must expose the old hash-based replacement"
        );
        let mut improved = frame(16_500);
        improved.detections[0].quality = 0.9;
        let better = original.ingest(improved).unwrap().observations.remove(0);
        assert_eq!(original.checkpoint().active[0].exemplars, vec![better]);
    }
    #[test]
    fn cuts_occlusion_reentry_and_density() {
        let mut t = tracker();
        let first = t.ingest(frame(0)).unwrap().observations[0].track_id.clone();
        assert_eq!(
            t.ingest(frame(500)).unwrap().observations[0].track_id,
            first
        );
        let mut empty = frame(1000);
        empty.detections.clear();
        let ended = t.ingest(empty).unwrap();
        assert_eq!(ended.closed_tracks[0].observation_count, 2);
        assert_eq!(ended.closed_tracks[0].density_weight(), 1);
        let reentry = t.ingest(frame(1500)).unwrap().observations[0]
            .track_id
            .clone();
        assert_ne!(first, reentry);
        let mut cut = frame(1600);
        cut.scene_score = 1.0;
        let update = t.ingest(cut).unwrap();
        assert_eq!(update.closed_tracks[0].track_id, reentry);
        assert_ne!(update.observations[0].track_id, reentry);
    }
    #[test]
    fn ambiguous_crossing_terminates_old_track() {
        let mut t = tracker();
        t.ingest(frame(0)).unwrap();
        let mut crossing = frame(500);
        let mut second = crossing.detections[0].clone();
        second.source_index = 1;
        crossing.detections.push(second);
        let update = t.ingest(crossing).unwrap();
        assert_eq!(update.closed_tracks.len(), 1);
        assert_ne!(
            update.observations[0].track_id,
            update.observations[1].track_id
        );
        assert!(update
            .observations
            .iter()
            .all(|o| o.track_id != update.closed_tracks[0].track_id));
    }
    #[test]
    fn capped_pose_exemplars_and_restart_are_exact() {
        let policy = VideoPolicy {
            max_exemplars: 2,
            ..VideoPolicy::default()
        };
        let mut t = VideoTracker::new("a".repeat(64), 0, policy.clone()).unwrap();
        for n in 0..4 {
            let mut f = frame(n * 500);
            f.detections[0].pose_bucket = format!("pose-{n}");
            f.detections[0].quality = 0.6 + n as f32 * 0.1;
            t.ingest(f).unwrap();
        }
        assert_eq!(t.checkpoint().active[0].exemplars.len(), 2);
        assert_eq!(
            t.checkpoint().active[0]
                .exemplars
                .iter()
                .map(|e| e.detection.pose_bucket.as_str())
                .collect::<Vec<_>>(),
            vec!["pose-3", "pose-2"]
        );
        let bytes = serde_json::to_vec(&t.checkpoint()).unwrap();
        let mut restored =
            VideoTracker::restore(policy, serde_json::from_slice(&bytes).unwrap()).unwrap();
        assert_eq!(
            t.ingest(frame(2000)).unwrap(),
            restored.ingest(frame(2000)).unwrap()
        );
        assert_eq!(t.checkpoint(), restored.checkpoint());
    }
    #[test]
    fn rejects_bad_pts_bounds_and_checkpoint_without_advancing() {
        let mut t = tracker();
        t.ingest(frame(0)).unwrap();
        let before = t.checkpoint();
        assert!(t.ingest(frame(0)).is_err());
        let mut bad = frame(500);
        bad.time.denominator = 0;
        assert!(t.ingest(bad).is_err());
        let mut bad = frame(500);
        bad.detections[0].bounds[0] = f32::NAN;
        assert!(t.ingest(bad).is_err());
        assert_eq!(t.checkpoint(), before);
        let mut bad = before;
        bad.active[0].track_id = "forged".into();
        assert!(VideoTracker::restore(VideoPolicy::default(), bad).is_err());
        assert!(VideoTime {
            pts: i64::MAX,
            numerator: u32::MAX,
            denominator: 1
        }
        .milliseconds()
        .is_err());
        assert_eq!(
            VideoTime {
                pts: 3003,
                numerator: 1,
                denominator: 90_000
            }
            .milliseconds()
            .unwrap(),
            33
        );
    }

    #[test]
    fn sampling_and_resource_limits_never_silently_drop_evidence() {
        let policy = VideoPolicy {
            max_samples: 2,
            max_detections: 1,
            ..VideoPolicy::default()
        };
        let mut t = VideoTracker::new("a".repeat(64), 0, policy).unwrap();
        t.ingest(frame(0)).unwrap();
        assert!(!t.should_sample(frame(100).time, 0.0).unwrap());
        assert!(t.should_sample(frame(100).time, 1.0).unwrap());
        let before = t.checkpoint();
        let mut oversized = frame(500);
        oversized.detections.push(oversized.detections[0].clone());
        assert!(t.ingest(oversized).is_err());
        assert_eq!(t.checkpoint(), before);
        t.ingest(frame(500)).unwrap();
        let before = t.checkpoint();
        assert!(t.ingest(frame(1000)).is_err());
        assert_eq!(t.checkpoint(), before);
        assert_eq!(t.finish()[0].observation_count, 2);
    }
}
