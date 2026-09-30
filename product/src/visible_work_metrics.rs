//! WP-087 bounded, redacted runtime endpoint samples. No filesystem or worker work.
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    time::Instant,
};

pub(crate) const SAMPLE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct LatencySample {
    sequence: u64,
    start_us: u64,
    end_us: u64,
    duration_us: u64,
}

pub(crate) struct LatencySeries<K> {
    epoch: Instant,
    lifetime_id: String,
    endpoint_scope: &'static str,
    pending: HashMap<K, Instant>,
    sequence: u64,
    dropped_records: u64,
    abandoned: u64,
    overflow: bool,
    samples: VecDeque<LatencySample>,
}

impl<K: Eq + Hash> LatencySeries<K> {
    pub(crate) fn new(endpoint_scope: &'static str) -> Self {
        Self {
            epoch: crate::runtime_evidence::clock().epoch,
            lifetime_id: uuid::Uuid::new_v4().to_string(),
            endpoint_scope,
            pending: HashMap::with_capacity(SAMPLE_CAPACITY),
            sequence: 0,
            dropped_records: 0,
            abandoned: 0,
            overflow: false,
            samples: VecDeque::with_capacity(SAMPLE_CAPACITY),
        }
    }

    pub(crate) fn begin(&mut self, key: K, started: Instant) {
        if self.pending.contains_key(&key) {
            return;
        }
        if self.pending.len() >= SAMPLE_CAPACITY {
            self.overflow = true;
            return;
        }
        self.pending.insert(key, started);
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn complete(&mut self, key: &K, ended: Instant) -> bool {
        let Some(started) = self.pending.remove(key) else {
            return false;
        };
        let Some(start_us) = started
            .checked_duration_since(self.epoch)
            .and_then(|d| u64::try_from(d.as_micros()).ok())
        else {
            self.overflow = true;
            return false;
        };
        let Some(end_us) = ended
            .checked_duration_since(self.epoch)
            .and_then(|d| u64::try_from(d.as_micros()).ok())
        else {
            self.overflow = true;
            return false;
        };
        if end_us < start_us {
            self.overflow = true;
            return false;
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.overflow = true;
            return false;
        };
        self.sequence = sequence;
        if self.samples.len() == SAMPLE_CAPACITY {
            self.samples.pop_front();
            self.dropped_records = self.dropped_records.checked_add(1).unwrap_or_else(|| {
                self.overflow = true;
                u64::MAX
            });
        }
        self.samples.push_back(LatencySample {
            sequence,
            start_us,
            end_us,
            duration_us: end_us - start_us,
        });
        true
    }

    pub(crate) fn retain_pending(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let before = self.pending.len();
        self.pending.retain(|key, _| keep(key));
        let removed = (before - self.pending.len()) as u64;
        self.abandoned = self.abandoned.checked_add(removed).unwrap_or_else(|| {
            self.overflow = true;
            u64::MAX
        });
    }

    pub(crate) fn clear_pending(&mut self) {
        self.retain_pending(|_| false);
    }

    pub(crate) fn snapshot(&self, now: Instant) -> serde_json::Value {
        let captured_at_us = now
            .checked_duration_since(self.epoch)
            .and_then(|d| u64::try_from(d.as_micros()).ok());
        serde_json::json!({
            "runtime_id": crate::runtime_evidence::clock().id,
            "timestamp_scope": crate::runtime_evidence::TIMESTAMP_SCOPE,
            "lifetime_id": self.lifetime_id, "endpoint_scope": self.endpoint_scope,
            "captured_at_us": captured_at_us, "sequence": self.sequence,
            "dropped_records": self.dropped_records, "abandoned": self.abandoned,
            "overflow": self.overflow || captured_at_us.is_none(),
            "pending": self.pending.len(), "samples": self.samples,
        })
    }
}

pub(crate) type ThumbnailScope = (String, u64, crate::media_thumbs::ThumbKey);
pub(crate) type NavigationScope = (String, u64, usize);

pub(crate) struct VisibleMediaMetrics {
    pub(crate) thumbnail: LatencySeries<ThumbnailScope>,
    pub(crate) navigation: LatencySeries<NavigationScope>,
}

impl Default for VisibleMediaMetrics {
    fn default() -> Self {
        Self {
            thumbnail: LatencySeries::new(
                "visible_priority_request_to_texture_painted_excluding_backend_and_vsync",
            ),
            navigation: LatencySeries::new(
                "grid_navigation_to_target_tile_painted_excluding_backend_and_vsync",
            ),
        }
    }
}

pub(crate) struct SeekClockMetric {
    series: LatencySeries<u8>,
    target: Option<(i64, i64, Instant)>,
}

impl Default for SeekClockMetric {
    fn default() -> Self {
        Self {
            series: LatencySeries::new(
                "native_seek_request_to_raw_libvlc_clock_confirmation_excluding_presentation",
            ),
            target: None,
        }
    }
}

impl SeekClockMetric {
    pub(crate) fn begin(&mut self, baseline: i64, target: i64, started: Instant) {
        self.cancel();
        // A no-op or unavailable clock does not supply a measured seek.
        if baseline < 0 || target < 0 || target.abs_diff(baseline) < 1_000 {
            return;
        }
        self.target = Some((baseline, target, started));
        self.series.begin(0, started);
    }

