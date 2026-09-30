//! App-owned inference isolation. Only the supervisor may publish worker output.
//! The worker never loads application configuration or opens a database.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod preparation_tests;

pub(crate) const SAFE_UNIT_LIMIT: Duration = Duration::from_millis(2_000);
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_IMAGE: usize = 256 * 1024 * 1024;
const MAX_FACES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerFence {
    pub job_id: String,
    pub asset_id: String,
    pub media_key: String,
    pub schema_generation: String,
    pub identity_revision: u64,
    pub catalog_revision: u64,
    pub admission_epoch: u64,
    pub model_generation: String,
    pub media_fingerprint: String,
    pub track_id: Option<String>,
    pub timestamp_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerFace {
    pub bbox: [f32; 4],
    pub score: f32,
    pub landmarks: [[f32; 2]; 5],
    pub bbox_normalized: [f32; 4],
    pub landmarks_normalized: [[f32; 2]; 5],
    pub detection_score: f32,
    pub face_fraction: f32,
    pub alignment_valid: bool,
    pub embedding: Vec<f32>,
    pub embedding_dim: usize,
    pub generation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerFaceFailure {
    pub detection_index: usize,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerFaceBatch {
    pub image_w: u32,
    pub image_h: u32,
    pub faces: Vec<WorkerFace>,
    pub failures: Vec<WorkerFaceFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerDetection {
    pub source_index: usize,
    pub bbox: [f32; 4],
    pub score: f32,
    pub landmarks: [[f32; 2]; 5],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerDetectionBatch {
    pub image_w: u32,
    pub image_h: u32,
    pub faces: Vec<WorkerDetection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prepared {
    pub generation: String,
    pub embedding_dim: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseArrival {
    pub phase: crate::identity::PreparationPhase,
    pub elapsed_micros: u64,
}
struct PhaseTrace {
    started: Instant,
    arrivals: Vec<PhaseArrival>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_phase: Option<crate::identity::PreparationPhase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_arrivals: Vec<PhaseArrival>,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub quarantined: bool,
    pub worker_id: String,
    pub model_generation: String,
}

impl WorkerError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            last_phase: None,
            phase_arrivals: Vec::new(),
            code: code.into(),
            message: message.into(),
            retryable: false,
            quarantined: false,
            worker_id: String::new(),
            model_generation: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
enum Operation {
    CpuExecutorBegin,
    ProductionCpuExecutorBegin,
    CandidateBootstrapBegin {
        root: PathBuf,
        pool_limit_bytes: u64,
    },
    CandidateBootstrapStep,
    CandidateManagedGpuPeaks,
    CandidatePoolBoundary {
        step: u8,
    },
    PreparationBegin {
        manifest: PathBuf,
        runtime: crate::match_acceleration::ProbeRuntime,
        candidate: bool,
    },
    PreparationStep,
    InspectPinnedImage {
        source_path: PathBuf,
    },
    EmbedPinnedImage {
        source_path: PathBuf,
    },
    CandidatePrepare {
        manifest: PathBuf,
        runtime: crate::match_acceleration::ProbeRuntime,
    },
    CandidateSample {
        #[serde(skip)]
        encoded: Vec<u8>,
        runtime: crate::match_acceleration::ProbeRuntime,
    },
    Prepare {
        manifest: PathBuf,
    },
    Embed {
        #[serde(skip)]
        encoded: Vec<u8>,
        source_path: PathBuf,
    },
    Detect {
        #[serde(skip)]
        encoded: Vec<u8>,
        source_path: PathBuf,
    },
    EmbedVideoExemplar {
        #[serde(skip)]
        encoded: Vec<u8>,
        source_path: PathBuf,
        detection_index: usize,
    },
    Decode {
        source_path: PathBuf,
        requested_ms: u64,
        stream_index: u32,
    },
    DecodeExact {
        source_path: PathBuf,
        time: crate::match_video::VideoTime,
        stream_index: u32,
    },
    DiscoveryBegin {
        root: PathBuf,
        exclusions: Vec<String>,
    },
    DiscoveryNext,
    DiscoveryMetadata {
        path: PathBuf,
    },
    FingerprintBeginForPlayback {
        source_path: PathBuf,
        expected_root: PathBuf,
    },
    FingerprintBegin {
        source_path: PathBuf,
        expected_root: PathBuf,
    },
    FingerprintStep,
    // Accepted only by a debug-build worker explicitly started as a harness.
    Harness {
        fault: HarnessFault,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum HarnessFault {
    Hang,
    Crash,
    LateFence,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: u32,
    worker_id: String,
    operation_id: String,
    fence: WorkerFence,
    body: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value")]
enum Output {
    CpuExecutorReady {
        threads: usize,
    },
    CandidatePoolBoundary {
        step: u8,
        peaks: Option<crate::match_cuda_bootstrap::ManagedGpuPeaks>,
    },
    CandidateManagedGpuPeaks(Option<crate::match_cuda_bootstrap::ManagedGpuPeaks>),
    CandidateBootstrapCheckpoint,
    CandidateBootstrapReady(crate::match_cuda_bootstrap::NativeProvenance),
    PreparationCheckpoint {
        generation: String,
    },
    PreparationProgress(crate::identity::PreparationPhase),
    ImageInfo(WorkerImageInfo),
    Prepared(Prepared),
    Faces(WorkerFaceBatch),
    Detections(WorkerDetectionBatch),
    Decoded(Option<crate::match_video_decode::DecodedVideoSample>),
    DiscoveryReady,
    DiscoveryStep(crate::match_discovery::Step),
    DiscoveryMetadata(Result<u64, String>),
    SourceReady,
    PlaybackSource(crate::match_video_decode::PlaybackSourceHandles),
    SourceProgress(crate::match_video_decode::VideoSourceProgress),
    CandidatePrepared(crate::match_acceleration::ProbePrepared),
    CandidateFrame(crate::match_acceleration::ProbeFrame),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerImageInfo {
    pub file_size: u64,
    pub working_bytes: u64,
    pub exif_orientation: u8,
    pub image_w: u32,
    pub image_h: u32,
}

fn valid_image_info(info: &WorkerImageInfo) -> bool {
    info.file_size > 0
        && info.file_size <= MAX_IMAGE as u64
        && info.image_w > 0
        && info.image_h > 0
        && (1..=8).contains(&info.exif_orientation)
        && info.working_bytes <= MAX_IMAGE as u64
        && u64::from(info.image_w)
            .checked_mul(u64::from(info.image_h))
            .and_then(|v| v.checked_mul(24))
            == Some(info.working_bytes)
}

fn inspect_image_bytes(bytes: &[u8]) -> Result<WorkerImageInfo, WorkerError> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| WorkerError::new("image_decode_failed", "pinned image format invalid"))?;
    let (image_w, image_h) = reader
        .into_dimensions()
        .map_err(|_| WorkerError::new("image_decode_failed", "pinned image header invalid"))?;
    let working_bytes = u64::from(image_w)
        .checked_mul(u64::from(image_h))
        .and_then(|v| v.checked_mul(24))
        .ok_or_else(|| {
            WorkerError::new("worker_decode_limit", "pinned image dimensions overflow")
        })?;
    let info = WorkerImageInfo {
        file_size: bytes.len() as u64,
        working_bytes,
        image_w,
        image_h,
        exif_orientation: u8::try_from(crate::media_thumbs::exif_orientation(bytes)).unwrap_or(1),
    };
    if !valid_image_info(&info) {
        return Err(WorkerError::new(
            "worker_decode_limit",
            "pinned image exceeds working budget",
        ));
    }
    Ok(info)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    protocol: u32,
    worker_id: String,
    operation_id: String,
    fence: WorkerFence,
    output: Result<Output, WorkerError>,
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("worker frame exceeds byte limit".into());
    }
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer.write_all(&bytes).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> Result<T, String> {
    let mut header = [0; 4];
    reader.read_exact(&mut header).map_err(|e| e.to_string())?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("invalid worker frame length".into());
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn write_request(writer: &mut impl Write, request: &Request) -> Result<(), String> {
    write_frame(writer, request)?;
    if let Operation::Embed { encoded, .. }
    | Operation::Detect { encoded, .. }
    | Operation::EmbedVideoExemplar { encoded, .. }
    | Operation::CandidateSample { encoded, .. } = &request.body
    {
        if encoded.len() > MAX_IMAGE {
            return Err("worker input exceeds source byte limit".into());
        }
        writer
            .write_all(&(encoded.len() as u32).to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer.write_all(encoded).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn read_request(reader: &mut impl Read) -> Result<Request, String> {
    let mut request: Request = read_frame(reader)?;
    if let Operation::Embed { encoded, .. }
    | Operation::Detect { encoded, .. }
    | Operation::EmbedVideoExemplar { encoded, .. }
    | Operation::CandidateSample { encoded, .. } = &mut request.body
    {
        let mut header = [0; 4];
        reader.read_exact(&mut header).map_err(|e| e.to_string())?;
        let length = u32::from_le_bytes(header) as usize;
        if length > MAX_IMAGE {
            return Err("worker input exceeds source byte limit".into());
        }
        encoded.resize(length, 0);
        reader.read_exact(encoded).map_err(|e| e.to_string())?;
    }
    Ok(request)
}

fn worker_executable(current: &Path, test_binary: bool) -> Result<PathBuf, WorkerError> {
    if !test_binary {
        return Ok(current.to_path_buf());
    }
    let parent = current
        .parent()
        .ok_or_else(|| WorkerError::new("worker_spawn_failed", "executable has no parent"))?;
    let parent = if parent.file_name().is_some_and(|name| name == "deps") {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    };
    Ok(parent.join(if cfg!(windows) {
        "facial-cli.exe"
    } else {
        "facial-cli"
    }))
}

/// One serial child; after a transport failure or timeout it is never reusable.
/// Caller retains all admission/revision decisions and Background permits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CpuExecutionPolicy {
    #[default]
    Baseline,
    PrivateTwoThread,
}

pub(crate) type OwnedWorkerExitObserver = Box<dyn Fn() -> Result<bool, String> + Send>;
impl CpuExecutionPolicy {
    pub(crate) fn active_units(self) -> u64 {
        match self {
            Self::Baseline => 1,
            Self::PrivateTwoThread => 2,
        }
    }
}

pub(crate) struct IsolatedMatchWorker {
    cpu_policy: CpuExecutionPolicy,
    cpu_two_admitted: bool,
    cpu_executor_init_micros: Option<u64>,
    worker_id: String,
    process: owned_process::Process,
    requests: Option<mpsc::SyncSender<Request>>,
    responses: mpsc::Receiver<Result<Response, String>>,
    discovery_resources: Option<crate::match_store::MatchResourceLease>,
    preparation_resources: Option<crate::match_store::MatchResourceLease>,
    quarantined: bool,
    prepared: Option<Prepared>,
    deadline: Option<Instant>,
    last_phase: std::sync::Arc<std::sync::Mutex<PhaseTrace>>,
}

impl Drop for IsolatedMatchWorker {
    fn drop(&mut self) {
        if !self.shutdown_and_confirm() {
            let leases = [
                self.discovery_resources.take(),
                self.preparation_resources.take(),
            ]
            .into_iter()
            .flatten()
            .collect();
            self.process.retain_resources_until_exit(leases);
        }
    }
}

impl IsolatedMatchWorker {
    #[cfg(test)]
    pub(crate) fn spawn() -> Result<Self, WorkerError> {
        let current = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        Self::spawn_at(&worker_executable(&current, cfg!(test))?, false)
    }

    pub(crate) fn spawn_diagnostic_preparation() -> Result<Self, WorkerError> {
        let current = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        Self::spawn_diagnostic_at(
            &worker_executable(&current, cfg!(test))?,
            false,
            crate::identity::PREPARATION_BUFFER_BYTES,
            1,
        )
    }

    #[cfg(test)]
    fn spawn_at(executable: &Path, harness: bool) -> Result<Self, WorkerError> {
        Self::spawn_diagnostic_at(executable, harness, 0, 0)
    }

    fn spawn_diagnostic_at(
        executable: &Path,
        harness: bool,
        preparation_bytes: u64,
        cpu_units: u64,
    ) -> Result<Self, WorkerError> {
        static GOVERNOR: std::sync::OnceLock<crate::match_store::MatchResourceGovernor> =
            std::sync::OnceLock::new();
        let governor = GOVERNOR.get_or_init(|| {
            crate::match_store::MatchResourceGovernor::new(Default::default())
                .expect("valid worker diagnostic governor")
        });
        let cap = crate::match_store::WORKER_MEMORY_LIMIT_BYTES;
        if cap == 0 {
            return Err(WorkerError::new(
                "worker_memory_policy_unset",
                "measured memory policy required",
            ));
        }
        let lease = governor
            .try_acquire(crate::match_store::ResourceRequest {
                queued_bytes: preparation_bytes,
                cpu_inference: cpu_units,
                worker_memory_bytes: cap,
                ..Default::default()
            })
            .map_err(|e| WorkerError::new("resource_pressure", e))?;
        Self::spawn_at_with_resources(executable, harness, lease)
    }

    pub(crate) fn cpu_executor_init_micros(&self) -> Option<u64> {
        self.cpu_executor_init_micros
    }

    pub(crate) fn spawn_cpu_two_thread_candidate() -> Result<Self, WorkerError> {
        let current = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        let mut worker = Self::spawn_diagnostic_at(
            &worker_executable(&current, cfg!(test))?,
            false,
            crate::identity::PREPARATION_BUFFER_BYTES,
            2,
        )?;
        worker.cpu_two_admitted = true;
        Ok(worker)
    }

    pub(crate) fn spawn_with_resources(
        resources: crate::match_store::MatchResourceLease,
        prior: Option<&Self>,
    ) -> Result<Self, WorkerError> {
        if prior.is_some_and(|worker| !worker.quarantined || !worker.confirmed_dead()) {
            return Err(WorkerError::new(
                "worker_retry_not_ready",
                "previous worker exit unconfirmed",
            ));
        }
        let current = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        Self::spawn_at_with_resources(&worker_executable(&current, cfg!(test))?, false, resources)
    }

    pub(crate) fn spawn_cuda_candidate(root: &Path) -> Result<Self, WorkerError> {
        if !root.is_absolute() || root.as_os_str().len() > 16000 {
            return Err(WorkerError::new(
                "candidate_root_invalid",
                "absolute bounded candidate root required",
            ));
        }
        static GOVERNOR: std::sync::OnceLock<crate::match_store::MatchResourceGovernor> =
            std::sync::OnceLock::new();
        let governor = GOVERNOR.get_or_init(|| {
            crate::match_store::MatchResourceGovernor::new(Default::default())
                .expect("valid standalone candidate governor")
        });
        let lease = governor
            .try_acquire(crate::match_store::ResourceRequest {
                queued_bytes: crate::identity::PREPARATION_BUFFER_BYTES,
                worker_memory_bytes: crate::match_store::WORKER_MEMORY_LIMIT_BYTES,
                gpu_vram_bytes: crate::match_store::ResourceBudget::default().gpu_vram_bytes,
                ..Default::default()
            })
            .map_err(|e| WorkerError::new("resource_pressure", e))?;
        let current = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        Self::spawn_at_with_candidate_resources(
            &worker_executable(&current, cfg!(test))?,
            false,
            lease,
            Some(root),
        )
    }
    fn spawn_at_with_resources(
        executable: &Path,
        harness: bool,
        resources: crate::match_store::MatchResourceLease,
    ) -> Result<Self, WorkerError> {
        Self::spawn_at_with_candidate_resources(executable, harness, resources, None)
    }
    fn spawn_at_with_candidate_resources(
        executable: &Path,
        harness: bool,
        resources: crate::match_store::MatchResourceLease,
        candidate_root: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let worker_id = uuid::Uuid::new_v4().to_string();
        let (process, mut input, mut output) =
            owned_process::spawn(executable, &worker_id, harness, resources, candidate_root)
                .map_err(|e| WorkerError::new("worker_spawn_failed", e))?;
        let (send, receive) = mpsc::sync_channel::<Request>(1);
        let (result_send, responses) = mpsc::sync_channel(1);
        let last_phase = std::sync::Arc::new(std::sync::Mutex::new(PhaseTrace {
            started: Instant::now(),
            arrivals: Vec::with_capacity(16),
        }));
        let transport_phase = std::sync::Arc::clone(&last_phase);
        std::thread::Builder::new()
            .name("match-worker-transport".into())
            .spawn(move || {
                while let Ok(request) = receive.recv() {
                    let result = write_request(&mut input, &request).and_then(|()| {
                        for _ in 0..16 {
                            let response: Response = read_frame(&mut output)?;
                            if let Ok(Output::PreparationProgress(phase)) = &response.output {
                                if response.protocol != request.protocol
                                    || response.worker_id != request.worker_id
                                    || response.operation_id != request.operation_id
                                    || response.fence != request.fence
                                {
                                    return Err("worker progress fence mismatch".into());
                                }
                                if !matches!(
                                    request.body,
                                    Operation::CandidatePrepare { .. } | Operation::PreparationStep
                                ) {
                                    return Err("unexpected preparation progress".into());
                                }
                                let mut trace = transport_phase
                                    .lock()
                                    .map_err(|_| "worker phase lock poisoned")?;
                                let elapsed_micros = trace
                                    .started
                                    .elapsed()
                                    .as_micros()
                                    .min(u128::from(u64::MAX))
                                    as u64;
                                if trace.arrivals.len() == 16 {
                                    return Err("worker preparation progress exceeded bound".into());
                                }
                                trace.arrivals.push(PhaseArrival {
                                    phase: *phase,
                                    elapsed_micros,
                                });
                            } else {
                                return Ok(response);
                            }
                        }
                        Err("worker preparation progress exceeded bound".into())
                    });
                    let failed = result.is_err();
                    if result_send.send(result).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        Ok(Self {
            cpu_policy: CpuExecutionPolicy::Baseline,
            cpu_two_admitted: false,
            cpu_executor_init_micros: None,
            worker_id,
            process,
            requests: Some(send),
            responses,
            discovery_resources: None,
            preparation_resources: None,
            quarantined: false,
            prepared: None,
            deadline: None,
            last_phase,
        })
    }

    pub(crate) fn candidate_gpu_peaks(
        &mut self,
        fence: &WorkerFence,
    ) -> Result<Option<crate::match_cuda_bootstrap::ManagedGpuPeaks>, WorkerError> {
        match self.execute(Operation::CandidateManagedGpuPeaks, fence)? {
            Output::CandidateManagedGpuPeaks(value)
                if value.as_ref().is_none_or(|v| {
                    v.limit_bytes == crate::match_store::ResourceBudget::default().gpu_vram_bytes
                        && v.used_high_bytes <= v.reserved_high_bytes
                        && v.reserved_high_bytes <= v.limit_bytes
                }) =>
            {
                Ok(value)
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "candidate_managed_pool_output",
                    "invalid managed pool counters",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn bootstrap_candidate(
        &mut self,
        root: &Path,
        fence: &WorkerFence,
    ) -> Result<crate::match_cuda_bootstrap::NativeProvenance, WorkerError> {
        self.bootstrap_candidate_limit(
            root,
            crate::match_store::ResourceBudget::default().gpu_vram_bytes,
            fence,
        )
    }
    fn bootstrap_candidate_limit(
        &mut self,
        root: &Path,
        pool_limit_bytes: u64,
        fence: &WorkerFence,
    ) -> Result<crate::match_cuda_bootstrap::NativeProvenance, WorkerError> {
        if !matches!(
            self.execute(
                Operation::CandidateBootstrapBegin {
                    root: root.to_path_buf(),
                    pool_limit_bytes
                },
                fence
            )?,
            Output::CandidateBootstrapCheckpoint
        ) {
            return Err(self.quarantine(
                WorkerError::new("candidate_bootstrap_protocol", "unexpected bootstrap begin"),
                fence,
            ));
        }
        for _ in 0..crate::match_cuda_bootstrap::MAX_STEPS {
            match self.execute(Operation::CandidateBootstrapStep, fence) {
                Ok(Output::CandidateBootstrapCheckpoint) => {}
                Ok(Output::CandidateBootstrapReady(info))
                    if info.payload_sha256 == crate::match_cuda_bootstrap::PAYLOAD_SHA256
                        && info.device_uuid.len() == 32
                        && info.cache_scope.len() <= 256 =>
                {
                    return Ok(info)
                }
                Ok(_) => {
                    return Err(self.quarantine(
                        WorkerError::new(
                            "candidate_bootstrap_protocol",
                            "unexpected bootstrap output",
                        ),
                        fence,
                    ))
                }
                Err(error) => return Err(self.quarantine(error, fence)),
            }
        }
        Err(self.quarantine(
            WorkerError::new(
                "candidate_bootstrap_step_limit",
                "candidate bootstrap incomplete",
            ),
            fence,
        ))
    }
    pub(crate) fn worker_id(&self) -> &str {
        &self.worker_id
    }
    pub(crate) fn confirmed_dead(&self) -> bool {
        self.process.confirmed_dead()
    }
    /// Retain observation handles for this exact owned process and Job. This
    /// does not terminate a process or infer exit from elapsed time.
    pub(crate) fn owned_exit_observer(&self) -> Result<OwnedWorkerExitObserver, WorkerError> {
        self.process
            .owned_exit_observer()
            .map_err(|error| WorkerError::new("worker_exit_observer_failed", error))
    }
    pub(crate) fn cpu_policy(&self) -> CpuExecutionPolicy {
        self.cpu_policy
    }
    pub(crate) fn initialize_production_cpu(
        &mut self,
        policy: CpuExecutionPolicy,
        permit: &crate::match_store::MatchComputePermit,
        fence: &WorkerFence,
    ) -> Result<(), WorkerError> {
        if permit.cpu_units() != policy.active_units()
            || permit.admission_epoch() != fence.admission_epoch
        {
            return Err(WorkerError::new(
                "resource_pressure",
                "exact CPU policy admission required",
            ));
        }
        if policy == CpuExecutionPolicy::Baseline {
            return Ok(());
        }
        if self.cpu_policy != CpuExecutionPolicy::Baseline
            || self.cpu_two_admitted
            || self.prepared.is_some()
        {
            return Err(WorkerError::new(
                "worker_not_fresh",
                "production CPU policy requires fresh worker",
            ));
        }
        match self.execute(Operation::ProductionCpuExecutorBegin, fence)? {
            Output::CpuExecutorReady { threads: 2 } => {
                self.cpu_policy = policy;
                Ok(())
            }
            _ => Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "production CPU executor invalid"),
                fence,
            )),
        }
    }

    pub(crate) fn is_prepared_for(&self, generation: &str) -> bool {
        !self.quarantined
            && self
                .prepared
                .as_ref()
                .is_some_and(|prepared| prepared.generation == generation)
    }

    #[cfg(all(test, windows, debug_assertions))]
    pub(crate) fn spawn_fault_harness() -> Result<Self, WorkerError> {
        let executable = std::env::current_exe()
            .map_err(|e| WorkerError::new("worker_spawn_failed", e.to_string()))?;
        let binary = executable
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| {
                WorkerError::new("worker_spawn_failed", "test executable parent missing")
            })?
            .join("facial-cli.exe");
        Self::spawn_at(&binary, true)
    }

    #[cfg(all(test, windows, debug_assertions))]
    pub(crate) fn exercise_fault(
        &mut self,
        fault: HarnessFault,
        fence: &WorkerFence,
    ) -> Result<(), WorkerError> {
        self.execute(Operation::Harness { fault }, fence)
            .map(|_| ())
    }

    /// Caller must first authorize retry in the durable job policy. This check
    /// prevents a second process consuming resources beside a timed-out child.
    #[cfg(test)]
    pub(crate) fn spawn_retry(prior: &mut Self) -> Result<Self, WorkerError> {
        if !prior.quarantined || !prior.confirmed_dead() {
            return Err(WorkerError::new(
                "worker_retry_not_ready",
                "retry requires a quarantined worker with confirmed exit",
            ));
        }
        prior.release_dead_preparation_resources();
        Self::spawn()
    }

    /// Used by bounded diagnostic coordinators before admitting another backend.
    pub(crate) fn terminate_owned(&mut self) {
        self.requests.take();
        self.prepared = None;
        self.quarantined = true;
        self.process.terminate();
    }

    pub(crate) fn shutdown_and_confirm(&mut self) -> bool {
        self.terminate_owned();
        let deadline = Instant::now() + Duration::from_millis(2000);
        while !self.confirmed_dead() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.confirmed_dead()
    }

    pub(crate) fn memory_peaks(&self) -> Result<(u64, u64), &'static str> {
        self.process
            .peaks()
            .map(|(process, job)| (process as u64, job as u64))
            .map_err(|_| "worker_memory_query_failed")
    }

    pub(crate) fn phase_arrivals(&self) -> Vec<PhaseArrival> {
        self.last_phase
            .lock()
            .map(|trace| trace.arrivals.clone())
            .unwrap_or_default()
    }
    fn quarantine(&mut self, mut error: WorkerError, fence: &WorkerFence) -> WorkerError {
        error.phase_arrivals = self.phase_arrivals();
        error.last_phase = error.phase_arrivals.last().map(|arrival| arrival.phase);
        self.quarantined = true;
        self.prepared = None;
        self.requests.take();
        self.process.terminate();
        error.retryable = true;
        error.quarantined = true;
        error.worker_id = self.worker_id.clone();
        error.model_generation = fence.model_generation.clone();
        error
    }

    fn execute(&mut self, body: Operation, fence: &WorkerFence) -> Result<Output, WorkerError> {
        if self.quarantined {
            return Err(self.quarantine(
                WorkerError::new(
                    "worker_quarantined",
                    "fresh worker required after confirmed exit",
                ),
                fence,
            ));
        }
        let start = Instant::now();
        if let Ok(mut phase) = self.last_phase.lock() {
            phase.started = start;
            phase.arrivals.clear();
        }
        self.deadline = Some(start + SAFE_UNIT_LIMIT);
        let operation_id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            protocol: 1,
            worker_id: self.worker_id.clone(),
            operation_id: operation_id.clone(),
            fence: fence.clone(),
            body,
        };
        if self
            .requests
            .as_ref()
            .is_none_or(|send| send.try_send(request).is_err())
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "worker_transport_failed",
                    "worker request channel unavailable",
                ),
                fence,
            ));
        }
        let response = self
            .responses
            .recv_timeout(SAFE_UNIT_LIMIT.saturating_sub(start.elapsed()));
        if start.elapsed() >= SAFE_UNIT_LIMIT {
            return Err(self.quarantine(
                WorkerError::new("safe_unit_timeout", "Match safe unit exceeded 2000 ms"),
                fence,
            ));
        }
        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(message)) => {
                return Err(
                    self.quarantine(WorkerError::new("worker_transport_failed", message), fence)
                )
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(self.quarantine(
                    WorkerError::new("safe_unit_timeout", "Match safe unit exceeded 2000 ms"),
                    fence,
                ))
            }
            Err(error) => {
                return Err(self.quarantine(
                    WorkerError::new("worker_transport_failed", error.to_string()),
                    fence,
                ))
            }
        };
        if response.protocol != 1
            || response.worker_id != self.worker_id
            || response.operation_id != operation_id
            || &response.fence != fence
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "worker_stale_result",
                    "worker result fence does not match admitted operation",
                ),
                fence,
            ));
        }
        response.output.map_err(|mut error| {
            error.phase_arrivals = self.phase_arrivals();
            error
        })
    }

    pub(crate) fn prepare(
        &mut self,
        manifest: &Path,
        fence: &WorkerFence,
    ) -> Result<Prepared, WorkerError> {
        let output = self.execute(
            Operation::Prepare {
                manifest: manifest.to_path_buf(),
            },
            fence,
        )?;
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new("safe_unit_timeout", "Match preparation exceeded 2000 ms"),
                fence,
            ));
        }
        match output {
            Output::Prepared(prepared)
                if prepared.generation == fence.model_generation
                    && prepared.embedding_dim == crate::identity::EMBEDDING_DIM =>
            {
                self.prepared = Some(prepared.clone());
                Ok(prepared)
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_generation_mismatch",
                    "prepared model differs from admission",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn begin_preparation(
        &mut self,
        manifest: &Path,
        runtime: crate::match_acceleration::ProbeRuntime,
        candidate: bool,
        fence: &WorkerFence,
    ) -> Result<(), WorkerError> {
        if runtime == crate::match_acceleration::ProbeRuntime::CpuTwoThread
            && (!candidate || !self.cpu_two_admitted || self.cpu_executor_init_micros.is_none())
        {
            return Err(WorkerError::new(
                "worker_not_prepared",
                "two-thread candidate requires admitted executor checkpoint",
            ));
        }
        self.prepared = None;
        let output = self.execute(
            Operation::PreparationBegin {
                manifest: manifest.to_path_buf(),
                runtime,
                candidate,
            },
            fence,
        )?;
        match output {
            Output::PreparationCheckpoint { generation }
                if generation == fence.model_generation =>
            {
                Ok(())
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_generation_mismatch",
                    "preparation checkpoint differs from admission",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn finish_compute_resources(
        &self,
        resources: crate::match_store::MatchResourceLease,
    ) {
        if self.quarantined && !self.confirmed_dead() {
            self.process.retain_resources_until_exit(vec![resources]);
        }
        // A completed response, or a confirmed empty Job, releases on return.
    }

    pub(crate) fn retain_preparation_resources(
        &mut self,
        resources: crate::match_store::MatchResourceLease,
    ) {
        self.preparation_resources = Some(resources);
    }
    pub(crate) fn release_dead_preparation_resources(&mut self) {
        if self.confirmed_dead() {
            self.process.release_dead_resources();
            self.preparation_resources.take();
        }
    }

    pub(crate) fn preparation_step(&mut self, fence: &WorkerFence) -> Result<bool, WorkerError> {
        match self.execute(Operation::PreparationStep, fence)? {
            Output::PreparationCheckpoint { generation }
                if generation == fence.model_generation =>
            {
                Ok(false)
            }
            Output::Prepared(prepared)
                if prepared.generation == fence.model_generation
                    && prepared.embedding_dim == crate::identity::EMBEDDING_DIM =>
            {
                self.process.release_preparation_bytes().map_err(|e| {
                    self.quarantine(WorkerError::new("worker_resource_accounting", e), fence)
                })?;
                self.prepared = Some(prepared);
                self.preparation_resources.take();
                Ok(true)
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_generation_mismatch",
                    "prepared model differs from admission",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn embed(
        &mut self,
        encoded: Vec<u8>,
        source_path: &Path,
        fence: &WorkerFence,
    ) -> Result<WorkerFaceBatch, WorkerError> {
        if encoded.len() > MAX_IMAGE {
            return Err(WorkerError::new(
                "worker_input_limit",
                "encoded image exceeds 256 MiB source limit",
            ));
        }
        self.embed_operation(
            Operation::Embed {
                encoded,
                source_path: source_path.to_path_buf(),
            },
            fence,
        )
    }

    pub(crate) fn inspect_pinned_image(
        &mut self,
        source_path: &Path,
        fence: &WorkerFence,
    ) -> Result<WorkerImageInfo, WorkerError> {
        let output = self.execute(
            Operation::InspectPinnedImage {
                source_path: source_path.to_owned(),
            },
            fence,
        )?;
        let valid = matches!(&output, Output::ImageInfo(info) if valid_image_info(info));
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "safe_unit_timeout",
                    "image header exceeded 2000 ms during validation",
                ),
                fence,
            ));
        }
        match output {
            Output::ImageInfo(info) if valid => Ok(info),
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "pinned image header or budget invalid",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn embed_pinned_image(
        &mut self,
        source_path: &Path,
        fence: &WorkerFence,
    ) -> Result<WorkerFaceBatch, WorkerError> {
        self.embed_operation(
            Operation::EmbedPinnedImage {
                source_path: source_path.to_owned(),
            },
            fence,
        )
    }

    pub(crate) fn embed_video_exemplar(
        &mut self,
        encoded: Vec<u8>,
        source_path: &Path,
        detection_index: usize,
        fence: &WorkerFence,
    ) -> Result<WorkerFaceBatch, WorkerError> {
        if encoded.len() > MAX_IMAGE || detection_index >= MAX_FACES {
            return Err(WorkerError::new(
                "worker_input_limit",
                "video exemplar request exceeds bounds",
            ));
        }
        self.embed_operation(
            Operation::EmbedVideoExemplar {
                encoded,
                source_path: source_path.to_path_buf(),
                detection_index,
            },
            fence,
        )
    }

    fn embed_operation(
        &mut self,
        body: Operation,
        fence: &WorkerFence,
    ) -> Result<WorkerFaceBatch, WorkerError> {
        if self
            .prepared
            .as_ref()
            .is_none_or(|p| p.generation != fence.model_generation)
        {
            return Err(WorkerError::new(
                "worker_not_prepared",
                "prepare exact model generation before inference",
            ));
        }
        match self.execute(body, fence)? {
            Output::Faces(batch) if valid_batch(&batch, &fence.model_generation) => {
                if self
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    Err(self.quarantine(
                        WorkerError::new(
                            "safe_unit_timeout",
                            "Match safe unit exceeded 2000 ms during output validation",
                        ),
                        fence,
                    ))
                } else {
                    Ok(batch)
                }
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "worker output violates image/vector contract",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn detect(
        &mut self,
        encoded: Vec<u8>,
        source_path: &Path,
        fence: &WorkerFence,
    ) -> Result<WorkerDetectionBatch, WorkerError> {
        if encoded.len() > MAX_IMAGE {
            return Err(WorkerError::new(
                "worker_input_limit",
                "video detection input exceeds bound",
            ));
        }
        if !self.is_prepared_for(&fence.model_generation) {
            return Err(WorkerError::new(
                "worker_not_prepared",
                "prepare exact model generation before detection",
            ));
        }
        let output = self.execute(
            Operation::Detect {
                encoded,
                source_path: source_path.to_path_buf(),
            },
            fence,
        )?;
        let valid = match &output {
            Output::Detections(batch) => {
                batch.image_w > 0
                    && batch.image_h > 0
                    && batch.faces.len() <= MAX_FACES
                    && batch.faces.iter().enumerate().all(|(index, face)| {
                        face.source_index == index
                            && face.score.is_finite()
                            && face
                                .bbox
                                .iter()
                                .chain(face.landmarks.iter().flatten())
                                .all(|value| value.is_finite())
                    })
            }
            _ => false,
        };
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new("safe_unit_timeout", "Match detection exceeded 2000 ms"),
                fence,
            ));
        }
        if !valid {
            return Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "invalid bounded detector result"),
                fence,
            ));
        }
        match output {
            Output::Detections(batch) => Ok(batch),
            _ => unreachable!(),
        }
    }

    pub(crate) fn decode(
        &mut self,
        source_path: &Path,
        requested_ms: u64,
        stream_index: u32,
        fence: &WorkerFence,
    ) -> Result<Option<crate::match_video_decode::DecodedVideoSample>, WorkerError> {
        self.decode_operation(
            Operation::Decode {
                source_path: source_path.to_path_buf(),
                requested_ms,
                stream_index,
            },
            stream_index,
            None,
            fence,
        )
    }

    pub(crate) fn decode_exact(
        &mut self,
        source_path: &Path,
        time: crate::match_video::VideoTime,
        stream_index: u32,
        fence: &WorkerFence,
    ) -> Result<Option<crate::match_video_decode::DecodedVideoSample>, WorkerError> {
        time.validate()
            .map_err(|error| WorkerError::new("invalid_video_time", error))?;
        self.decode_operation(
            Operation::DecodeExact {
                source_path: source_path.to_path_buf(),
                time,
                stream_index,
            },
            stream_index,
            Some(time),
            fence,
        )
    }

    fn decode_operation(
        &mut self,
        operation: Operation,
        stream_index: u32,
        exact_time: Option<crate::match_video::VideoTime>,
        fence: &WorkerFence,
    ) -> Result<Option<crate::match_video_decode::DecodedVideoSample>, WorkerError> {
        use sha2::{Digest, Sha256};
        let output = self.execute(operation, fence)?;
        let valid = match &output {
            Output::Decoded(None) => exact_time.is_none(),
            Output::Decoded(Some(sample)) => {
                (stream_index == u32::MAX || sample.stream_index == stream_index)
                    && exact_time.is_none_or(|time| sample.time == time)
                    && sample.time.validate().is_ok()
                    && sample.time.milliseconds().is_ok()
                    && sample.playback_origin.validate().is_ok()
                    && sample.width > 0
                    && sample.height > 0
                    && sample.width <= 640
                    && sample.height <= 640
                    && sample.encoded.len() <= 640 * 640 * 3 + 4096
                    && sample.scene_probe.len() == 1024
                    && sample.frame_sha256 == format!("{:x}", Sha256::digest(&sample.encoded))
            }
            _ => false,
        };
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new("safe_unit_timeout", "Match video decode exceeded 2000 ms"),
                fence,
            ));
        }
        if !valid {
            return Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "video sample provenance or byte bound invalid",
                ),
                fence,
            ));
        }
        match output {
            Output::Decoded(sample) => Ok(sample),
            _ => unreachable!(),
        }
    }

    pub(crate) fn retain_discovery_resources(
        &mut self,
        resources: crate::match_store::MatchResourceLease,
    ) {
        self.discovery_resources = Some(resources);
    }
    pub(crate) fn begin_discovery(
        &mut self,
        root: &Path,
        exclusions: &[String],
        fence: &WorkerFence,
    ) -> Result<(), WorkerError> {
        crate::match_discovery::validate_begin(root, exclusions)
            .map_err(|error| WorkerError::new("worker_input_limit", error))?;
        // Count without allocating a serialized copy; reserve the remaining
        // seven MiB for maximum escaping of paths/exclusions and framing.
        struct FenceCounter(usize);
        impl Write for FenceCounter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > 1024 * 1024 {
                    return Err(std::io::Error::other("discovery fence exceeds bound"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(FenceCounter(0), fence).map_err(|_| {
            WorkerError::new(
                "worker_input_limit",
                "discovery fence exceeds serialized bound",
            )
        })?;
        match self.execute(
            Operation::DiscoveryBegin {
                root: root.to_path_buf(),
                exclusions: exclusions.to_vec(),
            },
            fence,
        )? {
            Output::DiscoveryReady => Ok(()),
            _ => Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "invalid discovery begin response"),
                fence,
            )),
        }
    }
    pub(crate) fn discovery_next(
        &mut self,
        fence: &WorkerFence,
    ) -> Result<crate::match_discovery::Step, WorkerError> {
        match self.execute(Operation::DiscoveryNext, fence)? {
            Output::DiscoveryStep(step) => {
                if step.is_none() {
                    self.discovery_resources.take();
                }
                Ok(step)
            }
            _ => Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "invalid discovery step response"),
                fence,
            )),
        }
    }
    pub(crate) fn discovery_metadata(
        &mut self,
        path: &Path,
        fence: &WorkerFence,
    ) -> Result<Result<u64, String>, WorkerError> {
        match self.execute(
            Operation::DiscoveryMetadata {
                path: path.to_path_buf(),
            },
            fence,
        )? {
            Output::DiscoveryMetadata(result) => Ok(result),
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "invalid discovery metadata response",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn begin_playback_source(
        &mut self,
        path: &Path,
        root: &Path,
        fence: &WorkerFence,
    ) -> Result<std::sync::Arc<crate::match_video_decode::PlaybackSourcePin>, WorkerError> {
        let output = self.execute(
            Operation::FingerprintBeginForPlayback {
                source_path: path.to_path_buf(),
                expected_root: root.to_path_buf(),
            },
            fence,
        )?;
        let Output::PlaybackSource(source) = output else {
            return Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "missing playback source handles"),
                fence,
            ));
        };
        if source.handles.is_empty()
            || source.handles.len() > 65
            || !source.final_path.is_absolute()
            || source.final_path.as_os_str().len() > 32768
            || source.handles.iter().any(|h| *h == 0)
            || source
                .handles
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != source.handles.len()
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "invalid bounded playback source handles",
                ),
                fence,
            ));
        }
        let mut files = Vec::with_capacity(source.handles.len());
        for handle in source.handles {
            files.push(self.process.duplicate_source_file(handle).map_err(|_| {
                WorkerError::new(
                    "playback_pin_transfer_failed",
                    "owned child source duplication failed",
                )
            })?);
        }
        Ok(std::sync::Arc::new(
            crate::match_video_decode::PlaybackSourcePin {
                final_path: source.final_path,
                _files: files,
            },
        ))
    }

    pub(crate) fn begin_source(
        &mut self,
        source_path: &Path,
        expected_root: &Path,
        fence: &WorkerFence,
    ) -> Result<(), WorkerError> {
        match self.execute(
            Operation::FingerprintBegin {
                source_path: source_path.to_path_buf(),
                expected_root: expected_root.to_path_buf(),
            },
            fence,
        )? {
            Output::SourceReady => Ok(()),
            _ => Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "invalid source-open response"),
                fence,
            )),
        }
    }

    pub(crate) fn hash_source_step(
        &mut self,
        fence: &WorkerFence,
    ) -> Result<crate::match_video_decode::VideoSourceProgress, WorkerError> {
        match self.execute(Operation::FingerprintStep, fence)? {
            Output::SourceProgress(progress) if progress.bytes_hashed <= progress.size => {
                Ok(progress)
            }
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "invalid source fingerprint progress",
                ),
                fence,
            )),
        }
    }

    pub(crate) fn prepare_candidate(
        &mut self,
        manifest: &Path,
        runtime: crate::match_acceleration::ProbeRuntime,
        fence: &WorkerFence,
    ) -> Result<crate::match_acceleration::ProbePrepared, WorkerError> {
        if runtime == crate::match_acceleration::ProbeRuntime::CpuTwoThread {
            return Err(WorkerError::new(
                "checkpoint_required",
                "two-thread candidate requires explicit executor checkpoint",
            ));
        }
        let output = self.execute(
            Operation::CandidatePrepare {
                manifest: manifest.to_path_buf(),
                runtime,
            },
            fence,
        )?;
        let valid = matches!(&output, Output::CandidatePrepared(prepared)
            if prepared.runtime == runtime && prepared.generation == fence.model_generation);
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "safe_unit_timeout",
                    "candidate preparation exceeded 2000 ms during output validation",
                ),
                fence,
            ));
        }
        match output {
            Output::CandidatePrepared(prepared) if valid => Ok(prepared),
            _ => Err(self.quarantine(
                WorkerError::new(
                    "worker_invalid_output",
                    "candidate preparation binding invalid",
                ),
                fence,
            )),
        }
    }
    /// Diagnostic coordinator: each step is a separate supervised safe unit.
    pub(crate) fn prepare_candidate_checkpointed(
        &mut self,
        manifest: &Path,
        runtime: crate::match_acceleration::ProbeRuntime,
        fence: &WorkerFence,
    ) -> Result<crate::match_acceleration::ProbePrepared, WorkerError> {
        if runtime == crate::match_acceleration::ProbeRuntime::CpuTwoThread {
            if !self.cpu_two_admitted {
                return Err(WorkerError::new(
                    "resource_pressure",
                    "two CPU units must be admitted before launch",
                ));
            }
            let init_started = Instant::now();
            if !matches!(
                self.execute(Operation::CpuExecutorBegin, fence)?,
                Output::CpuExecutorReady { threads: 2 }
            ) {
                return Err(self.quarantine(
                    WorkerError::new("worker_invalid_output", "private CPU executor not ready"),
                    fence,
                ));
            }
            self.cpu_executor_init_micros =
                Some(init_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
        }
        if runtime == crate::match_acceleration::ProbeRuntime::CpuTwoThread
            && self.cpu_executor_init_micros.is_none()
        {
            return Err(WorkerError::new(
                "worker_not_prepared",
                "CPU initialization timing missing",
            ));
        }
        self.begin_preparation(manifest, runtime, true, fence)?;
        for _ in 0..128 {
            match self.execute(Operation::PreparationStep, fence)? {
                Output::PreparationCheckpoint { generation }
                    if generation == fence.model_generation => {}
                Output::CandidatePrepared(prepared)
                    if prepared.runtime == runtime
                        && prepared.generation == fence.model_generation =>
                {
                    self.process.release_preparation_bytes().map_err(|e| {
                        self.quarantine(WorkerError::new("worker_resource_accounting", e), fence)
                    })?;
                    return Ok(prepared);
                }
                _ => {
                    return Err(self.quarantine(
                        WorkerError::new(
                            "worker_invalid_output",
                            "candidate preparation binding invalid",
                        ),
                        fence,
                    ))
                }
            }
        }
        Err(self.quarantine(
            WorkerError::new(
                "worker_input_limit",
                "preparation checkpoint count exceeded",
            ),
            fence,
        ))
    }
    pub(crate) fn sample_candidate(
        &mut self,
        encoded: Vec<u8>,
        runtime: crate::match_acceleration::ProbeRuntime,
        fence: &WorkerFence,
    ) -> Result<crate::match_acceleration::ProbeFrame, WorkerError> {
        use sha2::{Digest, Sha256};
        if encoded.len() > MAX_IMAGE {
            return Err(WorkerError::new(
                "worker_input_limit",
                "candidate image exceeds byte bound",
            ));
        }
        let expected = format!("{:x}", Sha256::digest(&encoded));
        let output = self.execute(Operation::CandidateSample { encoded, runtime }, fence)?;
        let valid = matches!(&output,
            Output::CandidateFrame(frame) if frame.prepared.runtime==runtime && frame.prepared.generation==fence.model_generation
                && frame.input_sha256==expected && frame.detections.len()<=MAX_FACES && frame.faces.len()<=MAX_FACES
                && frame.failures.len()<=MAX_FACES && frame.faces.iter().all(|f|f.values.len()==512 && f.values.iter().all(|v|v.is_finite())));
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(self.quarantine(
                WorkerError::new(
                    "safe_unit_timeout",
                    "candidate sample exceeded 2000 ms during output validation",
                ),
                fence,
            ));
        }
        match output {
            Output::CandidateFrame(frame) if valid => Ok(frame),
            _ => Err(self.quarantine(
                WorkerError::new("worker_invalid_output", "candidate sample binding invalid"),
                fence,
            )),
        }
    }
}

