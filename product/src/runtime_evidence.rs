//! Bounded numeric runtime evidence on one process-local monotonic clock.
use serde::Serialize;
use std::{collections::VecDeque, sync::OnceLock, time::Instant};

pub(crate) const CAPACITY: usize = 256;
pub(crate) const TIMESTAMP_SCOPE: &str = "monotonic_us_since_process_runtime_epoch";

pub(crate) struct RuntimeClock {
    pub(crate) epoch: Instant,
    pub(crate) id: String,
}

pub(crate) fn clock() -> &'static RuntimeClock {
    static CLOCK: OnceLock<RuntimeClock> = OnceLock::new();
    CLOCK.get_or_init(|| RuntimeClock {
        epoch: Instant::now(),
        id: uuid::Uuid::new_v4().to_string(),
    })
}

pub(crate) fn timestamp(now: Instant) -> Option<u64> {
    now.checked_duration_since(clock().epoch)
        .and_then(|value| u64::try_from(value.as_micros()).ok())
}

#[derive(Clone, Debug, Serialize)]
struct Record<T> {
    sequence: u64,
    timestamp_us: u64,
    #[serde(flatten)]
    data: T,
}

#[derive(Clone, Debug)]
pub(crate) struct EvidenceRing<T> {
    lifetime_id: String,
    scope: &'static str,
    sequence: u64,
    dropped: u64,
    overflow: bool,
    samples: VecDeque<Record<T>>,
}

impl<T: Serialize> EvidenceRing<T> {
    pub(crate) fn new(scope: &'static str) -> Self {
        let _ = clock();
        Self {
            lifetime_id: uuid::Uuid::new_v4().to_string(),
            scope,
            sequence: 0,
            dropped: 0,
            overflow: false,
            samples: VecDeque::with_capacity(CAPACITY),
        }
    }

    pub(crate) fn push(&mut self, data: T, now: Instant) {
        let Some(timestamp_us) = timestamp(now) else {
            self.overflow = true;
            return;
        };
        if self
            .samples
            .back()
            .is_some_and(|last| timestamp_us < last.timestamp_us)
        {
            self.overflow = true;
            return;
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.overflow = true;
            return;
        };
        self.sequence = sequence;
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
            self.dropped = self.dropped.checked_add(1).unwrap_or_else(|| {
                self.overflow = true;
                u64::MAX
            });
        }
        self.samples.push_back(Record {
            sequence,
            timestamp_us,
            data,
        });
    }

    pub(crate) fn snapshot(&self, now: Instant) -> serde_json::Value {
        let captured_at_us = timestamp(now);
        serde_json::json!({ "runtime_id": clock().id, "lifetime_id": self.lifetime_id,
            "endpoint_scope": self.scope, "captured_at_us": captured_at_us,
            "sequence": self.sequence, "dropped_records": self.dropped,
            "overflow": self.overflow || captured_at_us.is_none(), "samples": self.samples })
    }
}


// Parent transport spans contain child work after a validated reply. They do
// not identify kernel start/end or equate resource admission with execution.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkerControlObservation {
    event: &'static str,
    worker_id: Option<String>,
    operation_id: Option<String>,
    operation: &'static str,
    fence_sha256: Option<String>,
    admission_epoch: u64,
    previous_epoch: Option<u64>,
    transition_start_us: Option<u64>,
    parent_request_start_us: Option<u64>,
}

fn worker_control_ring() -> &'static std::sync::Mutex<EvidenceRing<WorkerControlObservation>> {
    static RING: OnceLock<std::sync::Mutex<EvidenceRing<WorkerControlObservation>>> = OnceLock::new();
    RING.get_or_init(|| std::sync::Mutex::new(EvidenceRing::new(
        "parent_fenced_transport_external_playback_fullscreen_CAS_and_owned_exit_observer_calls_excluding_exact_kernel_timing_operator_pause_and_unobserved_raw_job_reaper")))
}

