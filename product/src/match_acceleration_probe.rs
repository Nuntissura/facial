//! Explicit diagnostic runner. No configuration, database, or runtime promotion.
#[cfg(test)]
#[path = "match_acceleration_probe_tests.rs"]
mod real_boundary_tests;
use crate::{
    match_acceleration::{self, ProbeFrame, ProbeRuntime},
    match_worker::{IsolatedMatchWorker, WorkerFence},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};

struct Options {
    manifest: PathBuf,
    image: PathBuf,
    samples: usize,
    cpu_only: bool,
    cpu_two_thread: bool,
    candidate_root: Option<PathBuf>,
}
fn parse(args: &[String]) -> Result<Options, &'static str> {
    let mut manifest = None;
    let mut image = None;
    let mut samples = None;
    let mut cpu_only = false;
    let mut cpu_two_thread = false;
    let mut candidate_root = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--cpu-two-thread" {
            if cpu_two_thread {
                return Err("argument_unknown_or_duplicate");
            }
            cpu_two_thread = true;
            continue;
        }
        if flag == "--cpu-only" {
            if cpu_only {
                return Err("argument_unknown_or_duplicate");
            }
            cpu_only = true;
            continue;
        }
        let value = args.next().ok_or("argument_value_missing")?;
        match flag.as_str() {
            "--candidate-root" if candidate_root.is_none() => {
                candidate_root = Some(PathBuf::from(value))
            }
            "--manifest" if manifest.is_none() => manifest = Some(PathBuf::from(value)),
            "--image" if image.is_none() => image = Some(PathBuf::from(value)),
            "--samples" if samples.is_none() => {
                let count = value.parse::<usize>().map_err(|_| "samples_invalid")?;
                if !(1..=20).contains(&count) {
                    return Err("samples_invalid");
                }
                samples = Some(count);
            }
            _ => return Err("argument_unknown_or_duplicate"),
        }
    }
    if cpu_two_thread && (cpu_only || candidate_root.is_some()) {
        return Err("candidate_options_conflict");
    }
    Ok(Options {
        manifest: manifest.ok_or("manifest_required")?,
        image: image.ok_or("image_required")?,
        samples: samples.unwrap_or(5),
        cpu_only,
        cpu_two_thread,
        candidate_root,
    })
}