fn valid_batch(batch: &WorkerFaceBatch, generation: &str) -> bool {
    batch.image_w > 0
        && batch.image_h > 0
        && batch.faces.len() <= MAX_FACES
        && batch.failures.len() <= MAX_FACES
        && batch.faces.iter().all(|face| {
            face.generation == generation
                && face.embedding_dim == 512
                && face.embedding.len() == 512
                && face.embedding.iter().all(|v| v.is_finite())
                && (face
                    .embedding
                    .iter()
                    .map(|v| f64::from(*v).powi(2))
                    .sum::<f64>()
                    - 1.0)
                    .abs()
                    <= 0.001
                && face
                    .bbox
                    .iter()
                    .chain(face.landmarks.iter().flatten())
                    .chain(face.bbox_normalized.iter())
                    .chain(face.landmarks_normalized.iter().flatten())
                    .all(|v| v.is_finite())
                && face.score.is_finite()
                && face.detection_score.is_finite()
                && face.face_fraction.is_finite()
        })
}

fn wire_face_batch(batch: crate::identity::FaceBatch) -> WorkerFaceBatch {
    WorkerFaceBatch {
        image_w: batch.image_w,
        image_h: batch.image_h,
        faces: batch
            .faces
            .into_iter()
            .map(|face| WorkerFace {
                bbox: face.face.bbox,
                score: face.face.score,
                landmarks: face.face.landmarks,
                bbox_normalized: face.bbox_normalized,
                landmarks_normalized: face.landmarks_normalized,
                detection_score: face.quality.detection_score,
                face_fraction: face.quality.face_fraction,
                alignment_valid: face.quality.alignment_valid,
                embedding: face.embedding.values().to_vec(),
                embedding_dim: face.embedding_dim,
                generation: face.generation,
            })
            .collect(),
        failures: batch
            .failures
            .into_iter()
            .map(|failure| WorkerFaceFailure {
                detection_index: failure.detection_index,
                code: failure.code,
                message: failure.message,
            })
            .collect(),
    }
}