pub(crate) fn note_worker_control(event: &'static str, worker_id: &str,
    operation_id: &str, operation: &'static str, fence: Option<&crate::match_worker::WorkerFence>, request_start: Option<Instant>) {
    use sha2::{Digest, Sha256};
    let fence_sha256 = fence.and_then(|value| serde_json::to_vec(value).ok())
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    if let Ok(mut ring) = worker_control_ring().lock() {
        ring.push(WorkerControlObservation { event, worker_id: Some(worker_id.into()),
            operation_id: (!operation_id.is_empty()).then(|| operation_id.into()), operation,
            fence_sha256, admission_epoch: fence.map_or(0, |value| value.admission_epoch),
            previous_epoch: None, transition_start_us: None,
            parent_request_start_us: request_start.and_then(timestamp) }, Instant::now());
    }
}

pub(crate) fn note_hold_transition(prior: u64, next: u64, start: Instant) {
    if let Ok(mut ring) = worker_control_ring().lock() {
        ring.push(WorkerControlObservation { event: "hold_transition", worker_id: None,
            operation_id: None, operation: "none", fence_sha256: None,
            admission_epoch: next, previous_epoch: Some(prior),
            transition_start_us: timestamp(start), parent_request_start_us: None }, Instant::now());
    }
}

pub(crate) fn worker_control_snapshot() -> Result<serde_json::Value, &'static str> {
    worker_control_ring().lock().map(|ring| ring.snapshot(Instant::now()))
        .map_err(|_| "worker control evidence lock poisoned")
}

#[derive(Clone, Debug)]
pub(crate) struct OperatorControlContext {
    pub(crate) action_id: Option<String>,
    pub(crate) requested_mode: &'static str,
    pub(crate) request_started: Instant,
    pub(crate) admission_epoch: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OperatorControlObservation {
    event: &'static str,
    action_id: Option<String>,
    requested_mode: &'static str,
    request_start_us: Option<u64>,
    transition_start_us: Option<u64>,
    previous_epoch: u64,
    next_epoch: u64,
    persisted_revision: Option<u64>,
}

pub(crate) fn operator_control_observation(
    event: &'static str,
    context: &OperatorControlContext,
    previous_epoch: u64,
    next_epoch: u64,
    transition_started: Option<Instant>,
    persisted_revision: Option<u64>,
) -> OperatorControlObservation {
    OperatorControlObservation {
        event,
        action_id: context.action_id.clone(),
        requested_mode: context.requested_mode,
        request_start_us: timestamp(context.request_started),
        transition_start_us: transition_started.and_then(timestamp),
        previous_epoch,
        next_epoch,
        persisted_revision,
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NativePlaybackObservation {
    pub(crate) poll_start_us: Option<u64>,
    pub(crate) poll_end_us: Option<u64>,
    pub(crate) status: crate::video_player::PlaybackStatus,
    pub(crate) native_player_present: bool,
    pub(crate) player_generation: u64,
    pub(crate) generation_overflow: bool,
    pub(crate) native_playing: bool,
    pub(crate) clock_available: bool,
    pub(crate) time_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_evidence_clock_and_bounded_loss_are_explicit() {
        let mut ring = EvidenceRing::new("numeric_test");
        for _ in 0..=CAPACITY {
            ring.push(serde_json::json!({"active": 1}), Instant::now());
        }
        let snapshot = ring.snapshot(Instant::now());
        assert_eq!(snapshot["runtime_id"], clock().id);
        assert_eq!(snapshot["sequence"], CAPACITY + 1);
        assert_eq!(snapshot["dropped_records"], 1);
        assert_eq!(snapshot["samples"].as_array().unwrap().len(), CAPACITY);
        assert_eq!(snapshot["samples"][0]["sequence"], 2);
        ring.push(
            serde_json::json!({"active": 0}),
            clock().epoch - std::time::Duration::from_micros(1),
        );
        assert_eq!(ring.snapshot(Instant::now())["overflow"], true);
    }
}