/// Retain a deny-write/delete source handle until both children have stopped.
/// Each read is capped independently of the file's reported size.
fn read_pinned(path: &PathBuf, limit: u64) -> Result<(File, Vec<u8>), &'static str> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    }
    let mut file = options.open(path).map_err(|_| "input_open_failed")?;
    let metadata = file.metadata().map_err(|_| "input_metadata_failed")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit {
        return Err("input_size_rejected");
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input_read_failed")?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > limit {
        return Err("input_changed_or_oversize");
    }
    Ok((file, bytes))
}
fn code_only(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        code.into()
    } else {
        "worker_failed".into()
    }
}
struct Batch {
    cpu_executor_init_micros: Option<u64>,
    phase_arrivals: Vec<crate::match_worker::PhaseArrival>,
    last_phase: Option<crate::identity::PreparationPhase>,
    frames: Vec<ProbeFrame>,
    error: Option<String>,
    confirmed_dead: bool,
    memory_peaks: Option<(u64, u64)>,
    memory_error: Option<&'static str>,
    native_provenance: Option<crate::match_cuda_bootstrap::NativeProvenance>,
    managed_gpu_peaks: Option<crate::match_cuda_bootstrap::ManagedGpuPeaks>,
}
fn batch(options: &Options, image: &[u8], runtime: ProbeRuntime, fence: &WorkerFence) -> Batch {
    let spawn = match runtime {
        ProbeRuntime::Cpu => IsolatedMatchWorker::spawn_diagnostic_preparation(),
        ProbeRuntime::CpuTwoThread => IsolatedMatchWorker::spawn_cpu_two_thread_candidate(),
        ProbeRuntime::Cuda => match &options.candidate_root {
            Some(root) => IsolatedMatchWorker::spawn_cuda_candidate(root),
            None => {
                return Batch {
                    cpu_executor_init_micros: None,
                    phase_arrivals: vec![],
                    last_phase: None,
                    frames: vec![],
                    error: Some("candidate_root_required".into()),
                    confirmed_dead: true,
                    memory_peaks: None,
                    memory_error: Some("worker_not_started"),
                    managed_gpu_peaks: None,
                    native_provenance: None,
                }
            }
        },
    };
    let mut worker = match spawn {
        Ok(worker) => worker,
        Err(error) => {
            return Batch {
                cpu_executor_init_micros: None,
                phase_arrivals: error.phase_arrivals,
                last_phase: error.last_phase,
                frames: vec![],
                error: Some(code_only(&error.code)),
                confirmed_dead: true,
                memory_peaks: None,
                memory_error: Some("worker_not_started"),
                managed_gpu_peaks: None,
                native_provenance: None,
            }
        }
    };
    let mut native_provenance = None;
    let bootstrap_error = if runtime == ProbeRuntime::Cuda {
        match worker.bootstrap_candidate(options.candidate_root.as_ref().unwrap(), fence) {
            Ok(info) => {
                native_provenance = Some(info);
                None
            }
            Err(error) => Some(error),
        }
    } else {
        None
    };
    let mut frames = Vec::new();
    let mut last_phase = None;
    let mut error = match bootstrap_error.map_or_else(
        || worker.prepare_candidate_checkpointed(&options.manifest, runtime, fence),
        Err,
    ) {
        Ok(prepared)
            if prepared.runtime == runtime && prepared.generation == fence.model_generation =>
        {
            None
        }
        Ok(_) => Some("candidate_prepare_fence_mismatch".into()),
        Err(error) => {
            last_phase = error.last_phase;
            Some(code_only(&error.code))
        }
    };
    let phase_arrivals = worker.phase_arrivals();
    if error.is_none() {
        for _ in 0..options.samples {
            match worker.sample_candidate(image.to_vec(), runtime, fence) {
                Ok(frame)
                    if frame.prepared.runtime == runtime
                        && frame.prepared.generation == fence.model_generation
                        && frame.input_sha256 == fence.media_fingerprint =>
                {
                    frames.push(frame)
                }
                Ok(_) => {
                    error = Some("candidate_sample_fence_mismatch".into());
                    break;
                }
                Err(failure) => {
                    error = Some(code_only(&failure.code));
                    break;
                }
            }
        }
    }
    let managed_gpu_peaks = if runtime == ProbeRuntime::Cuda && error.is_none() {
        match worker.candidate_gpu_peaks(fence) {
            Ok(value) => value,
            Err(failure) => {
                error = Some(code_only(&failure.code));
                None
            }
        }
    } else {
        None
    };
    let before_exit = worker.memory_peaks();
    let confirmed_dead = worker.shutdown_and_confirm();
    let after_exit = worker.memory_peaks();
    let memory_error = before_exit
        .as_ref()
        .err()
        .copied()
        .or_else(|| after_exit.as_ref().err().copied());
    let memory_peaks = match (before_exit, after_exit) {
        (Ok(before), Ok(after)) => Some((before.0.max(after.0), before.1.max(after.1))),
        (Ok(peak), Err(_)) | (Err(_), Ok(peak)) => Some(peak),
        (Err(_), Err(_)) => None,
    };
    if !confirmed_dead {
        error = Some("worker_exit_unconfirmed".into());
    }
    Batch {
        cpu_executor_init_micros: worker.cpu_executor_init_micros(),
        phase_arrivals,
        last_phase,
        frames,
        error,
        confirmed_dead,
        memory_peaks,
        memory_error,
        native_provenance,
        managed_gpu_peaks,
    }
}
fn execute(options: Options) -> Result<(Value, i32), &'static str> {
    let (_manifest_handle, manifest_bytes) = read_pinned(&options.manifest, 64 * 1024)?;
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|_| "manifest_invalid")?;
    let generation = manifest
        .get("generation")
        .and_then(Value::as_str)
        .filter(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("manifest_generation_invalid")?
        .to_string();
    let (_image_handle, image) = read_pinned(&options.image, 8 * 1024 * 1024)?;
    let input_hash = format!("{:x}", Sha256::digest(&image));
    let fence = WorkerFence {
        job_id: uuid::Uuid::new_v4().to_string(),
        asset_id: "acceleration-probe".into(),
        media_key: input_hash.clone(),
        schema_generation: "candidate-probe-v1".into(),
        identity_revision: 0,
        catalog_revision: 0,
        admission_epoch: 0,
        model_generation: generation.clone(),
        media_fingerprint: input_hash.clone(),
        track_id: None,
        timestamp_ms: None,
    };
    let cpu = batch(&options, &image, ProbeRuntime::Cpu, &fence);
    let cpu_times: Vec<u64> = cpu.frames.iter().map(|f| f.inference_micros).collect();
    let mut report = json!({"candidate_only":true,"promoted":false,"selected_runtime":"cpu",
        "generation":generation,"input_sha256":input_hash,"requested_samples":options.samples,
        "cpu_inference_micros":cpu_times,"cpu_exit_confirmed":cpu.confirmed_dead});
    report["cpu_last_phase"] = json!(cpu.last_phase);
    report["candidate_requested"] = json!(!options.cpu_only);
    report["cpu_peak_process_bytes"] = json!(cpu.memory_peaks.map(|peak| peak.0));
    report["cpu_peak_job_bytes"] = json!(cpu.memory_peaks.map(|peak| peak.1));
    report["cpu_memory_error"] = json!(cpu.memory_error);
    report["cpu_phase_arrivals"] = json!(cpu.phase_arrivals);
    if cpu.error.is_some() || cpu.frames.len() != options.samples {
        report["cpu_usable"] = json!(false);
        report["candidate_parity"] = json!(false);
        report["error"] = json!(cpu.error.unwrap_or_else(|| "cpu_samples_incomplete".into()));
        return Ok((report, 1));
    }
    // A successful transport with no valid face vectors is not a usable baseline.
    if cpu.frames.iter().any(|f| f.faces.is_empty()) {
        report["cpu_usable"] = json!(false);
        report["candidate_parity"] = json!(false);
        report["error"] = json!("cpu_insufficient_face_evidence");
        return Ok((report, 1));
    }
    report["cpu_usable"] = json!(true);
    let first = &cpu.frames[0].prepared;
    report["runtime_version"] = json!(first.runtime_version);
    report["detector_sha256"] = json!(first.detector_sha256);
    report["embedder_sha256"] = json!(first.embedder_sha256);
    report["cpu_prepare_micros"] = json!(first.prepare_micros);
    report["cpu_max_prepare_unit_micros"] = json!(first.max_prepare_unit_micros);
    report["cpu_preparation_units"] = json!(first.preparation_units);
    if options.cpu_only {
        report["candidate_parity"] = json!(false);
        report["candidate_status"] = json!("not_requested");
        return Ok((report, 0));
    }
    if options.cpu_two_thread {
        let candidate = batch(&options, &image, ProbeRuntime::CpuTwoThread, &fence);
        let comparisons: Vec<_> = cpu
            .frames
            .iter()
            .enumerate()
            .map(|(index, baseline)| {
                let result = candidate.error.as_deref().map_or_else(
                    || {
                        candidate
                            .frames
                            .get(index)
                            .ok_or("candidate_samples_incomplete")
                            .and_then(|frame| {
                                match_acceleration::parity_for(
                                    baseline,
                                    frame,
                                    ProbeRuntime::CpuTwoThread,
                                )
                            })
                    },
                    Err,
                );
                json!({"candidate_parity":result.is_ok(),"reason":result.err()})
            })
            .collect();
        let parity = candidate.error.is_none()
            && candidate.frames.len() == options.samples
            && comparisons
                .iter()
                .all(|row| row["candidate_parity"] == true);
        report["candidate_runtime"] = json!("cpu_two_thread");
        report["candidate_parity"] = json!(parity);
        report["candidate_resource_promotion_eligible"] = json!(false);
        report["cpu_two_thread"] = json!({
            "configured_cpu_units":2,"private_pool_threads":candidate.cpu_executor_init_micros.map(|_| 2),"executor_init_micros":candidate.cpu_executor_init_micros,"exit_confirmed":candidate.confirmed_dead,
            "peak_process_bytes":candidate.memory_peaks.map(|v|v.0),"peak_job_bytes":candidate.memory_peaks.map(|v|v.1),
            "memory_error":candidate.memory_error,"last_phase":candidate.last_phase,"phase_arrivals":candidate.phase_arrivals,
            "prepared":candidate.frames.first().map(|frame|&frame.prepared),
            "inference_micros":candidate.frames.iter().map(|frame|frame.inference_micros).collect::<Vec<_>>(),
            "speed_ratios":cpu.frames.iter().zip(&candidate.frames).map(|(a,b)| {
                (b.inference_micros > 0).then(|| a.inference_micros as f64 / b.inference_micros as f64)
            }).collect::<Vec<_>>(),"samples":comparisons,"error":candidate.error
        });
        return Ok((report, if candidate.confirmed_dead { 0 } else { 1 }));
    }
    let cuda = batch(&options, &image, ProbeRuntime::Cuda, &fence);
    report["candidate_native_provenance"] = json!(cuda.native_provenance);
    report["candidate_managed_gpu_pool"] = json!(cuda.managed_gpu_peaks);
    report["candidate_gpu_overhead_measured"] = json!(false);
    report["candidate_resource_promotion_eligible"] = json!(false);
    report["cuda_last_phase"] = json!(cuda.last_phase);
    report["cuda_peak_process_bytes"] = json!(cuda.memory_peaks.map(|peak| peak.0));
    report["cuda_peak_job_bytes"] = json!(cuda.memory_peaks.map(|peak| peak.1));
    report["cuda_memory_error"] = json!(cuda.memory_error);
    report["cuda_phase_arrivals"] = json!(cuda.phase_arrivals);
    report["cuda_exit_confirmed"] = json!(cuda.confirmed_dead);
    report["cuda_inference_micros"] = json!(cuda
        .frames
        .iter()
        .map(|f| f.inference_micros)
        .collect::<Vec<_>>());
    let comparisons: Vec<_> = cpu
        .frames
        .iter()
        .enumerate()
        .map(|(index, cpu)| {
            if let Some(error) = &cuda.error {
                match_acceleration::compare(cpu, Err(error))
            } else {
                match_acceleration::compare(
                    cpu,
                    cuda.frames.get(index).ok_or("candidate_samples_incomplete"),
                )
            }
        })
        .collect();
    let parity =
        cuda.frames.len() == options.samples && comparisons.iter().all(|r| r.candidate_parity);
    report["candidate_parity"] = json!(parity);
    report["samples"] = json!(comparisons);
    report["candidate_error"] = json!(cuda.error);
    // CUDA absence or numerical rejection is a valid diagnostic result, never a promotion.
    Ok((report, if cuda.confirmed_dead { 0 } else { 1 }))
}
pub(crate) fn run(args: &[String]) -> i32 {
    let (report, code) = match parse(args).and_then(execute) {
        Ok(result) => result,
        Err(code) => (
            json!({"candidate_only":true,"promoted":false,"cpu_usable":false,"candidate_parity":false,"error":code}),
            1,
        ),
    };
    println!("{report}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wp086_cpu_two_thread_options_are_explicit_and_separate() {
        let base = ["--manifest", "m", "--image", "i", "--cpu-two-thread"];
        assert!(parse(&args(&base)).unwrap().cpu_two_thread);
        for tail in [
            vec!["--cpu-only"],
            vec!["--candidate-root", "cuda"],
            vec!["--cpu-two-thread"],
        ] {
            let mut values = base.to_vec();
            values.extend(tail);
            assert!(parse(&args(&values)).is_err());
        }
        assert!(
            !parse(&args(&["--manifest", "m", "--image", "i"]))
                .unwrap()
                .cpu_two_thread
        );
    }
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }
    #[test]
    fn wp086_acceleration_probe_arguments_are_explicit_and_bounded() {
        let baseline = ["--manifest", "m", "--image", "i", "--cpu-only"];
        assert!(parse(&args(&baseline)).unwrap().cpu_only);
        assert!(parse(&args(&[
            "--manifest",
            "m",
            "--image",
            "i",
            "--cpu-only",
            "--cpu-only"
        ]))
        .is_err());
        assert_eq!(
            parse(&args(&[
                "--manifest",
                "manifest.json",
                "--image",
                "image.png"
            ]))
            .unwrap()
            .samples,
            5
        );
        for count in ["0", "21", "-1", "x"] {
            assert!(parse(&args(&[
                "--manifest",
                "m",
                "--image",
                "i",
                "--samples",
                count
            ]))
            .is_err());
        }
        assert!(parse(&args(&["--manifest", "m"])).is_err());
        assert!(parse(&args(&[
            "--manifest",
            "m",
            "--image",
            "i",
            "--manifest",
            "other"
        ]))
        .is_err());
        assert!(parse(&args(&[
            "--manifest",
            "m",
            "--image",
            "i",
            "--enable",
            "cuda"
        ]))
        .is_err());
        assert_eq!(
            parse(&args(&[
                "--manifest",
                "m",
                "--image",
                "i",
                "--samples",
                "20"
            ]))
            .unwrap()
            .samples,
            20
        );
    }
}