fn cpu_candidate_scope<T>(
    executor: &Option<(tract_linalg::multithread::Executor, WorkerFence)>,
    runtime: crate::match_acceleration::ProbeRuntime,
    fence: &WorkerFence,
    action: impl FnOnce() -> T,
) -> Result<T, WorkerError> {
    if runtime != crate::match_acceleration::ProbeRuntime::CpuTwoThread {
        if executor.is_some() {
            return Err(WorkerError::new(
                "runtime_mismatch",
                "candidate executor cannot serve baseline",
            ));
        }
        return Ok(action());
    }
    let (executor, admitted) = executor
        .as_ref()
        .ok_or_else(|| WorkerError::new("worker_not_prepared", "private executor required"))?;
    if admitted != fence {
        return Err(WorkerError::new(
            "worker_generation_mismatch",
            "CPU executor admission changed",
        ));
    }
    Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tract_linalg::multithread::multithread_tract_scope(executor.clone(), action)
    }))
    .unwrap_or_else(|_| std::process::abort()))
}

fn production_cpu_scope<T>(
    executor: &Option<(tract_linalg::multithread::Executor, String)>,
    fence: &WorkerFence,
    action: impl FnOnce() -> T,
) -> Result<T, WorkerError> {
    let Some((pool, generation)) = executor else {
        return Ok(action());
    };
    // Pool lifetime is model-bound; execute still fences each current request.
    if generation != &fence.model_generation {
        return Err(WorkerError::new(
            "worker_generation_mismatch",
            "production executor generation changed",
        ));
    }
    Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tract_linalg::multithread::multithread_tract_scope(pool.clone(), action)
    }))
    .unwrap_or_else(|_| std::process::abort()))
}
fn preparation_cpu_scope<T>(
    candidate: &Option<(tract_linalg::multithread::Executor, WorkerFence)>,
    production: &Option<(tract_linalg::multithread::Executor, String)>,
    runtime: crate::match_acceleration::ProbeRuntime,
    fence: &WorkerFence,
    action: impl FnOnce() -> T,
) -> Result<T, WorkerError> {
    if production.is_some() {
        if candidate.is_some() || runtime != crate::match_acceleration::ProbeRuntime::Cpu {
            return Err(WorkerError::new(
                "runtime_mismatch",
                "production CPU policy cannot serve candidate",
            ));
        }
        production_cpu_scope(production, fence, action)
    } else {
        cpu_candidate_scope(candidate, runtime, fence, action)
    }
}