    pub(crate) fn observe(&mut self, snapshot: &crate::video_player::Snapshot, now: Instant) {
        let Some((baseline, target, started)) = self.target else {
            return;
        };
        if snapshot.error.is_some() {
            self.cancel();
            return;
        }
        if !snapshot.confirmed
            || !matches!(
                snapshot.status,
                crate::video_player::PlaybackStatus::Playing
                    | crate::video_player::PlaybackStatus::Paused
            )
        {
            return;
        }
        let Some(elapsed) = now.checked_duration_since(started) else {
            self.series.overflow = true;
            self.cancel();
            return;
        };
        let progress = if snapshot.status == crate::video_player::PlaybackStatus::Playing {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        } else {
            0
        };
        let observed = snapshot.time_ms;
        let natural = baseline.saturating_add(progress);
        // Require a displaced raw native clock; natural forward progress alone
        // and the optimistic public Snapshot never confirm a seek.
        if observed >= target.saturating_sub(100)
            && observed <= target.saturating_add(progress).saturating_add(100)
            && observed.abs_diff(natural) > 500
        {
            self.series.complete(&0, now);
            self.target = None;
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.target = None;
        self.series.clear_pending();
    }

    pub(crate) fn snapshot(&self, now: Instant) -> serde_json::Value {
        self.series.snapshot(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wp087_visible_latency_requires_exact_pending_endpoint_and_preserves_first_start() {
        let mut series = LatencySeries::new("test");
        let start = series.epoch + Duration::from_micros(10);
        assert!(!series.complete(&"missing", start));
        series.begin("a", start);
        series.begin("a", start + Duration::from_micros(25));
        assert!(!series.complete(&"different", start + Duration::from_micros(100)));
        assert!(series.complete(&"a", start + Duration::from_micros(100)));
        assert!(!series.complete(&"a", start + Duration::from_micros(200)));
        let another = LatencySeries::<u8>::new("other");
        assert_eq!(series.epoch, another.epoch);
        assert_eq!(
            series.snapshot(start + Duration::from_micros(200))["runtime_id"],
            another.snapshot(start + Duration::from_micros(200))["runtime_id"]
        );
        assert_eq!(
            series.snapshot(start + Duration::from_micros(200))["samples"][0]["duration_us"],
            100
        );
    }

    #[test]
    fn wp087_visible_latency_bounded_history_reports_sequence_loss_and_redacts_keys() {
        let mut series = LatencySeries::new("test");
        let start = series.epoch;
        for i in 0..=SAMPLE_CAPACITY {
            let key = format!("PRIVATE-PERSON-crop-path-{i}");
            series.begin(key.clone(), start);
            assert!(series.complete(&key, start + Duration::from_micros(i as u64)));
        }
        let state = series.snapshot(start + Duration::from_secs(1));
        assert_eq!(state["sequence"], 257);
        assert_eq!(state["dropped_records"], 1);
        assert_eq!(state["samples"].as_array().unwrap().len(), SAMPLE_CAPACITY);
        assert_eq!(state["samples"][0]["sequence"], 2);
        assert!(!state.to_string().contains("PRIVATE"));
        let other = LatencySeries::<String>::new("test");
        assert_ne!(series.lifetime_id, other.lifetime_id);
    }

    #[test]
    fn wp087_visible_latency_invalid_clock_and_pending_overflow_fail_closed() {
        let mut series = LatencySeries::new("test");
        let start = series.epoch + Duration::from_micros(10);
        series.begin(0, start);
        assert!(!series.complete(&0, series.epoch));
        assert!(series.overflow);
        let mut series = LatencySeries::new("test");
        for i in 0..=SAMPLE_CAPACITY {
            series.begin(i, series.epoch);
        }
        assert_eq!(series.pending.len(), SAMPLE_CAPACITY);
        assert!(series.overflow);
        series.retain_pending(|key| *key == 0);
        assert_eq!(series.abandoned, 255);
        assert_eq!(series.pending.len(), 1);
    }

    #[test]
    fn wp087_native_seek_ignores_optimistic_or_naturally_advanced_clock() {
        use crate::video_player::{PlaybackStatus, Snapshot};
        let mut metric = SeekClockMetric::default();
        let start = metric.series.epoch;
        metric.begin(1_000, 10_000, start);
        let mut snapshot = Snapshot {
            path: "PRIVATE".into(),
            playing: true,
            time_ms: 10_000,
            length_ms: 20_000,
            volume: 100,
            audio_track: -1,
            subtitle_track: -1,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            looping: false,
            confirmed: false,
            status: PlaybackStatus::Playing,
            error: None,
        };
        metric.observe(&snapshot, start + Duration::from_millis(25));
        assert_eq!(metric.series.sequence, 0);
        snapshot.confirmed = true;
        metric.observe(&snapshot, start + Duration::from_secs(9));
        assert_eq!(metric.series.sequence, 0);
        metric.observe(&snapshot, start + Duration::from_millis(100));
        assert_eq!(metric.series.sequence, 1);
        assert_eq!(
            metric.snapshot(start + Duration::from_secs(10))["samples"][0]["duration_us"],
            100_000
        );
        assert!(!metric
            .snapshot(start + Duration::from_secs(10))
            .to_string()
            .contains("PRIVATE"));
        metric.begin(10_000, 10_001, start);
        assert!(metric.target.is_none());
        metric.begin(10_000, 1_000, start);
        snapshot.error = Some("PRIVATE-error".into());
        metric.observe(&snapshot, start + Duration::from_millis(100));
        assert_eq!(metric.series.sequence, 1);
        assert!(metric.target.is_none());
    }

    #[test]
    fn wp087_native_seek_zero_requires_an_available_raw_clock() {
        use crate::video_player::{PlaybackStatus, Snapshot};
        let mut metric = SeekClockMetric::default();
        let start = metric.series.epoch;
        metric.begin(10_000, 0, start);
        let mut snapshot = Snapshot {
            path: String::new(),
            playing: false,
            time_ms: 0,
            length_ms: 20_000,
            volume: 100,
            audio_track: -1,
            subtitle_track: -1,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            looping: false,
            confirmed: false,
            status: PlaybackStatus::Paused,
            error: None,
        };
        metric.observe(&snapshot, start + Duration::from_millis(10));
        assert_eq!(metric.series.sequence, 0);
        assert!(metric.target.is_some());
        snapshot.confirmed = true;
        metric.observe(&snapshot, start + Duration::from_millis(20));
        assert_eq!(metric.series.sequence, 1);
        // Bind the unavailable-clock producer to the tested negative consumer.
        let source = include_str!("video_player.rs");
        let native_start = source.find("mod windows_impl {").unwrap();
        let start = source[native_start..]
            .find(&format!(
                "pub fn {}(&mut self) -> Option<Snapshot>",
                "snapshot"
            ))
            .unwrap()
            + native_start;
        let body = &source[start
            ..source[start..]
                .find(&format!("fn {}(&self)", "playback_status"))
                .unwrap()
                + start];
        assert!(body.contains("time_ms: native_time_ms.max(0)"));
        assert!(body.contains("confirmed: native_time_ms >= 0"));
    }
}
