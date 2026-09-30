//! Candidate-only numerical comparison. No result authorizes a runtime switch.
//! tract 0.23.5's runtime facade registers `cpu` and feature-gated `cuda`;
//! CUDA preparation checks its native dependencies and transforms the same IR.
//! See https://github.com/sonos/tract (pinned implementation: tract-cuda 0.23.5).
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbeRuntime {
    Cpu,
    Cuda,
    CpuTwoThread,
}
impl ProbeRuntime {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Cpu | Self::CpuTwoThread => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProbePrepared {
    pub runtime: ProbeRuntime,
    pub runtime_version: String,
    pub generation: String,
    pub detector_sha256: String,
    pub embedder_sha256: String,
    pub prepare_micros: u64,
    pub max_prepare_unit_micros: u64,
    pub preparation_units: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProbeDetection {
    pub bbox: [f32; 4],
    pub landmarks: [[f32; 2]; 5],
    pub score: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProbeFace {
    pub detection_index: usize,
    pub bbox_normalized: [f32; 4],
    pub landmarks_normalized: [[f32; 2]; 5],
    pub detection_score: f32,
    pub face_fraction: f32,
    pub alignment_valid: bool,
    pub values: Vec<f32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProbeFailure {
    pub detection_index: usize,
    pub code: String,
}
/// IPC evidence only: never convert these vectors into trusted IdentityVector.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProbeFrame {
    pub prepared: ProbePrepared,
    pub input_sha256: String,
    pub image_w: u32,
    pub image_h: u32,
    pub detections: Vec<ProbeDetection>,
    pub faces: Vec<ProbeFace>,
    pub failures: Vec<ProbeFailure>,
    pub inference_micros: u64,
}
/// Frozen candidate parity policy, not an identity calibration/decision threshold.
pub(crate) const POLICY: &str = "wp086-candidate-parity-v1";
const PIXEL_ABS: f32 = 0.25;
const NORMALIZED_ABS: f32 = 0.0001;
const VECTOR_ABS: f32 = 0.0001;
const COSINE_ERROR: f64 = 0.00001;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProbeReport {
    pub policy: String,
    pub candidate_parity: bool,
    pub reason: String,
    pub selected_runtime: ProbeRuntime,
    pub promoted: bool,
    pub cpu_prepare_micros: u64,
    pub cuda_prepare_micros: Option<u64>,
    pub cpu_max_prepare_unit_micros: u64,
    pub cuda_max_prepare_unit_micros: Option<u64>,
    pub cpu_preparation_units: u32,
    pub cuda_preparation_units: Option<u32>,
    pub cpu_inference_micros: u64,
    pub cuda_inference_micros: Option<u64>,
    pub inference_speed_ratio: Option<f64>,
}
fn near(a: f32, b: f32, tolerance: f32) -> bool {
    a.is_finite() && b.is_finite() && (a - b).abs() <= tolerance
}
fn slice_near(a: &[f32], b: &[f32], tolerance: f32) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&a, &b)| near(a, b, tolerance))
}
fn points_near(a: &[[f32; 2]; 5], b: &[[f32; 2]; 5], tolerance: f32) -> bool {
    a.iter().zip(b).all(|(a, b)| slice_near(a, b, tolerance))
}
fn parity(cpu: &ProbeFrame, cuda: &ProbeFrame) -> Result<(), &'static str> {
    parity_for(cpu, cuda, ProbeRuntime::Cuda)
}
pub(crate) fn parity_for(
    cpu: &ProbeFrame,
    cuda: &ProbeFrame,
    expected: ProbeRuntime,
) -> Result<(), &'static str> {
    if cpu.prepared.runtime != ProbeRuntime::Cpu || cuda.prepared.runtime != expected {
        return Err("runtime_mismatch");
    }
    for frame in [cpu, cuda] {
        if frame.prepared.preparation_units == 0
            || frame.prepared.max_prepare_unit_micros > frame.prepared.prepare_micros
        {
            return Err("preparation_timing_invalid");
        }
        if frame.prepared.max_prepare_unit_micros >= 2_000_000
            || frame.inference_micros >= 2_000_000
        {
            return Err("safe_unit_deadline_exceeded");
        }
        let valid_hash = |v: &str| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit());
        if !valid_hash(&frame.input_sha256)
            || !valid_hash(&frame.prepared.detector_sha256)
            || !valid_hash(&frame.prepared.embedder_sha256)
        {
            return Err("invalid_evidence_hash");
        }
        let mut seen = vec![false; frame.detections.len()];
        for index in frame
            .faces
            .iter()
            .map(|v| v.detection_index)
            .chain(frame.failures.iter().map(|v| v.detection_index))
        {
            let Some(slot) = seen.get_mut(index) else {
                return Err("invalid_detection_partition");
            };
            if *slot {
                return Err("invalid_detection_partition");
            }
            *slot = true;
        }
        if seen.iter().any(|seen| !*seen) {
            return Err("invalid_detection_partition");
        }
    }
    if cpu.prepared.generation.is_empty()
        || cpu.prepared.generation != cuda.prepared.generation
        || cpu.prepared.runtime_version != cuda.prepared.runtime_version
        || cpu.prepared.detector_sha256 != cuda.prepared.detector_sha256
        || cpu.prepared.embedder_sha256 != cuda.prepared.embedder_sha256
    {
        return Err("model_mismatch");
    }
    if cpu.input_sha256 != cuda.input_sha256
        || cpu.image_w == 0
        || cpu.image_h == 0
        || (cpu.image_w, cpu.image_h) != (cuda.image_w, cuda.image_h)
    {
        return Err("input_mismatch");
    }
    if cpu.detections.len() != cuda.detections.len()
        || cpu.faces.len() != cuda.faces.len()
        || cpu.failures != cuda.failures
    {
        return Err("face_outcome_mismatch");
    }
    for (a, b) in cpu.detections.iter().zip(&cuda.detections) {
        if !slice_near(&a.bbox, &b.bbox, PIXEL_ABS)
            || !points_near(&a.landmarks, &b.landmarks, PIXEL_ABS)
            || !near(a.score, b.score, NORMALIZED_ABS)
        {
            return Err("detection_mismatch");
        }
    }
    if cpu.faces.is_empty() {
        return Err("insufficient_face_evidence");
    }
    for (a, b) in cpu.faces.iter().zip(&cuda.faces) {
        if a.detection_index != b.detection_index
            || a.detection_index >= cpu.detections.len()
            || !a.alignment_valid
            || !b.alignment_valid
            || !slice_near(&a.bbox_normalized, &b.bbox_normalized, NORMALIZED_ABS)
            || !points_near(
                &a.landmarks_normalized,
                &b.landmarks_normalized,
                NORMALIZED_ABS,
            )
            || !near(a.detection_score, b.detection_score, NORMALIZED_ABS)
            || !near(a.face_fraction, b.face_fraction, NORMALIZED_ABS)
        {
            return Err("geometry_mismatch");
        }
        if a.values.len() != crate::identity::EMBEDDING_DIM
            || !slice_near(&a.values, &b.values, VECTOR_ABS)
        {
            return Err("vector_mismatch");
        }
        let norm = |v: &[f32]| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
        let na = norm(&a.values);
        let nb = norm(&b.values);
        let dot: f64 = a
            .values
            .iter()
            .zip(&b.values)
            .map(|(&x, &y)| f64::from(x) * f64::from(y))
            .sum();
        if (na - 1.0).abs() > 0.001
            || (nb - 1.0).abs() > 0.001
            || (1.0 - dot / (na * nb)).abs() > COSINE_ERROR
        {
            return Err("vector_cosine_mismatch");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wp086_preparation_parity_checks_each_unit_not_total_time() {
        let mut cpu = frame(ProbeRuntime::Cpu);
        let mut cuda = frame(ProbeRuntime::Cuda);
        for value in [&mut cpu, &mut cuda] {
            value.prepared.prepare_micros = 3_000_000;
            value.prepared.max_prepare_unit_micros = 1_500_000;
            value.prepared.preparation_units = 2;
        }
        let report = compare(&cpu, Ok(&cuda));
        assert!(report.candidate_parity);
        assert_eq!(report.cpu_prepare_micros, 3_000_000);
        assert_eq!(report.cpu_max_prepare_unit_micros, 1_500_000);
        assert_eq!(report.cpu_preparation_units, 2);
        cuda.prepared.max_prepare_unit_micros = 2_000_001;
        assert_eq!(
            compare(&cpu, Ok(&cuda)).reason,
            "safe_unit_deadline_exceeded"
        );
        cuda.prepared.max_prepare_unit_micros = 2_000_000;
        assert!(!compare(&cpu, Ok(&cuda)).candidate_parity);
        cuda.prepared.max_prepare_unit_micros = 1_500_000;
        cuda.prepared.preparation_units = 0;
        assert_eq!(
            compare(&cpu, Ok(&cuda)).reason,
            "preparation_timing_invalid"
        );
    }
    #[test]
    fn wp086_two_thread_parity_cannot_be_mislabeled_as_cuda() {
        assert_eq!(ProbeRuntime::CpuTwoThread.name(), "cpu");
        let cpu = frame(ProbeRuntime::Cpu);
        let mut candidate = frame(ProbeRuntime::CpuTwoThread);
        assert!(parity_for(&cpu, &candidate, ProbeRuntime::CpuTwoThread).is_ok());
        assert_eq!(parity(&cpu, &candidate), Err("runtime_mismatch"));
        candidate.faces[0].values[0] += 0.5;
        assert!(parity_for(&cpu, &candidate, ProbeRuntime::CpuTwoThread).is_err());
    }
    fn frame(runtime: ProbeRuntime) -> ProbeFrame {
        let mut values = vec![0.0; crate::identity::EMBEDDING_DIM];
        values[0] = 1.0;
        ProbeFrame {
            prepared: ProbePrepared {
                runtime,
                runtime_version: "tract-0.23.5".into(),
                generation: "generation".into(),
                detector_sha256: "a".repeat(64),
                embedder_sha256: "b".repeat(64),
                prepare_micros: 100,
                max_prepare_unit_micros: 100,
                preparation_units: 1,
            },
            input_sha256: "c".repeat(64),
            image_w: 100,
            image_h: 100,
            detections: vec![ProbeDetection {
                bbox: [0.0, 0.0, 50.0, 50.0],
                landmarks: [[20.0, 20.0]; 5],
                score: 0.9,
            }],
            faces: vec![ProbeFace {
                detection_index: 0,
                bbox_normalized: [0.0, 0.0, 0.5, 0.5],
                landmarks_normalized: [[0.2, 0.2]; 5],
                detection_score: 0.9,
                face_fraction: 0.25,
                alignment_valid: true,
                values,
            }],
            failures: vec![],
            inference_micros: 100,
        }
    }
    #[test]
    fn wp086_candidate_parity_never_promotes_or_calibrates() {
        let cpu = frame(ProbeRuntime::Cpu);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.inference_micros = 50;
        let report = compare(&cpu, Ok(&cuda));
        assert!(report.candidate_parity);
        assert!(!report.promoted);
        assert_eq!(report.selected_runtime, ProbeRuntime::Cpu);
        assert_eq!(report.inference_speed_ratio, Some(2.0));
        let unavailable = compare(&cpu, Err("runtime_unavailable"));
        assert!(!unavailable.candidate_parity);
        assert_eq!(unavailable.cuda_prepare_micros, None);
        assert_eq!(unavailable.inference_speed_ratio, None);
    }
    #[test]
    fn wp086_candidate_checks_geometry_vectors_and_exact_evidence() {
        let cpu = frame(ProbeRuntime::Cpu);
        let rejected = |cuda: ProbeFrame| assert!(!compare(&cpu, Ok(&cuda)).candidate_parity);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.detections[0].landmarks[4][1] += 1.0;
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.faces[0].values[1] = 0.01;
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.faces[0].values[1] = f32::NAN;
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.faces[0].face_fraction += 0.01;
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.input_sha256 = "d".repeat(64);
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.prepared.generation.push('x');
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.failures.push(ProbeFailure {
            detection_index: 0,
            code: "invalid_alignment".into(),
        });
        rejected(cuda);
        let mut cuda = frame(ProbeRuntime::Cuda);
        cuda.prepared.prepare_micros = 2_000_001;
        cuda.prepared.max_prepare_unit_micros = 2_000_001;
        rejected(cuda);
    }
    #[test]
    fn wp086_candidate_empty_and_failed_faces_are_insufficient() {
        let mut cpu = frame(ProbeRuntime::Cpu);
        let mut cuda = frame(ProbeRuntime::Cuda);
        for frame in [&mut cpu, &mut cuda] {
            frame.detections.clear();
            frame.faces.clear();
        }
        assert_eq!(
            compare(&cpu, Ok(&cuda)).reason,
            "insufficient_face_evidence"
        );
        for frame in [&mut cpu, &mut cuda] {
            frame.detections.push(ProbeDetection {
                bbox: [0.0; 4],
                landmarks: [[0.0; 2]; 5],
                score: 0.8,
            });
            frame.failures.push(ProbeFailure {
                detection_index: 0,
                code: "invalid_alignment".into(),
            });
        }
        assert_eq!(
            compare(&cpu, Ok(&cuda)).reason,
            "insufficient_face_evidence"
        );
    }
}
/// Missing/failed CUDA is an honest fallback. The caller must supply successful
/// supervised CPU evidence; a failed CPU probe cannot use this constructor.
pub(crate) fn compare(cpu: &ProbeFrame, cuda: Result<&ProbeFrame, &str>) -> ProbeReport {
    let (candidate_parity, reason, candidate) = match cuda {
        Ok(candidate) => match parity(cpu, candidate) {
            Ok(()) => (true, "candidate_parity_only".to_string(), Some(candidate)),
            Err(reason) => (false, reason.to_string(), Some(candidate)),
        },
        Err(code) => (false, format!("candidate_unavailable:{code}"), None),
    };
    ProbeReport {
        policy: POLICY.into(),
        candidate_parity,
        reason,
        selected_runtime: ProbeRuntime::Cpu,
        promoted: false,
        cpu_prepare_micros: cpu.prepared.prepare_micros,
        cuda_prepare_micros: candidate.map(|v| v.prepared.prepare_micros),
        cpu_max_prepare_unit_micros: cpu.prepared.max_prepare_unit_micros,
        cuda_max_prepare_unit_micros: candidate.map(|v| v.prepared.max_prepare_unit_micros),
        cpu_preparation_units: cpu.prepared.preparation_units,
        cuda_preparation_units: candidate.map(|v| v.prepared.preparation_units),
        cpu_inference_micros: cpu.inference_micros,
        cuda_inference_micros: candidate.map(|v| v.inference_micros),
        inference_speed_ratio: candidate
            .filter(|v| v.inference_micros > 0)
            .map(|v| cpu.inference_micros as f64 / v.inference_micros as f64),
    }
}