pub(crate) fn worker_entry(args: &[String]) -> i32 {
    use sha2::{Digest, Sha256};
    let Some(worker_id) = args.first().filter(|id| uuid::Uuid::parse_str(id).is_ok()) else {
        return 2;
    };
    let harness = cfg!(debug_assertions) && args.get(1).is_some_and(|arg| arg == "--test-harness");
    if args.len() != if harness { 2 } else { 1 } {
        return 2;
    }
    let mut pool_boundary: Option<tract_cuda::ManagedPoolBoundary> = None;
    let mut bootstrap: Option<(crate::match_cuda_bootstrap::Bootstrap, WorkerFence)> = None;
    let mut production_executor: Option<(tract_linalg::multithread::Executor, String)> = None;
    let mut cpu_executor: Option<(tract_linalg::multithread::Executor, WorkerFence)> = None;
    let mut engine: Option<crate::identity::IdentityEngine> = None;
    let mut candidate: Option<crate::identity::AccelerationCandidateEngine> = None;
    let mut preparation: Option<(
        crate::identity::PreparationSession,
        WorkerFence,
        crate::match_acceleration::ProbeRuntime,
        bool,
        Instant,
        u64,
        u32,
    )> = None;
    let mut source: Option<crate::match_video_decode::VideoSourceReader> = None;
    let mut discovery: Option<crate::match_discovery::Discovery> = None;
    let mut decoded_frames = std::collections::VecDeque::<String>::new();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    loop {
        let request = match read_request(&mut reader) {
            Ok(request) => request,
            Err(_) => return 0,
        };
        if request.protocol != 1
            || &request.worker_id != worker_id
            || uuid::Uuid::parse_str(&request.operation_id).is_err()
        {
            return 2;
        }
        let mut response = Response {
            protocol: 1,
            worker_id: worker_id.clone(),
            operation_id: request.operation_id,
            fence: request.fence,
            output: Err(WorkerError::new(
                "worker_invalid_operation",
                "operation rejected",
            )),
        };
        if let Operation::Embed { encoded, .. }
        | Operation::Detect { encoded, .. }
        | Operation::EmbedVideoExemplar { encoded, .. }
        | Operation::CandidateSample { encoded, .. } = &request.body
        {
            let dimensions = image::ImageReader::new(std::io::Cursor::new(encoded))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.into_dimensions().ok());
            if dimensions.is_some_and(|(w, h)| u64::from(w) * u64::from(h) * 3 > MAX_IMAGE as u64) {
                response.output = Err(WorkerError::new(
                    "worker_decode_limit",
                    "decoded image exceeds 256 MiB working bound",
                ));
                if write_frame(&mut writer, &response).is_err() {
                    return 1;
                }
                continue;
            }
        }
        if let Operation::Detect {
            encoded,
            source_path,
        }
        | Operation::EmbedVideoExemplar {
            encoded,
            source_path,
            ..
        } = &request.body
        {
            let hash = format!("{:x}", Sha256::digest(encoded));
            if !source
                .as_ref()
                .is_some_and(|reader| reader.matches(source_path))
                || !decoded_frames.contains(&hash)
            {
                response.output = Err(WorkerError::new(
                    "stale_video_frame",
                    "video frame is not a decoded sample of the pinned source",
                ));
                if write_frame(&mut writer, &response).is_err() {
                    return 1;
                }
                continue;
            }
        }
        let requests_cuda = matches!(
            &request.body,
            Operation::PreparationBegin {
                runtime: crate::match_acceleration::ProbeRuntime::Cuda,
                ..
            } | Operation::CandidatePoolBoundary { .. }
                | Operation::CandidateManagedGpuPeaks
                | Operation::CandidatePrepare {
                    runtime: crate::match_acceleration::ProbeRuntime::Cuda,
                    ..
                }
                | Operation::CandidateSample {
                    runtime: crate::match_acceleration::ProbeRuntime::Cuda,
                    ..
                }
        );
        if requests_cuda
            && !bootstrap
                .as_ref()
                .is_some_and(|(state, fence)| state.ready() && fence == &response.fence)
        {
            response.output = Err(WorkerError::new(
                "candidate_bootstrap_required",
                "verified isolated runtime required before CUDA",
            ));
            if write_frame(&mut writer, &response).is_err() {
                return 1;
            }
            continue;
        }
        response.output = match request.body {
            Operation::ProductionCpuExecutorBegin => {
                if production_executor.is_some()
                    || cpu_executor.is_some()
                    || engine.is_some()
                    || candidate.is_some()
                    || preparation.is_some()
                    || bootstrap.is_some()
                {
                    Err(WorkerError::new(
                        "worker_not_fresh",
                        "production executor requires fresh worker",
                    ))
                } else {
                    let executor = std::panic::catch_unwind(|| {
                        tract_linalg::multithread::Executor::multithread_with_name(
                            2,
                            "facial-match-production",
                        )
                    })
                    .unwrap_or_else(|_| std::process::abort());
                    let threads = match &executor {
                        tract_linalg::multithread::Executor::MultiThread(pool) => {
                            pool.current_num_threads()
                        }
                        _ => 0,
                    };
                    production_executor = Some((executor, response.fence.model_generation.clone()));
                    Ok(Output::CpuExecutorReady { threads })
                }
            }
            Operation::CpuExecutorBegin => {
                if cpu_executor.is_some()
                    || production_executor.is_some()
                    || engine.is_some()
                    || candidate.is_some()
                    || preparation.is_some()
                {
                    Err(WorkerError::new(
                        "worker_not_fresh",
                        "private executor requires fresh candidate worker",
                    ))
                } else {
                    let executor = std::panic::catch_unwind(|| {
                        tract_linalg::multithread::Executor::multithread_with_name(
                            2,
                            "facial-match-cpu",
                        )
                    })
                    .unwrap_or_else(|_| std::process::abort());
                    let threads = match &executor {
                        tract_linalg::multithread::Executor::MultiThread(pool) => {
                            pool.current_num_threads()
                        }
                        _ => 0,
                    };
                    cpu_executor = Some((executor, response.fence.clone()));
                    Ok(Output::CpuExecutorReady { threads })
                }
            }

            Operation::CandidateManagedGpuPeaks => crate::match_cuda_bootstrap::managed_gpu_peaks()
                .map(Output::CandidateManagedGpuPeaks)
                .map_err(|code| WorkerError::new(code, code)),
            Operation::CandidatePoolBoundary { step } => {
                let advance = if step == 0
                    && pool_boundary.is_none()
                    && candidate.is_none()
                    && preparation.is_none()
                {
                    tract_cuda::ManagedPoolBoundary::begin()
                        .map(|state| {
                            pool_boundary = Some(state);
                        })
                        .map_err(|_| ())
                } else if let Some(state) = pool_boundary.as_mut() {
                    state.advance(step).map_err(|_| ())
                } else {
                    Err(())
                };
                advance
                    .map_err(|_| {
                        WorkerError::new(
                            "candidate_pool_boundary_failed",
                            "managed pool boundary failed",
                        )
                    })
                    .and_then(|()| {
                        crate::match_cuda_bootstrap::managed_gpu_peaks()
                            .map(|peaks| Output::CandidatePoolBoundary { step, peaks })
                            .map_err(|code| WorkerError::new(code, code))
                    })
            }
            Operation::CandidateBootstrapBegin {
                root,
                pool_limit_bytes,
            } => {
                if bootstrap.is_some()
                    || std::env::var_os("FACIAL_CUDA_CANDIDATE_ROOT").as_deref()
                        != Some(root.as_os_str())
                {
                    Err(WorkerError::new(
                        "candidate_bootstrap_root",
                        "fresh explicitly configured child required",
                    ))
                } else {
                    crate::match_cuda_bootstrap::Bootstrap::begin(root, pool_limit_bytes)
                        .map(|state| {
                            bootstrap = Some((state, response.fence.clone()));
                            Output::CandidateBootstrapCheckpoint
                        })
                        .map_err(|code| WorkerError::new(code, code))
                }
            }
            Operation::CandidateBootstrapStep => match bootstrap.as_mut() {
                Some((state, fence)) if fence == &response.fence => state
                    .advance()
                    .map(|ready| match ready {
                        Some(info) => Output::CandidateBootstrapReady(info),
                        None => Output::CandidateBootstrapCheckpoint,
                    })
                    .map_err(|code| WorkerError::new(code, code)),
                _ => Err(WorkerError::new(
                    "candidate_bootstrap_fence",
                    "candidate bootstrap fence mismatch",
                )),
            },
            Operation::PreparationBegin {
                manifest,
                runtime,
                candidate: is_candidate,
            } => {
                preparation.take();
                engine.take();
                candidate.take();
                let started = Instant::now();
                preparation_cpu_scope(
                    &cpu_executor,
                    &production_executor,
                    runtime,
                    &response.fence,
                    || crate::identity::PreparationSession::begin(&manifest, runtime.name()),
                )
                .map_err(|e| crate::identity::IdentityError {
                    code: e.code,
                    message: e.message,
                })
                .and_then(|result| result)
                .and_then(|session| {
                    if session.generation() != response.fence.model_generation {
                        return Err(crate::identity::IdentityError {
                            code: "generation_mismatch".into(),
                            message: "preparation generation differs from admission".into(),
                        });
                    }
                    let generation = session.generation().to_string();
                    let elapsed = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                    preparation = Some((
                        session,
                        response.fence.clone(),
                        runtime,
                        is_candidate,
                        started,
                        elapsed,
                        1,
                    ));
                    Ok(Output::PreparationCheckpoint { generation })
                })
                .map_err(|error| WorkerError::new(&error.code, error.message))
            }
            Operation::PreparationStep => (|| {
                let step_started = Instant::now();
                let Some((
                    mut session,
                    admitted,
                    runtime,
                    is_candidate,
                    started,
                    previous_max,
                    units,
                )) = preparation.take()
                else {
                    return Err(WorkerError::new(
                        "worker_not_prepared",
                        "begin preparation first",
                    ));
                };
                if admitted != response.fence {
                    return Err(WorkerError::new(
                        "worker_generation_mismatch",
                        "preparation admission changed",
                    ));
                }
                let phase = session.phase();
                write_frame(
                    &mut writer,
                    &Response {
                        protocol: response.protocol,
                        worker_id: response.worker_id.clone(),
                        operation_id: response.operation_id.clone(),
                        fence: response.fence.clone(),
                        output: Ok(Output::PreparationProgress(phase)),
                    },
                )
                .map_err(|error| WorkerError::new("worker_transport", error))?;
                let complete = preparation_cpu_scope(
                    &cpu_executor,
                    &production_executor,
                    runtime,
                    &response.fence,
                    || session.step(),
                )?
                .map_err(|error| {
                    let mut error = WorkerError::new(&error.code, error.message);
                    error.last_phase = Some(phase);
                    error
                })?;
                let maximum = previous_max
                    .max(step_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
                let units = units + 1;
                match complete {
                    None => {
                        let generation = session.generation().to_string();
                        preparation = Some((
                            session,
                            admitted,
                            runtime,
                            is_candidate,
                            started,
                            maximum,
                            units,
                        ));
                        Ok(Output::PreparationCheckpoint { generation })
                    }
                    Some(loaded) if is_candidate => {
                        let loaded = crate::identity::AccelerationCandidateEngine::from_prepared(
                            loaded,
                            runtime,
                            started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                            maximum,
                            units,
                        )
                        .map_err(|error| WorkerError::new(&error.code, error.message))?;
                        let prepared = loaded.prepared().clone();
                        candidate = Some(loaded);
                        Ok(Output::CandidatePrepared(prepared))
                    }
                    Some(loaded) => {
                        let prepared = Prepared {
                            generation: loaded.generation().to_string(),
                            embedding_dim: loaded.embedding_dim(),
                        };
                        engine = Some(loaded);
                        Ok(Output::Prepared(prepared))
                    }
                }
            })(),
            Operation::InspectPinnedImage { source_path } => (|| {
                let reader = source.as_ref().ok_or_else(|| {
                    WorkerError::new("source_not_pinned", "pin image before header inspection")
                })?;
                let bytes = reader
                    .image_bytes(&source_path, &response.fence.media_fingerprint)
                    .map_err(|error| WorkerError::new("source_not_pinned", error))?;
                inspect_image_bytes(&bytes).map(Output::ImageInfo)
            })(),
            Operation::EmbedPinnedImage { source_path } => {
                production_cpu_scope(&production_executor, &response.fence, || {
                    (|| {
                        let reader = source.as_ref().ok_or_else(|| {
                            WorkerError::new("source_not_pinned", "pin image before inference")
                        })?;
                        let bytes = reader
                            .image_bytes(&source_path, &response.fence.media_fingerprint)
                            .map_err(|error| WorkerError::new("source_not_pinned", error))?;
                        inspect_image_bytes(&bytes)?;
                        let engine = engine
                            .as_ref()
                            .filter(|engine| engine.generation() == response.fence.model_generation)
                            .ok_or_else(|| {
                                WorkerError::new("worker_not_prepared", "exact generation required")
                            })?;
                        engine
                            .embed_faces_bytes(&bytes, &source_path)
                            .map(|batch| Output::Faces(wire_face_batch(batch)))
                            .map_err(|error| WorkerError::new(&error.code, error.message))
                    })()
                })
                .and_then(|result| result)
            }
            Operation::DiscoveryBegin { root, exclusions } => {
                discovery.take();
                crate::match_discovery::Discovery::begin(root, exclusions)
                    .map(|value| {
                        discovery = Some(value);
                        Output::DiscoveryReady
                    })
                    .map_err(|error| WorkerError::new("discovery_io", error))
            }
            Operation::DiscoveryNext => discovery
                .as_mut()
                .ok_or_else(|| WorkerError::new("discovery_not_open", "begin discovery first"))
                .and_then(|discovery| {
                    discovery
                        .next()
                        .map(Output::DiscoveryStep)
                        .map_err(|error| WorkerError::new("discovery_io", error))
                }),
            Operation::DiscoveryMetadata { path } => discovery
                .as_ref()
                .ok_or_else(|| WorkerError::new("discovery_not_open", "begin discovery first"))
                .map(|discovery| Output::DiscoveryMetadata(discovery.metadata(&path))),
            Operation::FingerprintBeginForPlayback {
                source_path,
                expected_root,
            } => {
                source.take();
                decoded_frames.clear();
                crate::match_video_decode::VideoSourceReader::begin(&source_path, &expected_root)
                    .and_then(|mut reader| {
                        let handles = reader.playback_handles()?;
                        source = Some(reader);
                        Ok(Output::PlaybackSource(handles))
                    })
                    .map_err(|error| WorkerError::new("source_pin_failed", error))
            }
            Operation::FingerprintBegin {
                source_path,
                expected_root,
            } => {
                source.take();
                decoded_frames.clear();
                crate::match_video_decode::VideoSourceReader::begin(&source_path, &expected_root)
                    .map(|reader| {
                        source = Some(reader);
                        Output::SourceReady
                    })
                    .map_err(|error| WorkerError::new("source_open_failed", error))
            }
            Operation::FingerprintStep => match source.as_mut() {
                Some(reader) => reader
                    .step()
                    .map(Output::SourceProgress)
                    .map_err(|error| WorkerError::new("source_fingerprint_failed", error)),
                None => Err(WorkerError::new(
                    "source_not_open",
                    "open pinned source before fingerprinting",
                )),
            },
            Operation::CandidatePrepare { ref manifest, .. }
                if harness
                    && matches!(
                        manifest.to_str(),
                        Some(
                            "__wp086-progress-flood__"
                                | "__wp086-progress-fence__"
                                | "__wp086-progress-hang__"
                        )
                    ) =>
            {
                // Only the explicitly launched debug harness recognizes these paths.
                // Keep the request CandidatePrepare so production transport checks run.
                let mismatch = manifest == Path::new("__wp086-progress-fence__");
                let paced = manifest == Path::new("__wp086-progress-hang__");
                for _ in 0..if paced {
                    12
                } else if mismatch {
                    1
                } else {
                    16
                } {
                    let mut progress_fence = response.fence.clone();
                    if mismatch {
                        progress_fence.admission_epoch =
                            progress_fence.admission_epoch.wrapping_add(1);
                    }
                    let progress = Response {
                        protocol: response.protocol,
                        worker_id: response.worker_id.clone(),
                        operation_id: response.operation_id.clone(),
                        fence: progress_fence,
                        output: Ok(Output::PreparationProgress(
                            crate::identity::PreparationPhase::ManifestRead,
                        )),
                    };
                    if write_frame(&mut writer, &progress).is_err() {
                        return 1;
                    }
                    if paced {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                loop {
                    std::thread::park();
                }
            }
            Operation::CandidatePrepare { manifest, runtime } => {
                let mut last_phase = None;
                let mut progress = |phase| {
                    last_phase = Some(phase);
                    let message = Response {
                        protocol: response.protocol,
                        worker_id: response.worker_id.clone(),
                        operation_id: response.operation_id.clone(),
                        fence: response.fence.clone(),
                        output: Ok(Output::PreparationProgress(phase)),
                    };
                    let _ = write_frame(&mut writer, &message);
                };
                crate::identity::AccelerationCandidateEngine::load_with_progress(
                    &manifest,
                    runtime,
                    &mut progress,
                )
                .map(|loaded| {
                    let prepared = loaded.prepared().clone();
                    candidate = Some(loaded);
                    Output::CandidatePrepared(prepared)
                })
                .map_err(|error| {
                    let mut error = WorkerError::new(&error.code, error.message);
                    error.last_phase = last_phase;
                    error
                })
            }
            Operation::CandidateSample { encoded, runtime } => {
                (|| match candidate.as_ref().filter(|engine| {
                    engine.prepared().runtime == runtime
                        && engine.prepared().generation == response.fence.model_generation
                }) {
                    Some(engine) => preparation_cpu_scope(
                        &cpu_executor,
                        &production_executor,
                        runtime,
                        &response.fence,
                        || engine.sample(&encoded),
                    )?
                    .map(Output::CandidateFrame)
                    .map_err(|error| WorkerError::new(&error.code, error.message)),
                    None => Err(WorkerError::new(
                        "worker_not_prepared",
                        "candidate runtime/generation not prepared",
                    )),
                })()
            }
            Operation::Prepare { manifest } => {
                crate::identity::IdentityEngine::load_manifest(&manifest)
                    .map(|loaded| {
                        let prepared = Prepared {
                            generation: loaded.generation().into(),
                            embedding_dim: loaded.embedding_dim(),
                        };
                        engine = Some(loaded);
                        Output::Prepared(prepared)
                    })
                    .map_err(|error| WorkerError::new(&error.code, error.message))
            }
            Operation::Decode {
                source_path,
                requested_ms,
                stream_index,
            } => {
                if !source
                    .as_ref()
                    .is_some_and(|reader| reader.matches(&source_path))
                {
                    Err(WorkerError::new(
                        "source_not_pinned",
                        "decode requires the completed fingerprint of this pinned path",
                    ))
                } else {
                    crate::match_video_decode::decode_sample(
                        &source_path,
                        requested_ms,
                        stream_index,
                    )
                    .map(|sample| {
                        if let Some(sample) = &sample {
                            if decoded_frames.len() >= 4096 {
                                decoded_frames.pop_front();
                            }
                            decoded_frames.push_back(sample.frame_sha256.clone());
                        }
                        Output::Decoded(sample)
                    })
                    .map_err(|error| WorkerError::new("video_decode_failed", error))
                }
            }
            Operation::DecodeExact {
                source_path,
                time,
                stream_index,
            } => {
                if !source
                    .as_ref()
                    .is_some_and(|reader| reader.matches(&source_path))
                {
                    Err(WorkerError::new(
                        "source_not_pinned",
                        "exact decode requires completed fingerprint of this pinned path",
                    ))
                } else {
                    crate::match_video_decode::decode_exact_sample(&source_path, time, stream_index)
                        .map(|sample| {
                            if let Some(sample) = &sample {
                                if decoded_frames.len() >= 4096 {
                                    decoded_frames.pop_front();
                                }
                                decoded_frames.push_back(sample.frame_sha256.clone());
                            }
                            Output::Decoded(sample)
                        })
                        .map_err(|error| WorkerError::new("video_decode_failed", error))
                }
            }
            Operation::Detect { encoded, .. } => {
                production_cpu_scope(&production_executor, &response.fence, || {
                    match engine
                        .as_ref()
                        .filter(|engine| engine.generation() == response.fence.model_generation)
                    {
                        None => Err(WorkerError::new(
                            "worker_not_prepared",
                            "exact generation required",
                        )),
                        Some(engine) => engine
                            .detect_faces_bytes(&encoded)
                            .map(|(image_w, image_h, faces)| {
                                Output::Detections(WorkerDetectionBatch {
                                    image_w,
                                    image_h,
                                    faces: faces
                                        .into_iter()
                                        .enumerate()
                                        .map(|(source_index, face)| WorkerDetection {
                                            source_index,
                                            bbox: face.bbox,
                                            score: face.score,
                                            landmarks: face.landmarks,
                                        })
                                        .collect(),
                                })
                            })
                            .map_err(|error| WorkerError::new(&error.code, error.message)),
                    }
                })
                .and_then(|result| result)
            }
            Operation::EmbedVideoExemplar {
                encoded,
                detection_index,
                ..
            } => production_cpu_scope(&production_executor, &response.fence, || {
                match engine
                    .as_ref()
                    .filter(|engine| engine.generation() == response.fence.model_generation)
                {
                    None => Err(WorkerError::new(
                        "worker_not_prepared",
                        "exact generation required",
                    )),
                    Some(engine) => engine
                        .embed_video_exemplar_bytes(&encoded, detection_index)
                        .map(|batch| Output::Faces(wire_face_batch(batch)))
                        .map_err(|error| WorkerError::new(&error.code, error.message)),
                }
            })
            .and_then(|result| result),
            Operation::Embed {
                encoded,
                source_path,
            } => production_cpu_scope(&production_executor, &response.fence, || {
                if encoded.len() > MAX_IMAGE {
                    return Err(WorkerError::new("worker_input_limit", "image too large"));
                }
                match engine
                    .as_ref()
                    .filter(|engine| engine.generation() == response.fence.model_generation)
                {
                    None => Err(WorkerError::new(
                        "worker_not_prepared",
                        "exact generation required",
                    )),
                    Some(engine) => engine
                        .embed_faces_bytes(&encoded, &source_path)
                        .map(|batch| Output::Faces(wire_face_batch(batch)))
                        .map_err(|error| WorkerError::new(&error.code, error.message)),
                }
            })
            .and_then(|result| result),
            Operation::Harness { fault } if harness => match fault {
                HarnessFault::Hang => loop {
                    std::thread::park();
                },
                HarnessFault::Crash => return 17,
                HarnessFault::LateFence => {
                    response.fence.admission_epoch = response.fence.admission_epoch.wrapping_add(1);
                    Ok(Output::Prepared(Prepared {
                        generation: response.fence.model_generation.clone(),
                        embedding_dim: 512,
                    }))
                }
            },
            Operation::Harness { .. } => return 2,
        };
        if write_frame(&mut writer, &response).is_err() {
            return 1;
        }
    }
}

#[cfg(not(windows))]
mod owned_process {
    use super::*;
    pub(super) struct Process;
    impl Process {
        pub(super) fn duplicate_source_file(&self, _raw: u64) -> Result<std::fs::File, String> {
            Err("native playback pin transfer requires Windows".into())
        }
        pub(super) fn release_dead_resources(&mut self) {}
        pub(super) fn release_preparation_bytes(&mut self) -> Result<(), String> {
            Err("unsupported".into())
        }
        pub(super) fn peaks(&self) -> Result<(usize, usize), String> {
            Err("worker memory accounting unsupported".into())
        }
        pub(super) fn confirmed_dead(&self) -> bool {
            true
        }
        pub(super) fn owned_exit_observer(&self) -> Result<OwnedWorkerExitObserver, String> {
            Err("owned worker exit observation requires Windows".into())
        }
        pub(super) fn terminate(&mut self) {}
        pub(super) fn retain_resources_until_exit(
            &self,
            leases: Vec<crate::match_store::MatchResourceLease>,
        ) {
            // No worker can be spawned on this platform.
            std::mem::forget(leases);
            eprintln!("match_worker_exit_retention_unsupported");
        }
    }
    pub(super) fn spawn(
        _: &Path,
        _: &str,
        _: bool,
        _: crate::match_store::MatchResourceLease,
        _: Option<&Path>,
    ) -> Result<(Process, std::fs::File, std::fs::File), String> {
        Err(
            "Match worker isolation is not implemented on this platform; no in-process fallback"
                .into(),
        )
    }
}

#[cfg(windows)]
mod owned_process {
    use super::*;
    use std::{
        ffi::OsStr,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
    };
    use windows_sys::Win32::{
        Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0},
        Security::SECURITY_ATTRIBUTES,
        System::{JobObjects::*, Pipes::CreatePipe, Threading::*},
    };

    pub(super) struct Process {
        process: OwnedHandle,
        job: OwnedHandle,
        resources: Option<crate::match_store::MatchResourceLease>,
    }
    impl Process {
        pub(super) fn duplicate_source_file(&self, raw: u64) -> Result<std::fs::File, String> {
            use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
            let raw = usize::try_from(raw).map_err(|_| "invalid child handle")?;
            let mut duplicated = std::ptr::null_mut();
            let success = unsafe {
                DuplicateHandle(
                    self.process.as_raw_handle(),
                    raw as HANDLE,
                    GetCurrentProcess(),
                    &mut duplicated,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if success == 0 {
                return Err(last_error());
            }
            Ok(unsafe { std::fs::File::from_raw_handle(duplicated) })
        }
        pub(super) fn release_dead_resources(&mut self) {
            if self.confirmed_dead() {
                self.resources.take();
            }
        }
        pub(super) fn release_preparation_bytes(&mut self) -> Result<(), String> {
            self.resources
                .as_mut()
                .ok_or("resident worker lease absent")?
                .release_worker_preparation_bytes()
        }
        pub(super) fn peaks(&self) -> Result<(usize, usize), String> {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            let success = unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if success == 0 {
                Err(last_error())
            } else {
                Ok((info.PeakProcessMemoryUsed, info.PeakJobMemoryUsed))
            }
        }
        #[cfg(test)]
        pub(super) fn id(&self) -> u32 {
            unsafe { GetProcessId(self.process.as_raw_handle()) }
        }
        pub(super) fn confirmed_dead(&self) -> bool {
            (unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) == WAIT_OBJECT_0 })
                && job_is_empty(&self.job) == Ok(true)
        }
        pub(super) fn owned_exit_observer(&self) -> Result<OwnedWorkerExitObserver, String> {
            let process = self
                .process
                .try_clone()
                .map_err(|error| error.to_string())?;
            let job = self.job.try_clone().map_err(|error| error.to_string())?;
            Ok(Box::new(move || {
                let state = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) };
                if state == windows_sys::Win32::Foundation::WAIT_FAILED {
                    return Err(last_error());
                }
                if state != WAIT_OBJECT_0 {
                    return Ok(false);
                }
                job_is_empty(&job).map_err(|()| "owned worker Job exit query failed".into())
            }))
        }
        pub(super) fn retain_resources_until_exit(
            &self,
            leases: Vec<crate::match_store::MatchResourceLease>,
        ) {
            let job = match self.job.try_clone() {
                Ok(job) => job,
                Err(_) => {
                    std::mem::forget(leases);
                    eprintln!("match_worker_exit_retention_handle_failed");
                    return;
                }
            };
            let retained = std::sync::Arc::new(ExitRetention {
                job,
                _leases: leases,
            });
            let background = retained.clone();
            if std::thread::Builder::new()
                .name("match-worker-exit-reaper".into())
                .spawn(move || loop {
                    match job_is_empty(&background.job) {
                        Ok(true) => break,
                        Ok(false) => std::thread::sleep(Duration::from_millis(50)),
                        Err(()) => {
                            std::mem::forget(background);
                            eprintln!("match_worker_exit_retention_query_failed");
                            break;
                        }
                    }
                })
                .is_err()
            {
                // Keep an owner outside the spawn closure: spawn failure drops it.
                std::mem::forget(retained);
                eprintln!("match_worker_exit_retention_spawn_failed");
            }
        }
        #[cfg(test)]
        pub(super) fn exit_observer(&self) -> Box<dyn Fn() -> bool> {
            let job = self.job.try_clone().unwrap();
            Box::new(move || job_is_empty(&job) == Ok(true))
        }
        pub(super) fn terminate(&mut self) {
            unsafe {
                TerminateJobObject(self.job.as_raw_handle(), 124);
            }
        }
    }
    impl Drop for Process {
        fn drop(&mut self) {
            self.terminate();
            if !self.confirmed_dead() {
                if let Some(resources) = self.resources.take() {
                    self.retain_resources_until_exit(vec![resources]);
                }
            }
        }
    }

    struct ExitRetention {
        job: OwnedHandle,
        _leases: Vec<crate::match_store::MatchResourceLease>,
    }

    fn job_is_empty(job: &OwnedHandle) -> Result<bool, ()> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let success = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as _,
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            Err(())
        } else {
            Ok(info.ActiveProcesses == 0)
        }
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }
    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
    fn pipe(parent_reads: bool) -> Result<(OwnedHandle, OwnedHandle), String> {
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        unsafe {
            if CreatePipe(&mut read, &mut write, &attributes, 0) == 0 {
                return Err(last_error());
            }
            let read = OwnedHandle::from_raw_handle(read);
            let write = OwnedHandle::from_raw_handle(write);
            let (parent, child) = if parent_reads {
                (read, write)
            } else {
                (write, read)
            };
            if SetHandleInformation(parent.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) == 0 {
                return Err(last_error());
            }
            Ok((parent, child))
        }
    }

    pub(super) fn spawn(
        executable: &Path,
        worker_id: &str,
        harness: bool,
        resources: crate::match_store::MatchResourceLease,
        candidate_root: Option<&Path>,
    ) -> Result<(Process, std::fs::File, std::fs::File), String> {
        let cap = usize::try_from(resources.worker_memory_bytes())
            .map_err(|_| "worker memory limit overflow")?;
        if cap == 0 || cap as u64 != crate::match_store::WORKER_MEMORY_LIMIT_BYTES {
            return Err("worker memory policy missing or mismatched".into());
        }
        let executable = executable.canonicalize().map_err(|e| e.to_string())?;
        let (input, child_input) = pipe(false)?;
        let (output, child_output) = pipe(true)?;
        // stderr is a separate drain: arbitrary library diagnostics cannot corrupt framing.
        let (errors, child_errors) = pipe(true)?;
        unsafe {
            let raw_job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw_job.is_null() {
                return Err(last_error());
            }
            let job = OwnedHandle::from_raw_handle(raw_job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = cap;
            // The Rust worker may own one existing FFmpeg decoder child. Both
            // remain in this same non-breakaway kill-on-close Job.
            limits.BasicLimitInformation.ActiveProcessLimit = 2;
            if SetInformationJobObject(
                raw_job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(last_error());
            }
            let mut bytes = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut bytes);
            let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
            let attributes = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attributes, 2, 0, &mut bytes) == 0 {
                return Err(last_error());
            }
            struct AttributeGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
            impl Drop for AttributeGuard {
                fn drop(&mut self) {
                    unsafe {
                        DeleteProcThreadAttributeList(self.0);
                    }
                }
            }
            let _attributes_guard = AttributeGuard(attributes);
            let jobs = [raw_job];
            let inherited: [HANDLE; 3] = [
                child_input.as_raw_handle(),
                child_output.as_raw_handle(),
                child_errors.as_raw_handle(),
            ];
            for (attribute, pointer, size) in [
                (
                    PROC_THREAD_ATTRIBUTE_JOB_LIST,
                    jobs.as_ptr() as *const std::ffi::c_void,
                    std::mem::size_of_val(&jobs),
                ),
                (
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                    inherited.as_ptr() as *const std::ffi::c_void,
                    std::mem::size_of_val(&inherited),
                ),
            ] {
                if UpdateProcThreadAttribute(
                    attributes,
                    0,
                    attribute as usize,
                    pointer,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ) == 0
                {
                    return Err(last_error());
                }
            }
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = inherited[0];
            startup.StartupInfo.hStdOutput = inherited[1];
            startup.StartupInfo.hStdError = inherited[2];
            startup.lpAttributeList = attributes;
            let application = wide(executable.as_os_str());
            // The executable path is passed separately; argv[0] is a fixed token.
            let mut command = wide(OsStr::new(&format!(
                "facial-cli __match-worker-v1 {worker_id}{}",
                if harness { " --test-harness" } else { "" }
            )));
            // Build only the child environment; the parent and other projects are untouched.
            let mut child_environment = candidate_root.map(|root| {
                let replaced = [
                    "CUDA_HOME",
                    "CUDA_PATH",
                    "CUDA_CACHE_PATH",
                    "CUDA_CACHE_DISABLE",
                    "TRACT_CUDA_CACHE_DIR",
                    "FACIAL_CUDA_CANDIDATE_ROOT",
                ];
                let mut entries: Vec<(std::ffi::OsString, std::ffi::OsString)> =
                    std::env::vars_os()
                        .filter(|(key, _)| {
                            !replaced
                                .iter()
                                .any(|name| key.to_string_lossy().eq_ignore_ascii_case(name))
                        })
                        .collect();
                for name in ["CUDA_HOME", "CUDA_PATH", "FACIAL_CUDA_CANDIDATE_ROOT"] {
                    entries.push((name.into(), root.as_os_str().to_owned()));
                }
                // Prevent driver JIT disk writes before the verified GPU-scoped tract cache exists.
                entries.push(("CUDA_CACHE_DISABLE".into(), "1".into()));
                entries.sort_by_key(|(key, _)| key.to_string_lossy().to_uppercase());
                let mut block = Vec::<u16>::new();
                for (key, value) in entries {
                    block.extend(key.encode_wide());
                    block.push('=' as u16);
                    block.extend(value.encode_wide());
                    block.push(0);
                }
                block.push(0);
                block
            });
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_NO_WINDOW
                    | EXTENDED_STARTUPINFO_PRESENT
                    | BELOW_NORMAL_PRIORITY_CLASS
                    | if child_environment.is_some() {
                        CREATE_UNICODE_ENVIRONMENT
                    } else {
                        0
                    },
                child_environment
                    .as_mut()
                    .map_or(std::ptr::null_mut(), |block| block.as_mut_ptr() as _),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            ) == 0
            {
                return Err(last_error());
            }
            let process = Process {
                process: OwnedHandle::from_raw_handle(info.hProcess),
                job,
                resources: Some(resources),
            };
            drop(OwnedHandle::from_raw_handle(info.hThread));
            drop((child_input, child_output, child_errors));
            let mut errors = std::fs::File::from(errors);
            std::thread::Builder::new()
                .name("match-worker-stderr".into())
                .spawn(move || {
                    // Discard raw runtime diagnostics (may contain paths); bounded memory.
                    let mut buffer = [0; 4096];
                    while errors.read(&mut buffer).is_ok_and(|count| count > 0) {}
                })
                .map_err(|e| e.to_string())?;
            Ok((
                process,
                std::fs::File::from(input),
                std::fs::File::from(output),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_image_header_budget_is_derived_and_rejects_overflow() {
        let mut info = WorkerImageInfo {
            file_size: 32,
            working_bytes: 32 * 32 * 24,
            exif_orientation: 1,
            image_w: 32,
            image_h: 32,
        };
        assert!(valid_image_info(&info));
        info.file_size = 0;
        assert!(!valid_image_info(&info));
        info.file_size = 32;
        info.working_bytes -= 1;
        assert!(!valid_image_info(&info));
        info.image_w = u32::MAX;
        info.image_h = u32::MAX;
        info.working_bytes = 0;
        assert!(!valid_image_info(&info));
        assert!(inspect_image_bytes(b"invalid image header").is_err());
    }
    #[test]
    fn worker_protocol_rejects_oversized_header_before_payload() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_le_bytes().as_slice().to_vec();
        assert!(read_frame::<Response>(&mut bytes.as_slice())
            .unwrap_err()
            .contains("length"));
        bytes.clear();
    }
    #[test]
    fn worker_output_rejects_wrong_generation_and_nonfinite_vector() {
        let mut face = WorkerFace {
            bbox: [0.; 4],
            score: 1.,
            landmarks: [[0.; 2]; 5],
            bbox_normalized: [0.; 4],
            landmarks_normalized: [[0.; 2]; 5],
            detection_score: 1.,
            face_fraction: 0.1,
            alignment_valid: true,
            embedding: vec![0.; 512],
            embedding_dim: 512,
            generation: "g1".into(),
        };
        face.embedding[0] = 1.;
        let mut batch = WorkerFaceBatch {
            image_w: 1,
            image_h: 1,
            faces: vec![face],
            failures: vec![],
        };
        assert!(valid_batch(&batch, "g1"));
        assert!(!valid_batch(&batch, "g2"));
        batch.faces[0].embedding[3] = f32::NAN;
        assert!(!valid_batch(&batch, "g1"));
    }

    #[test]
    fn worker_request_binary_image_round_trip_is_bounded() {
        let request = Request {
            protocol: 1,
            worker_id: uuid::Uuid::new_v4().to_string(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            fence: fence(),
            body: Operation::Embed {
                encoded: vec![0, 255, 13, 10],
                source_path: PathBuf::from("image.jpg"),
            },
        };
        let mut buffer = Vec::new();
        write_request(&mut buffer, &request).unwrap();
        let decoded = read_request(&mut buffer.as_slice()).unwrap();
        assert_eq!(decoded.fence, request.fence);
        assert!(
            matches!(decoded.body, Operation::Embed { encoded, .. } if encoded == [0, 255, 13, 10])
        );
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_discovery_worker_preserves_exclusions_stat_failure_and_fresh_restart() {
        let directory =
            std::env::temp_dir().join(format!("facial-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(directory.join("excluded")).unwrap();
        std::fs::write(directory.join("excluded").join("hidden.jpg"), b"hidden").unwrap();
        std::fs::write(directory.join("visible.jpg"), b"visible").unwrap();
        let root = directory.canonicalize().unwrap();
        let mut worker = harness_worker();
        let fence = fence();
        worker
            .begin_discovery(&root, &["excluded".into()], &fence)
            .unwrap();
        let mut seen = Vec::new();
        while let Some(entry) = worker.discovery_next(&fence).unwrap() {
            let entry = entry.unwrap();
            seen.push(entry.path().strip_prefix(&root).unwrap().to_path_buf());
            if entry.is_file {
                assert_eq!(
                    worker
                        .discovery_metadata(entry.path(), &fence)
                        .unwrap()
                        .unwrap(),
                    7
                );
                std::fs::remove_file(entry.path()).unwrap();
                assert!(worker
                    .discovery_metadata(entry.path(), &fence)
                    .unwrap()
                    .is_err());
            }
        }
        assert_eq!(seen.len(), 2);
        assert!(seen.contains(&PathBuf::from("visible.jpg")));
        drop(worker);
        std::fs::write(root.join("visible.jpg"), b"visible").unwrap();
        let mut worker = harness_worker();
        worker
            .begin_discovery(&root, &["excluded".into()], &fence)
            .unwrap();
        let mut restarted = Vec::new();
        while let Some(entry) = worker.discovery_next(&fence).unwrap() {
            restarted.push(
                entry
                    .unwrap()
                    .path()
                    .strip_prefix(&root)
                    .unwrap()
                    .to_path_buf(),
            );
        }
        seen.sort();
        restarted.sort();
        assert_eq!(seen, restarted);
        let vanished = root.join("vanished");
        std::fs::create_dir(&vanished).unwrap();
        worker.begin_discovery(&vanished, &[], &fence).unwrap();
        std::fs::remove_dir(&vanished).unwrap();
        let failure = worker.discovery_next(&fence).unwrap().unwrap().unwrap_err();
        assert_eq!(failure.path(), Some(vanished.as_path()));
        assert!(worker
            .begin_discovery(&root.join("missing"), &[], &fence)
            .is_err());
        assert!(worker.discovery_next(&fence).is_err());
        drop(worker);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wp086_discovery_escaped_request_fits_reserved_begin_bytes() {
        let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let exclusions: Vec<String> = (0..1024)
            .map(|index| {
                format!(
                    "{}{}{}{}",
                    alphabet[index / 1296] as char,
                    alphabet[index / 36 % 36] as char,
                    alphabet[index % 36] as char,
                    "\"".repeat(1021)
                )
            })
            .collect();
        crate::match_discovery::validate_begin(Path::new("root"), &exclusions).unwrap();
        assert_eq!(
            exclusions.iter().map(String::len).sum::<usize>(),
            1024 * 1024
        );
        let request = Request {
            protocol: 1,
            worker_id: uuid::Uuid::new_v4().to_string(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            fence: fence(),
            body: Operation::DiscoveryBegin {
                root: PathBuf::from("root"),
                exclusions,
            },
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(encoded.len() > crate::match_discovery::CURSOR_BYTES as usize);
        assert!(encoded.len() <= crate::match_discovery::BEGIN_REQUEST_BYTES as usize);
        assert!(crate::match_discovery::validate_begin(
            Path::new("root"),
            &vec!["x".repeat(1024); 1025]
        )
        .is_err());
    }

    #[test]
    fn wp086_private_cpu_executor_scope_restores_baseline_and_rejects_wrong_fence() {
        use tract_linalg::multithread::{current_tract_executor, Executor};
        let admitted = fence();
        assert!(matches!(current_tract_executor(), Executor::SingleThread));
        let executor = Some((
            Executor::multithread_with_name(2, "facial-match-test"),
            admitted.clone(),
        ));
        let threads = cpu_candidate_scope(
            &executor,
            crate::match_acceleration::ProbeRuntime::CpuTwoThread,
            &admitted,
            || match current_tract_executor() {
                Executor::MultiThread(pool) => pool.current_num_threads(),
                _ => 0,
            },
        )
        .unwrap();
        assert_eq!(threads, 2);
        let Executor::MultiThread(pool) = &executor.as_ref().unwrap().0 else {
            unreachable!()
        };
        let names = std::sync::Mutex::new(std::collections::BTreeSet::new());
        let mut parallel = vec![0usize; 65_536];
        cpu_candidate_scope(
            &executor,
            crate::match_acceleration::ProbeRuntime::CpuTwoThread,
            &admitted,
            || {
                tract_linalg::multithread::par_chunks_mut(
                    &mut parallel,
                    1024,
                    65_536,
                    |first, chunk| {
                        names.lock().unwrap().insert((
                            pool.current_thread_index(),
                            std::thread::current().name().unwrap_or("").to_owned(),
                        ));
                        for (offset, value) in chunk.iter_mut().enumerate() {
                            *value = first * 1024 + offset;
                        }
                        Ok(())
                    },
                )
            },
        )
        .unwrap()
        .unwrap();
        let observed = names.lock().unwrap();
        assert!(!observed.is_empty());
        for (index, name) in observed.iter() {
            assert!(matches!(index, Some(0 | 1)));
            assert_eq!(name, &format!("facial-match-test-{}", index.unwrap()));
        }
        drop(observed);
        assert!(matches!(current_tract_executor(), Executor::SingleThread));
        let baseline_calls = std::sync::atomic::AtomicUsize::new(0);
        let mut baseline = vec![0usize; 65_536];
        tract_linalg::multithread::par_chunks_mut(&mut baseline, 1024, 65_536, |first, chunk| {
            baseline_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for (offset, value) in chunk.iter_mut().enumerate() {
                *value = first * 1024 + offset;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(baseline_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(parallel, baseline);
        let mut stale = admitted.clone();
        stale.admission_epoch += 1;
        assert!(cpu_candidate_scope(
            &executor,
            crate::match_acceleration::ProbeRuntime::CpuTwoThread,
            &stale,
            || ()
        )
        .is_err());
        assert!(cpu_candidate_scope(
            &executor,
            crate::match_acceleration::ProbeRuntime::Cpu,
            &admitted,
            || ()
        )
        .is_err());
    }

    #[test]
    fn wp086_two_thread_worker_reserves_both_cpu_units_until_confirmed_exit() {
        let mut first = IsolatedMatchWorker::spawn_cpu_two_thread_candidate().unwrap();
        assert!(matches!(
            first
                .execute(Operation::CpuExecutorBegin, &fence())
                .unwrap(),
            Output::CpuExecutorReady { threads: 2 }
        ));
        // Remove preparation bytes: the next launch must be rejected by CPU admission.
        first.process.release_preparation_bytes().unwrap();
        assert!(IsolatedMatchWorker::spawn_cpu_two_thread_candidate().is_err());
        assert!(first.shutdown_and_confirm());
        first.release_dead_preparation_resources();
        let mut fresh = IsolatedMatchWorker::spawn_cpu_two_thread_candidate().unwrap();
        assert!(matches!(
            fresh
                .execute(Operation::CpuExecutorBegin, &fence())
                .unwrap(),
            Output::CpuExecutorReady { threads: 2 }
        ));
        assert!(fresh.shutdown_and_confirm());
    }

    #[test]
    fn wp086_production_cpu_pool_accepts_later_asset_but_rejects_model_change() {
        let first = fence();
        let pool = Some((
            tract_linalg::multithread::Executor::multithread_with_name(
                2,
                "facial-match-production-test",
            ),
            first.model_generation.clone(),
        ));
        let mut later = first.clone();
        later.asset_id = "later-asset".into();
        later.media_key = "later-media".into();
        later.media_fingerprint = "later-fingerprint".into();
        later.admission_epoch += 1;
        later.track_id = Some("later-track".into());
        later.timestamp_ms = Some(1500);
        let observed = production_cpu_scope(&pool, &later, || {
            match tract_linalg::multithread::current_tract_executor() {
                tract_linalg::multithread::Executor::MultiThread(pool) => {
                    pool.current_num_threads()
                }
                _ => 0,
            }
        })
        .unwrap();
        assert_eq!(observed, 2);
        assert!(matches!(
            tract_linalg::multithread::current_tract_executor(),
            tract_linalg::multithread::Executor::SingleThread
        ));
        later.model_generation = "changed-model".into();
        let called = std::cell::Cell::new(false);
        assert!(production_cpu_scope(&pool, &later, || called.set(true)).is_err());
        assert!(!called.get());
    }

    fn fence() -> WorkerFence {
        WorkerFence {
            job_id: "job".into(),
            asset_id: "asset".into(),
            media_key: "media".into(),
            schema_generation: "schema".into(),
            identity_revision: 1,
            catalog_revision: 1,
            admission_epoch: 1,
            model_generation: "fixture-generation".into(),
            media_fingerprint: "fixture-fingerprint".into(),
            track_id: None,
            timestamp_ms: None,
        }
    }

    #[cfg(all(windows, debug_assertions))]
    fn harness_worker() -> IsolatedMatchWorker {
        let executable = std::env::current_exe().unwrap();
        let binary = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("facial-cli.exe");
        IsolatedMatchWorker::spawn_at(&binary, true)
            .expect("build facial-cli before worker process fault probes")
    }

    #[test]
    fn wp086_worker_executable_self_hosts_portable_and_cli() {
        assert_eq!(
            crate::run_gui(&["__match-worker-v1".into(), "invalid-worker-id".into()]),
            2
        );
        for name in ["facial-portable-0.1.8.exe", "facial.exe", "facial-cli.exe"] {
            let current = Path::new("isolated").join(name);
            assert_eq!(worker_executable(&current, false).unwrap(), current);
        }
        let current = Path::new("build").join("deps").join("facial-tests.exe");
        assert_eq!(
            worker_executable(&current, true).unwrap(),
            Path::new("build").join(if cfg!(windows) {
                "facial-cli.exe"
            } else {
                "facial-cli"
            })
        );
    }

    #[test]
    #[cfg(windows)]
    fn wp086_actual_portable_gui_worker_is_hidden_without_cli_sibling() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
        };
        unsafe extern "system" fn inspect(
            window: windows_sys::Win32::Foundation::HWND,
            data: isize,
        ) -> i32 {
            let state = unsafe { &mut *(data as *mut (u32, bool)) };
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(window, &mut pid);
            }
            if pid == state.0 && unsafe { IsWindowVisible(window) } != 0 {
                state.1 = true;
            }
            1
        }
        let current = std::env::current_exe().unwrap();
        let cli = worker_executable(&current, true).unwrap();
        let gui = cli.parent().unwrap().join("facial.exe");
        assert!(gui.is_file(), "build --bins before actual GUI-worker proof");
        let root = std::env::temp_dir().join(format!(
            "facial-portable-worker-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let portable = root.join("facial-portable-proof.exe");
        std::fs::copy(&gui, &portable).unwrap();
        let source = root.join("source.dat");
        std::fs::write(&source, b"owned portable worker proof").unwrap();
        assert!(!root.join("facial-cli.exe").exists());
        let mut worker = IsolatedMatchWorker::spawn_at(&portable, false).unwrap();
        let result = worker.begin_source(&source, &root, &fence());
        let mut visible = (worker.process.id(), false);
        unsafe {
            EnumWindows(Some(inspect), &mut visible as *mut _ as isize);
        }
        let exited = worker.shutdown_and_confirm();
        drop(worker);
        std::fs::remove_dir_all(&root).unwrap();
        result
            .expect("GUI portable must execute typed worker operation before application startup");
        assert!(exited, "owned GUI worker must exit");
        assert!(!visible.1, "hidden worker must not create a visible window");
    }

    #[cfg(all(windows, debug_assertions))]
    fn confirm_exit(worker: &IsolatedMatchWorker) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.confirmed_dead() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            worker.confirmed_dead(),
            "owned worker did not exit after quarantine"
        );
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_hung_worker_times_out_is_quarantined_and_cannot_be_reused() {
        let mut worker = harness_worker();
        let started = Instant::now();
        let error = worker
            .execute(
                Operation::Harness {
                    fault: HarnessFault::Hang,
                },
                &fence(),
            )
            .unwrap_err();
        assert_eq!(error.code, "safe_unit_timeout");
        assert!(error.retryable && error.quarantined);
        // OS dispatch tolerance is separate from the exact 2000 ms logical deadline.
        assert!(started.elapsed() < SAFE_UNIT_LIMIT + Duration::from_millis(250));
        assert_eq!(
            worker
                .execute(
                    Operation::Harness {
                        fault: HarnessFault::Crash
                    },
                    &fence()
                )
                .unwrap_err()
                .code,
            "worker_quarantined"
        );
        confirm_exit(&worker);
        let retry = IsolatedMatchWorker::spawn_retry(&mut worker).unwrap();
        assert_ne!(retry.worker_id(), worker.worker_id());
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_worker_rejects_late_fence_and_crash() {
        let mut stale = harness_worker();
        assert_eq!(
            stale
                .execute(
                    Operation::Harness {
                        fault: HarnessFault::LateFence
                    },
                    &fence()
                )
                .unwrap_err()
                .code,
            "worker_stale_result"
        );
        confirm_exit(&stale);
        let mut crashed = harness_worker();
        assert_eq!(
            crashed
                .execute(
                    Operation::Harness {
                        fault: HarnessFault::Crash
                    },
                    &fence()
                )
                .unwrap_err()
                .code,
            "worker_transport_failed"
        );
        confirm_exit(&crashed);
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_preparation_progress_flood_and_wrong_fence_quarantine_owned_worker() {
        for (path, message, has_phase) in [
            (
                "__wp086-progress-flood__",
                "worker preparation progress exceeded bound",
                true,
            ),
            (
                "__wp086-progress-fence__",
                "worker progress fence mismatch",
                false,
            ),
        ] {
            let mut worker = harness_worker();
            let started = Instant::now();
            let error = worker
                .prepare_candidate(
                    Path::new(path),
                    crate::match_acceleration::ProbeRuntime::Cpu,
                    &fence(),
                )
                .unwrap_err();
            assert_eq!(error.code, "worker_transport_failed");
            assert_eq!(error.message, message);
            assert_eq!(error.last_phase.is_some(), has_phase);
            assert_eq!(!error.phase_arrivals.is_empty(), has_phase);
            assert!(error.phase_arrivals.len() <= 16);
            assert!(error
                .phase_arrivals
                .windows(2)
                .all(|pair| pair[0].elapsed_micros <= pair[1].elapsed_micros));
            assert!(error.quarantined && error.retryable);
            assert!(started.elapsed() < SAFE_UNIT_LIMIT + Duration::from_millis(250));
            confirm_exit(&worker);
        }
    }

    #[cfg(all(test, windows, debug_assertions))]
    mod resident_memory_tests {
        use super::*;
        #[test]
        fn wp086_resident_worker_charge_survives_ready_and_ends_after_exit() {
            use crate::match_store::{MatchResourceGovernor, ResourceBudget, ResourceRequest};
            let cap = crate::match_store::WORKER_MEMORY_LIMIT_BYTES;
            assert!(
                cap > 0,
                "set measured worker memory policy before runtime proof"
            );
            let governor = MatchResourceGovernor::new(ResourceBudget {
                worker_memory_bytes: cap,
                ..Default::default()
            })
            .unwrap();
            let request = ResourceRequest {
                queued_bytes: crate::identity::PREPARATION_BUFFER_BYTES,
                worker_memory_bytes: cap,
                ..Default::default()
            };
            let lease = governor.try_acquire(request).unwrap();
            let current = std::env::current_exe().unwrap();
            let mut worker = IsolatedMatchWorker::spawn_at_with_resources(
                &worker_executable(&current, true).unwrap(),
                true,
                lease,
            )
            .unwrap();
            assert!(governor.try_acquire(request).is_err());
            worker.process.release_preparation_bytes().unwrap();
            assert_eq!(governor.usage().unwrap().queued_bytes, 0);
            assert_eq!(governor.usage().unwrap().worker_memory_bytes, cap);
            worker.release_dead_preparation_resources();
            assert_eq!(governor.usage().unwrap().worker_memory_bytes, cap);
            assert!(worker.shutdown_and_confirm());
            worker.release_dead_preparation_resources();
            assert_eq!(governor.usage().unwrap().worker_memory_bytes, 0);
            let replacement = governor.try_acquire(request).unwrap();
            drop(worker);
            assert_eq!(governor.usage().unwrap().worker_memory_bytes, cap);
            drop(replacement);
            assert_eq!(governor.usage().unwrap().worker_memory_bytes, 0);
        }
    }

    #[test]
    #[cfg(all(windows, debug_assertions))]
    fn wp086_preparation_progress_cannot_extend_original_watchdog() {
        let mut worker = harness_worker();
        let started = Instant::now();
        let error = worker
            .prepare_candidate(
                Path::new("__wp086-progress-hang__"),
                crate::match_acceleration::ProbeRuntime::Cpu,
                &fence(),
            )
            .unwrap_err();
        assert_eq!(error.code, "safe_unit_timeout");
        assert!(matches!(
            error.last_phase,
            Some(crate::identity::PreparationPhase::ManifestRead)
        ));
        assert!(error.quarantined && error.retryable);
        assert!(!error.phase_arrivals.is_empty());
        assert!(error.phase_arrivals.len() <= 12);
        assert!(error
            .phase_arrivals
            .windows(2)
            .all(|pair| pair[0].elapsed_micros <= pair[1].elapsed_micros));
        // Twelve accepted progress frames span 1.2 seconds. Restarting the
        // watchdog per frame would exceed this original two-second bound.
        assert!(started.elapsed() < SAFE_UNIT_LIMIT + Duration::from_millis(250));
        confirm_exit(&worker);
    }
}

#[cfg(test)]
#[path = "match_worker/cuda_pool_tests.rs"]
mod cuda_pool_tests;
