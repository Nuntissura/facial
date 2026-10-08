//! Optional, deterministic face-identity engine (Phase 2, Option A).
//!
//! Default CPU ONNX inference via `tract` — no Python or native CPU runtime. Models
//! are provisioned at runtime (never bundled). When no embedder is configured,
//! or loading fails, the engine stays disabled and the app reports
//! `identity: unavailable` instead of faking a verdict.
//!
//! Alignment is mandatory: YuNet detects every face, each valid 5-point shape
//! is aligned to the canonical ArcFace template, and invalid or missing faces
//! fail closed. Whole-image embedding is deliberately forbidden.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use image::RgbImage;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use statrs::distribution::{Beta, ContinuousCDF};
use tract::prelude::*;

mod preparation;
pub(crate) use preparation::{PreparationSession, PREPARATION_BUFFER_BYTES};

#[cfg(all(not(windows), unix))]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::{ffi::OsString, os::windows::ffi::OsStringExt, os::windows::io::AsRawHandle};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS},
};

/// Canonical ArcFace 5-point destination template for a 112x112 crop
/// (left eye, right eye, nose, left mouth, right mouth).
const ARCFACE_DST: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

const DET_INPUT: usize = 640;
/// Floor score for a face to be usable for alignment (kept low so a single
/// imperfect face still aligns). Face *counting* uses the separate, higher
/// `identity_count_threshold` from config, applied by the caller.
const DET_THRESHOLD: f32 = 0.6;
/// IoU cutoff for greedy non-max suppression (OpenCV FaceDetectorYN default).
const NMS_THRESHOLD: f32 = 0.3;
const EMBED_INPUT: usize = 112;
pub const EMBEDDING_DIM: usize = 512;
const RUNTIME_NAME: &str = "cpu";
const RUNTIME_VERSION: &str = "tract-0.23.5";
const DETECTOR_LAYOUT_VERSION: &str = "yunet-2023mar-12-plane-v1";
const ALIGNMENT_VERSION: &str = "arcface-five-point-112-v1";
const EXTERNAL_DATA_POLICY: &str = "forbidden-load-buffer-only";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IdentityError {
    pub code: String,
    pub message: String,
}

impl IdentityError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for IdentityError {}

type IdentityResult<T> = Result<T, IdentityError>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactManifest {
    pub role: String,
    pub relative_path: Option<String>,
    pub sha256: String,
    pub bytes: u64,
    pub provenance: String,
    pub license: String,
    pub external_data_allowed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceManifest {
    pub schema_version: u32,
    pub runtime: String,
    pub runtime_name: String,
    pub detector: ModelArtifactManifest,
    pub embedder: ModelArtifactManifest,
    pub detector_input: [usize; 4],
    pub embedder_input: [usize; 4],
    pub embedding_dim: usize,
    pub detector_preprocessing: String,
    pub embedder_preprocessing: String,
    pub embedding_normalization: String,
    pub detection_threshold: f32,
    pub nms_threshold: f32,
    pub detector_layout_version: String,
    pub alignment_version: String,
    pub external_data_policy: String,
    pub generation: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdentityVector {
    values: Vec<f32>,
    dimension: usize,
    generation: String,
}

impl IdentityVector {
    /// Rehydrate only independently validated, current canonical store evidence.
    pub(crate) fn from_persisted(values: Vec<f32>, generation: &str) -> IdentityResult<Self> {
        let norm = values
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>();
        if values.len() != EMBEDDING_DIM
            || values.iter().any(|v| !v.is_finite())
            || !norm.is_finite()
            || (norm - 1.0).abs() > 0.0001
            || generation.trim() != generation
            || generation.is_empty()
            || generation.len() > 1024
            || generation.chars().any(char::is_control)
        {
            return Err(IdentityError::new("invalid_persisted_vector","persisted evidence must have 512 finite unit-normalized values and a valid generation"));
        }
        Ok(Self::new(values, generation))
    }
    fn new(values: Vec<f32>, generation: &str) -> Self {
        Self {
            dimension: values.len(),
            values,
            generation: generation.to_string(),
        }
    }

    pub(crate) fn values(&self) -> &[f32] {
        &self.values
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FaceQualityInputs {
    pub detection_score: f32,
    pub face_fraction: f32,
    pub alignment_valid: bool,
}

#[derive(Clone, Debug)]
pub struct FaceEmbedding {
    pub face: Face,
    pub bbox_normalized: [f32; 4],
    pub landmarks_normalized: [[f32; 2]; 5],
    pub quality: FaceQualityInputs,
    pub embedding: IdentityVector,
    pub embedding_dim: usize,
    pub generation: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FaceFailure {
    pub detection_index: usize,
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub struct FaceBatch {
    pub image_w: u32,
    pub image_h: u32,
    pub faces: Vec<FaceEmbedding>,
    pub failures: Vec<FaceFailure>,
    pub image: RgbImage,
}

/// A single detected face in ORIGINAL image pixel coordinates.
#[derive(Clone, Debug, Serialize)]
pub struct Face {
    /// Bounding box: x, y (top-left), w, h.
    pub bbox: [f32; 4],
    /// Detection confidence `sqrt(cls*obj)` in [0,1].
    pub score: f32,
    /// 5 landmarks (left eye, right eye, nose, left mouth, right mouth).
    pub landmarks: [[f32; 2]; 5],
}

/// Result of embedding one image together with its face detections, so the
/// identity gate can report face box/count/scale without a second pass.
pub struct GateDetect {
    pub embedding: IdentityVector,
    /// YuNet alignment is mandatory for every emitted embedding.
    pub aligned: bool,
    pub image_w: u32,
    pub image_h: u32,
    /// All faces >= `DET_THRESHOLD`, post-NMS, sorted by score descending.
    pub faces: Vec<Face>,
    /// The decoded image, kept so curation metadata (face-crop sharpness,
    /// hair heuristic) computes without a second decode (WP-019).
    pub image: RgbImage,
    pub generation: String,
    pub embedding_dim: usize,
}

/// The YuNet 2023mar float model (OpenCV Zoo, MIT — license vendored beside
/// the asset) compiled into the binary as the DEFAULT detector (WP-020).
/// `identity_detector_path` / `FACIAL_IDENTITY_DETECTOR` override it.
pub const BUNDLED_YUNET: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/models/face_detection_yunet_2023mar.onnx"
));

/// Candidate engines are child-worker-only and never expose a trusted vector.
/// Preparation and each sample require separate supervised 2,000 ms units.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreparationPhase {
    ManifestRead,
    ManifestMetadata,
    ManifestCanonicalize,
    ManifestReadBytes,
    ManifestValidate,
    EmbedderReadHash,
    DetectorReadHash,
    DetectorParse,
    DetectorPrepare,
    DetectorStartup,
    EmbedderParse,
    EmbedderPrepare,
    EmbedderStartup,
}
pub(crate) struct AccelerationCandidateEngine {
    inner: IdentityEngine,
    prepared: crate::match_acceleration::ProbePrepared,
}

impl AccelerationCandidateEngine {
    pub(crate) fn from_prepared(
        inner: IdentityEngine,
        runtime: crate::match_acceleration::ProbeRuntime,
        total_micros: u64,
        max_prepare_unit_micros: u64,
        preparation_units: u32,
    ) -> IdentityResult<Self> {
        if preparation_units == 0 || max_prepare_unit_micros > total_micros {
            return Err(IdentityError::new(
                "preparation_timing_invalid",
                "preparation timing lacks valid units",
            ));
        }
        let actual = runtime_for_name(runtime.name())
            .and_then(|value| value.name())
            .map_err(|error| IdentityError::new("runtime_unavailable", error.to_string()))?;
        if actual != runtime.name() {
            return Err(IdentityError::new(
                "runtime_mismatch",
                "candidate runtime mismatch",
            ));
        }
        let prepared = crate::match_acceleration::ProbePrepared {
            runtime,
            runtime_version: RUNTIME_VERSION.into(),
            generation: inner.generation().into(),
            detector_sha256: inner.manifest.detector.sha256.clone(),
            embedder_sha256: inner.manifest.embedder.sha256.clone(),
            prepare_micros: total_micros,
            max_prepare_unit_micros,
            preparation_units,
        };
        Ok(Self { inner, prepared })
    }
    pub(crate) fn load(
        path: &Path,
        runtime: crate::match_acceleration::ProbeRuntime,
    ) -> IdentityResult<Self> {
        Self::load_with_progress(path, runtime, &mut |_| {})
    }
    pub(crate) fn load_with_progress(
        path: &Path,
        runtime: crate::match_acceleration::ProbeRuntime,
        progress: &mut dyn FnMut(PreparationPhase),
    ) -> IdentityResult<Self> {
        let started = std::time::Instant::now();
        let actual = runtime_for_name(runtime.name())
            .and_then(|runtime| runtime.name())
            .map_err(|error| IdentityError::new("runtime_unavailable", error.to_string()))?;
        if actual != runtime.name() {
            return Err(IdentityError::new(
                "runtime_mismatch",
                "candidate runtime facade selected a different backend",
            ));
        }
        // The existing CPU manifest remains authoritative for bytes and semantics;
        // candidate runtime selection never changes its identity generation.
        let inner = IdentityEngine::load_manifest_progress(path, runtime.name(), progress)?;
        let total_micros = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        let prepared = crate::match_acceleration::ProbePrepared {
            runtime,
            runtime_version: RUNTIME_VERSION.into(),
            generation: inner.generation().into(),
            detector_sha256: inner.manifest.detector.sha256.clone(),
            embedder_sha256: inner.manifest.embedder.sha256.clone(),
            prepare_micros: total_micros,
            max_prepare_unit_micros: total_micros,
            preparation_units: 1,
        };
        Ok(Self { inner, prepared })
    }

    pub(crate) fn prepared(&self) -> &crate::match_acceleration::ProbePrepared {
        &self.prepared
    }

    pub(crate) fn sample(
        &self,
        encoded: &[u8],
    ) -> IdentityResult<crate::match_acceleration::ProbeFrame> {
        use crate::match_acceleration::{ProbeDetection, ProbeFace, ProbeFailure, ProbeFrame};
        let started = std::time::Instant::now();
        let input_sha256 = sha256_hex(encoded);
        let image = image::load_from_memory(encoded)
            .map_err(|error| IdentityError::new("image_decode", error.to_string()))?
            .to_rgb8();
        let (image_w, image_h) = image.dimensions();
        let detections = self.inner.detector.detect_all(&image, DET_THRESHOLD)?;
        let geometry = detections
            .iter()
            .map(|face| ProbeDetection {
                bbox: face.bbox,
                landmarks: face.landmarks,
                score: face.score,
            })
            .collect();
        let (faces, failures) = if detections.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let batch = self.inner.embed_detections(image, detections)?;
            let mut indices = (0..batch.faces.len() + batch.failures.len()).filter(|index| {
                !batch
                    .failures
                    .iter()
                    .any(|failure| failure.detection_index == *index)
            });
            let faces = batch
                .faces
                .into_iter()
                .map(|face| ProbeFace {
                    detection_index: indices
                        .next()
                        .expect("each successful detection has an index"),
                    bbox_normalized: face.bbox_normalized,
                    landmarks_normalized: face.landmarks_normalized,
                    detection_score: face.quality.detection_score,
                    face_fraction: face.quality.face_fraction,
                    alignment_valid: face.quality.alignment_valid,
                    values: face.embedding.values().to_vec(),
                })
                .collect();
            let failures = batch
                .failures
                .into_iter()
                .map(|failure| ProbeFailure {
                    detection_index: failure.detection_index,
                    code: failure.code,
                })
                .collect();
            (faces, failures)
        };
        Ok(ProbeFrame {
            prepared: self.prepared.clone(),
            input_sha256,
            image_w,
            image_h,
            detections: geometry,
            faces,
            failures,
            inference_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        })
    }
}

pub struct IdentityEngine {
    model: Runnable,
    model_path: PathBuf,
    model_sha256: String,
    detector: Detector,
    /// Where the active detector came from: "override" (accepted configured
    /// path) or "bundled" (compiled-in YuNet).
    detector_origin: &'static str,
    detector_sha256: Option<String>,
    manifest: InferenceManifest,
}

impl IdentityEngine {
    /// Explicitly import selected files into an app-owned model root and write
    /// the immutable expected-hash manifest used by every later startup. Raw
    /// paths are accepted only at this operator-triggered provisioning boundary.
    pub fn provision(
        model_path: &Path,
        detector_path: Option<&Path>,
        manifest_path: &Path,
    ) -> IdentityResult<Self> {
        let (_, embedder_bytes) = read_import_artifact(model_path, "embedder")?;
        drop(load_embedder(&embedder_bytes)?);
        let embedder_sha = sha256_hex(&embedder_bytes);
        let root = manifest_path.parent().ok_or_else(|| {
            IdentityError::new("manifest_path_invalid", "manifest path has no model root")
        })?;
        std::fs::create_dir_all(root)
            .map_err(|e| IdentityError::new("manifest_write_failed", e.to_string()))?;
        let embedder_name = format!("embedder-{embedder_sha}.onnx");
        persist_import(root, &embedder_name, &embedder_bytes)?;

        let (detector_artifact, detector_sha) = match detector_path {
            Some(path) => {
                let (_, bytes) = read_import_artifact(path, "detector")?;
                drop(Detector::load_from_bytes(&bytes)?);
                let sha = sha256_hex(&bytes);
                let name = format!("detector-{sha}.onnx");
                persist_import(root, &name, &bytes)?;
                (
                    ModelArtifactManifest {
                        role: "detector".to_string(),
                        relative_path: Some(name),
                        sha256: sha.clone(),
                        bytes: bytes.len() as u64,
                        provenance: "operator-provisioned YuNet-compatible detector".to_string(),
                        license: "operator-authorized external asset; not redistributed by Facial"
                            .to_string(),
                        external_data_allowed: false,
                    },
                    sha,
                )
            }
            None => {
                let sha = sha256_hex(BUNDLED_YUNET);
                (
                    ModelArtifactManifest {
                        role: "detector".to_string(),
                        relative_path: None,
                        sha256: sha.clone(),
                        bytes: BUNDLED_YUNET.len() as u64,
                        provenance: "OpenCV Zoo YuNet 2023mar bundled with Facial".to_string(),
                        license: "MIT".to_string(),
                        external_data_allowed: false,
                    },
                    sha,
                )
            }
        };
        let mut manifest = InferenceManifest {
            schema_version: 1,
            runtime: RUNTIME_VERSION.to_string(),
            runtime_name: RUNTIME_NAME.to_string(),
            detector: detector_artifact,
            embedder: ModelArtifactManifest {
                role: "embedder".to_string(),
                relative_path: Some(embedder_name),
                sha256: embedder_sha,
                bytes: embedder_bytes.len() as u64,
                provenance: "operator-provisioned ArcFace-compatible embedder".to_string(),
                license: "operator-authorized external asset; not redistributed by Facial"
                    .to_string(),
                external_data_allowed: false,
            },
            detector_input: [1, 3, DET_INPUT, DET_INPUT],
            embedder_input: [1, 3, EMBED_INPUT, EMBED_INPUT],
            embedding_dim: EMBEDDING_DIM,
            detector_preprocessing: "letterbox-640;rgb-to-bgr;raw-0-255".to_string(),
            embedder_preprocessing: "five-point-yunet-similarity-align-112;rgb;(x-127.5)/128"
                .to_string(),
            embedding_normalization: "finite-l2-unit".to_string(),
            detection_threshold: DET_THRESHOLD,
            nms_threshold: NMS_THRESHOLD,
            detector_layout_version: DETECTOR_LAYOUT_VERSION.to_string(),
            alignment_version: ALIGNMENT_VERSION.to_string(),
            external_data_policy: EXTERNAL_DATA_POLICY.to_string(),
            generation: String::new(),
        };
        manifest.generation = generation_for_manifest(&manifest)?;
        let body = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| IdentityError::new("manifest_invalid", e.to_string()))?;
        let manifest_name = manifest_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("identity-manifest.json");
        let candidate_path = root.join(format!(
            ".{manifest_name}.candidate-{}",
            uuid::Uuid::new_v4()
        ));
        persist_atomic(&candidate_path, &body, false)?;
        let candidate_engine = Self::load_manifest(&candidate_path);
        let _ = std::fs::remove_file(&candidate_path);
        let engine = candidate_engine?;
        persist_atomic(manifest_path, &body, true)?;
        let _ = detector_sha;
        Ok(engine)
    }

    /// Load only from a predeclared manifest. Artifact paths are relative to
    /// the manifest's canonical parent, hashes and sizes are checked before
    /// ONNX parsing, and path traversal/symlink escapes fail closed.
    pub fn load_manifest(manifest_path: &Path) -> IdentityResult<Self> {
        Self::load_manifest_for_runtime(manifest_path, RUNTIME_NAME)
    }

    fn load_manifest_for_runtime(manifest_path: &Path, runtime_name: &str) -> IdentityResult<Self> {
        Self::load_manifest_progress(manifest_path, runtime_name, &mut |_| {})
    }
    fn load_manifest_progress(
        manifest_path: &Path,
        runtime_name: &str,
        progress: &mut dyn FnMut(PreparationPhase),
    ) -> IdentityResult<Self> {
        crate::match_benchmark::note_model_load();
        let (canonical_manifest, manifest_bytes) =
            read_import_artifact_traced(manifest_path, "manifest", progress)?;
        progress(PreparationPhase::ManifestValidate);
        let manifest: InferenceManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| IdentityError::new("manifest_invalid", e.to_string()))?;
        validate_manifest_contract(&manifest)?;
        let expected_generation = generation_for_manifest(&manifest)?;
        if manifest.generation != expected_generation {
            return Err(IdentityError::new(
                "generation_mismatch",
                "manifest generation does not match its semantic contents",
            ));
        }
        let root = canonical_manifest.parent().ok_or_else(|| {
            IdentityError::new(
                "manifest_path_invalid",
                "manifest has no canonical model root",
            )
        })?;
        progress(PreparationPhase::EmbedderReadHash);
        let (model_path, embedder_bytes) = secure_read_declared(root, &manifest.embedder)?;
        progress(PreparationPhase::DetectorReadHash);
        let (detector, detector_origin, detector_sha) =
            match manifest.detector.relative_path.as_deref() {
                Some(_) => {
                    let (_, bytes) = secure_read_declared(root, &manifest.detector)?;
                    (
                        Detector::load_progress(&bytes, runtime_name, progress)?,
                        "override",
                        manifest.detector.sha256.clone(),
                    )
                }
                None => {
                    verify_artifact_bytes(&manifest.detector, BUNDLED_YUNET)?;
                    (
                        Detector::load_progress(BUNDLED_YUNET, runtime_name, progress)?,
                        "bundled",
                        manifest.detector.sha256.clone(),
                    )
                }
            };
        let model = load_embedder_progress(&embedder_bytes, runtime_name, progress)?;
        Ok(Self {
            model,
            model_path,
            model_sha256: manifest.embedder.sha256.clone(),
            detector,
            detector_origin,
            detector_sha256: Some(detector_sha),
            manifest,
        })
    }

    pub fn detector_origin(&self) -> &'static str {
        self.detector_origin
    }

    pub fn detector_sha256(&self) -> Option<&str> {
        self.detector_sha256.as_deref()
    }

    pub fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn has_detector(&self) -> bool {
        true
    }

    pub fn align_method(&self) -> &'static str {
        "yunet_112"
    }

    pub fn manifest(&self) -> &InferenceManifest {
        &self.manifest
    }

    pub fn generation(&self) -> &str {
        &self.manifest.generation
    }

    pub fn embedding_dim(&self) -> usize {
        self.manifest.embedding_dim
    }

    /// Compatibility adapter for the existing identity gate: return the first
    /// deterministic valid face. It never embeds the whole image.
    pub fn embed_file(&self, image_path: &Path) -> IdentityResult<IdentityVector> {
        Ok(self.embed_with_detection(image_path)?.embedding)
    }

    pub fn embed_file_detail(&self, image_path: &Path) -> IdentityResult<(IdentityVector, bool)> {
        let d = self.embed_with_detection(image_path)?;
        Ok((d.embedding, d.aligned))
    }

    /// Decode once, detect once, and emit one embedding per valid face.
    pub fn embed_faces(&self, image_path: &Path) -> IdentityResult<FaceBatch> {
        let img = image::open(image_path)
            .map_err(|e| {
                IdentityError::new(
                    "image_decode_failed",
                    format!("{}: {e}", image_path.display()),
                )
            })?
            .to_rgb8();
        self.embed_faces_rgb(img)
    }

    /// Decode an already captured, caller-bounded source snapshot. Match uses
    /// this path so the bytes covered by its durable media fingerprint are the
    /// exact bytes consumed by detection and embedding; the source pathname is
    /// diagnostic context only and is never reopened here.
    pub(crate) fn embed_faces_bytes(
        &self,
        encoded: &[u8],
        source_path: &Path,
    ) -> IdentityResult<FaceBatch> {
        let img = image::load_from_memory(encoded)
            .map_err(|e| {
                IdentityError::new(
                    "image_decode_failed",
                    format!("{}: {e}", source_path.display()),
                )
            })?
            .to_rgb8();
        self.embed_faces_rgb(img)
    }

    /// Video sampling detects first and embeds only an admitted exemplar.
    pub(crate) fn detect_faces_bytes(
        &self,
        encoded: &[u8],
    ) -> IdentityResult<(u32, u32, Vec<Face>)> {
        let img = image::load_from_memory(encoded)
            .map_err(|e| IdentityError::new("image_decode_failed", e.to_string()))?
            .to_rgb8();
        let faces = self.detector.detect_all(&img, DET_THRESHOLD)?;
        Ok((img.width(), img.height(), faces))
    }

    /// Redetect from the same immutable frame bytes so caller geometry cannot
    /// redirect alignment. One face is the bounded video embedding safe unit.
    pub(crate) fn embed_video_exemplar_bytes(
        &self,
        encoded: &[u8],
        detection_index: usize,
    ) -> IdentityResult<FaceBatch> {
        let img = image::load_from_memory(encoded)
            .map_err(|e| IdentityError::new("image_decode_failed", e.to_string()))?
            .to_rgb8();
        let detections = self.detector.detect_all(&img, DET_THRESHOLD)?;
        let face = detections.get(detection_index).cloned().ok_or_else(|| {
            IdentityError::new(
                "stale_detection",
                "video exemplar detection no longer exists",
            )
        })?;
        self.embed_detections(img, vec![face])
    }

    fn embed_faces_rgb(&self, img: RgbImage) -> IdentityResult<FaceBatch> {
        let (image_w, image_h) = img.dimensions();
        if image_w == 0 || image_h == 0 {
            return Err(IdentityError::new(
                "image_dimensions_invalid",
                "decoded image is empty",
            ));
        }
        let detections = self.detector.detect_all(&img, DET_THRESHOLD)?;
        self.embed_detections(img, detections)
    }

    fn embed_detections(&self, img: RgbImage, detections: Vec<Face>) -> IdentityResult<FaceBatch> {
        let (image_w, image_h) = img.dimensions();
        if detections.is_empty() {
            return Err(IdentityError::new(
                "missing_face",
                "YuNet found no face above the alignment floor",
            ));
        }
        let mut faces = Vec::with_capacity(detections.len());
        let mut failures = Vec::new();
        for (detection_index, face) in detections.into_iter().enumerate() {
            let result = validate_face(&face, image_w, image_h)
                .and_then(|_| {
                    align_112(&img, &face.landmarks).ok_or_else(|| {
                        IdentityError::new(
                            "invalid_alignment",
                            "five-point similarity transform is singular",
                        )
                    })
                })
                .and_then(|aligned| self.embed_aligned(&aligned));
            match result {
                Ok(embedding) => {
                    let bbox_normalized = normalize_bbox(face.bbox, image_w, image_h);
                    let landmarks_normalized =
                        normalize_landmarks(face.landmarks, image_w, image_h);
                    faces.push(FaceEmbedding {
                        quality: FaceQualityInputs {
                            detection_score: face.score,
                            face_fraction: bbox_normalized[2] * bbox_normalized[3],
                            alignment_valid: true,
                        },
                        face,
                        bbox_normalized,
                        landmarks_normalized,
                        embedding: IdentityVector::new(embedding, self.generation()),
                        embedding_dim: self.embedding_dim(),
                        generation: self.generation().to_string(),
                    });
                }
                Err(error) => failures.push(FaceFailure {
                    detection_index,
                    code: error.code,
                    message: error.message,
                }),
            }
        }
        Ok(FaceBatch {
            image_w,
            image_h,
            faces,
            failures,
            image: img,
        })
    }

    pub fn embed_with_detection(&self, image_path: &Path) -> IdentityResult<GateDetect> {
        let mut batch = self.embed_faces(image_path)?;
        let Some(first) = batch.faces.first() else {
            let failure = batch.failures.first();
            return Err(IdentityError::new(
                failure
                    .map(|v| v.code.as_str())
                    .unwrap_or("invalid_alignment"),
                failure
                    .map(|v| v.message.as_str())
                    .unwrap_or("no detection produced a valid alignment"),
            ));
        };
        let embedding = first.embedding.clone();
        let faces = batch.faces.iter().map(|value| value.face.clone()).collect();
        Ok(GateDetect {
            embedding,
            aligned: true,
            image_w: batch.image_w,
            image_h: batch.image_h,
            faces,
            image: std::mem::take(&mut batch.image),
            generation: self.generation().to_string(),
            embedding_dim: self.embedding_dim(),
        })
    }

    fn embed_aligned(&self, img: &RgbImage) -> IdentityResult<Vec<f32>> {
        if img.dimensions() != (EMBED_INPUT as u32, EMBED_INPUT as u32) {
            return Err(IdentityError::new(
                "invalid_alignment",
                "aligned crop is not 112x112",
            ));
        }
        let mut data = vec![0f32; 3 * EMBED_INPUT * EMBED_INPUT];
        for y in 0..EMBED_INPUT {
            for x in 0..EMBED_INPUT {
                let px = img.get_pixel(x as u32, y as u32);
                for c in 0..3usize {
                    data[c * EMBED_INPUT * EMBED_INPUT + y * EMBED_INPUT + x] =
                        (px[c] as f32 - 127.5) / 128.0;
                }
            }
        }
        let input = Tensor::from_slice(&[1, 3, EMBED_INPUT, EMBED_INPUT], &data)
            .map_err(|e| IdentityError::new("tensor_invalid", e.to_string()))?;
        let result = self
            .model
            .run([input])
            .map_err(|e| IdentityError::new("inference_failed", e.to_string()))?;
        if result.len() != 1 {
            return Err(IdentityError::new(
                "malformed_output",
                format!(
                    "embedder returned {} outputs, expected exactly one",
                    result.len()
                ),
            ));
        }
        let output = &result[0];
        let shape = output
            .shape()
            .map_err(|e| IdentityError::new("malformed_output", e.to_string()))?;
        if shape != [1, EMBEDDING_DIM] {
            return Err(IdentityError::new(
                "dimension_mismatch",
                format!(
                    "embedder returned shape {:?}, expected [1, {}]",
                    shape, EMBEDDING_DIM
                ),
            ));
        }
        let view = output
            .as_slice::<f32>()
            .map_err(|e| IdentityError::new("malformed_output", e.to_string()))?;
        let mut emb: Vec<f32> = view.iter().copied().collect();
        if emb.len() != self.embedding_dim() {
            return Err(IdentityError::new(
                "dimension_mismatch",
                format!(
                    "embedder returned {}, manifest declares {}",
                    emb.len(),
                    self.embedding_dim()
                ),
            ));
        }
        normalize_embedding(&mut emb)?;
        Ok(emb)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn read_import_artifact(path: &Path, role: &str) -> IdentityResult<(PathBuf, Vec<u8>)> {
    read_import_artifact_traced(path, role, &mut |_| {})
}
fn read_import_artifact_traced(
    path: &Path,
    role: &str,
    progress: &mut dyn FnMut(PreparationPhase),
) -> IdentityResult<(PathBuf, Vec<u8>)> {
    progress(PreparationPhase::ManifestMetadata);
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        IdentityError::new("model_missing", format!("{role} {}: {e}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(IdentityError::new(
            "unsafe_artifact_path",
            format!(
                "{role} must be a regular non-symlink file: {}",
                path.display()
            ),
        ));
    }
    progress(PreparationPhase::ManifestCanonicalize);
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| IdentityError::new("unsafe_artifact_path", format!("{role}: {e}")))?;
    progress(PreparationPhase::ManifestReadBytes);
    let bytes = std::fs::read(&canonical)
        .map_err(|e| IdentityError::new("model_read_failed", format!("{role}: {e}")))?;
    if bytes.is_empty() {
        return Err(IdentityError::new(
            "model_empty",
            format!("{role} is empty"),
        ));
    }
    Ok((canonical, bytes))
}

fn validate_relative_path(value: &str, role: &str) -> IdentityResult<PathBuf> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(IdentityError::new(
            "unsafe_artifact_path",
            format!("{role} relative_path must contain only normal relative components"),
        ));
    }
    Ok(path.to_path_buf())
}

fn verify_artifact_bytes(artifact: &ModelArtifactManifest, bytes: &[u8]) -> IdentityResult<()> {
    if artifact.external_data_allowed {
        return Err(IdentityError::new(
            "external_data_forbidden",
            format!(
                "{} manifest permits undeclared external data",
                artifact.role
            ),
        ));
    }
    if bytes.len() as u64 != artifact.bytes {
        return Err(IdentityError::new(
            "artifact_size_mismatch",
            format!(
                "{} expected {} bytes, observed {}",
                artifact.role,
                artifact.bytes,
                bytes.len()
            ),
        ));
    }
    let observed = sha256_hex(bytes);
    if observed != artifact.sha256 {
        return Err(IdentityError::new(
            "artifact_hash_mismatch",
            format!("{} hash does not match the pinned manifest", artifact.role),
        ));
    }
    Ok(())
}

fn secure_read_declared(
    canonical_root: &Path,
    artifact: &ModelArtifactManifest,
) -> IdentityResult<(PathBuf, Vec<u8>)> {
    let relative = artifact.relative_path.as_deref().ok_or_else(|| {
        IdentityError::new(
            "manifest_invalid",
            format!("{} has no relative_path", artifact.role),
        )
    })?;
    let relative = validate_relative_path(relative, &artifact.role)?;
    let target = canonical_root.join(relative);
    let metadata = std::fs::symlink_metadata(&target)
        .map_err(|e| IdentityError::new("model_missing", format!("{}: {e}", artifact.role)))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(IdentityError::new(
            "unsafe_artifact_path",
            format!("{} must be a regular non-symlink file", artifact.role),
        ));
    }
    let canonical = std::fs::canonicalize(&target)
        .map_err(|e| IdentityError::new("unsafe_artifact_path", e.to_string()))?;
    if !canonical.starts_with(canonical_root) || canonical.parent() != target.parent() {
        return Err(IdentityError::new(
            "unsafe_artifact_path",
            format!("{} escaped the manifest model root", artifact.role),
        ));
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|e| IdentityError::new("model_read_failed", format!("{}: {e}", artifact.role)))?;
    if bytes.is_empty() {
        return Err(IdentityError::new(
            "model_empty",
            format!("{} is empty", artifact.role),
        ));
    }
    verify_artifact_bytes(artifact, &bytes)?;
    Ok((canonical, bytes))
}

fn persist_import(root: &Path, relative: &str, bytes: &[u8]) -> IdentityResult<()> {
    let relative = validate_relative_path(relative, "import")?;
    let target = root.join(relative);
    if target.exists() {
        let metadata = std::fs::symlink_metadata(&target)
            .map_err(|e| IdentityError::new("model_read_failed", e.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(IdentityError::new(
                "unsafe_artifact_path",
                format!(
                    "existing imported artifact {} is not a regular file",
                    target.display()
                ),
            ));
        }
        let existing = std::fs::read(&target)
            .map_err(|e| IdentityError::new("model_read_failed", e.to_string()))?;
        if existing != bytes {
            return Err(IdentityError::new(
                "artifact_hash_mismatch",
                format!(
                    "existing imported artifact {} has different bytes",
                    target.display()
                ),
            ));
        }
        return Ok(());
    }
    persist_atomic(&target, bytes, false)
}

fn persist_atomic(target: &Path, bytes: &[u8], replace: bool) -> IdentityResult<()> {
    let parent = target.parent().ok_or_else(|| {
        IdentityError::new("manifest_write_failed", "target has no parent directory")
    })?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| IdentityError::new("manifest_write_failed", "invalid target filename"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| IdentityError::new("manifest_write_failed", e.to_string()))?
        .as_nanos();
    let temp = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| IdentityError::new("manifest_write_failed", e.to_string()))?;
    let write_result = (|| -> std::io::Result<()> {
        output.write_all(bytes)?;
        output.sync_all()?;
        Ok(())
    })();
    drop(output);
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(IdentityError::new("manifest_write_failed", err.to_string()));
    }

    if !replace || !target.exists() {
        return std::fs::rename(&temp, target).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            IdentityError::new("manifest_write_failed", e.to_string())
        });
    }

    let metadata = std::fs::symlink_metadata(target)
        .map_err(|e| IdentityError::new("manifest_write_failed", e.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        let _ = std::fs::remove_file(&temp);
        return Err(IdentityError::new(
            "unsafe_artifact_path",
            format!(
                "replacement target {} is not a regular file",
                target.display()
            ),
        ));
    }
    atomic_replace(target, &temp).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        IdentityError::new("manifest_write_failed", err.to_string())
    })
}

#[cfg(windows)]
fn atomic_replace(target: &Path, replacement: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let replaced = unsafe {
        ReplaceFileW(
            target_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(target: &Path, replacement: &Path) -> std::io::Result<()> {
    std::fs::rename(replacement, target)
}

pub fn restore_manifest(manifest_path: &Path, prior: Option<&[u8]>) -> IdentityResult<()> {
    match prior {
        Some(bytes) => persist_atomic(manifest_path, bytes, true),
        None => match std::fs::remove_file(manifest_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(IdentityError::new(
                "manifest_restore_failed",
                err.to_string(),
            )),
        },
    }
}

fn validate_manifest_contract(manifest: &InferenceManifest) -> IdentityResult<()> {
    let expected = manifest.schema_version == 1
        && manifest.runtime == RUNTIME_VERSION
        && manifest.runtime_name == RUNTIME_NAME
        && manifest.detector.role == "detector"
        && manifest.embedder.role == "embedder"
        && manifest.detector_input == [1, 3, DET_INPUT, DET_INPUT]
        && manifest.embedder_input == [1, 3, EMBED_INPUT, EMBED_INPUT]
        && manifest.embedding_dim == EMBEDDING_DIM
        && manifest.detector_preprocessing == "letterbox-640;rgb-to-bgr;raw-0-255"
        && manifest.embedder_preprocessing
            == "five-point-yunet-similarity-align-112;rgb;(x-127.5)/128"
        && manifest.embedding_normalization == "finite-l2-unit"
        && manifest.detection_threshold == DET_THRESHOLD
        && manifest.nms_threshold == NMS_THRESHOLD
        && manifest.detector_layout_version == DETECTOR_LAYOUT_VERSION
        && manifest.alignment_version == ALIGNMENT_VERSION
        && manifest.external_data_policy == EXTERNAL_DATA_POLICY
        && !manifest.detector.external_data_allowed
        && !manifest.embedder.external_data_allowed
        && !manifest.detector.provenance.trim().is_empty()
        && !manifest.detector.license.trim().is_empty()
        && !manifest.embedder.provenance.trim().is_empty()
        && !manifest.embedder.license.trim().is_empty();
    if !expected {
        return Err(IdentityError::new(
            "manifest_contract_mismatch",
            "manifest does not declare the exact shipped inference contract",
        ));
    }
    for artifact in [&manifest.detector, &manifest.embedder] {
        if artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || artifact.bytes == 0
        {
            return Err(IdentityError::new(
                "manifest_invalid",
                format!("{} hash/size declaration is invalid", artifact.role),
            ));
        }
    }
    if manifest.embedder.relative_path.is_none() {
        return Err(IdentityError::new(
            "manifest_invalid",
            "embedder must be rooted beside the manifest",
        ));
    }
    Ok(())
}

fn load_embedder(bytes: &[u8]) -> IdentityResult<Runnable> {
    load_embedder_for_runtime(bytes, RUNTIME_NAME)
}

fn load_embedder_for_runtime(bytes: &[u8], runtime_name: &str) -> IdentityResult<Runnable> {
    load_embedder_progress(bytes, runtime_name, &mut |_| {})
}
fn load_embedder_progress(
    bytes: &[u8],
    runtime_name: &str,
    progress: &mut dyn FnMut(PreparationPhase),
) -> IdentityResult<Runnable> {
    progress(PreparationPhase::EmbedderParse);
    let mut model = onnx()
        .map_err(|e| IdentityError::new("runtime_unavailable", e.to_string()))?
        .load_buffer(bytes)
        .map_err(|e| {
            IdentityError::new(
                "model_parse_rejected",
                format!("embedder ONNX rejected (external data is undeclared): {e}"),
            )
        })?;
    model
        .set_input_fact(0, "1,3,112,112,f32")
        .map_err(|e| IdentityError::new("model_shape_invalid", e.to_string()))?;
    let model = model
        .into_model()
        .map_err(|e| IdentityError::new("model_shape_invalid", e.to_string()))?;
    progress(PreparationPhase::EmbedderPrepare);
    let runnable = runtime_for_name(runtime_name)
        .map_err(|e| IdentityError::new("runtime_unavailable", e.to_string()))?
        .prepare(model)
        .map_err(|e| IdentityError::new("model_prepare_failed", e.to_string()))?;
    progress(PreparationPhase::EmbedderStartup);
    validate_embedder_startup(&runnable)?;
    Ok(runnable)
}

fn validate_embedder_startup(runnable: &Runnable) -> IdentityResult<()> {
    let input = Tensor::from_slice(
        &[1, 3, EMBED_INPUT, EMBED_INPUT],
        &vec![0f32; 3 * EMBED_INPUT * EMBED_INPUT],
    )
    .map_err(|e| IdentityError::new("tensor_invalid", e.to_string()))?;
    let output = runnable.run([input]).map_err(|e| {
        IdentityError::new("inference_failed", format!("embedder startup probe: {e}"))
    })?;
    let shape = output
        .first()
        .ok_or_else(|| IdentityError::new("malformed_output", "embedder returned no output"))?
        .shape()
        .map_err(|e| IdentityError::new("malformed_output", e.to_string()))?;
    if output.len() != 1 || shape != [1, EMBEDDING_DIM] {
        return Err(IdentityError::new(
            "dimension_mismatch",
            format!("embedder startup output must be exactly [1, {EMBEDDING_DIM}]"),
        ));
    }
    let values = output[0]
        .as_slice::<f32>()
        .map_err(|e| IdentityError::new("malformed_output", e.to_string()))?;
    if !values.iter().all(|value| value.is_finite()) {
        return Err(IdentityError::new(
            "non_finite",
            "embedder startup output is non-finite",
        ));
    }
    Ok(())
}

fn generation_for_manifest(manifest: &InferenceManifest) -> IdentityResult<String> {
    // Local names and roots are presentation/provisioning metadata, not model
    // semantics. Renaming or relocating identical bytes must retain generation.
    let basis = serde_json::json!({
        "schema_version": manifest.schema_version,
        "runtime": manifest.runtime,
        "runtime_name": manifest.runtime_name,
        "detector_sha256": manifest.detector.sha256,
        "detector_bytes": manifest.detector.bytes,
        "embedder_sha256": manifest.embedder.sha256,
        "embedder_bytes": manifest.embedder.bytes,
        "detector_input": manifest.detector_input,
        "embedder_input": manifest.embedder_input,
        "embedding_dim": manifest.embedding_dim,
        "detector_preprocessing": manifest.detector_preprocessing,
        "embedder_preprocessing": manifest.embedder_preprocessing,
        "embedding_normalization": manifest.embedding_normalization,
        "detection_threshold": manifest.detection_threshold,
        "nms_threshold": manifest.nms_threshold,
        "detector_layout_version": manifest.detector_layout_version,
        "alignment_version": manifest.alignment_version,
        "external_data_policy": manifest.external_data_policy,
        "detector_provenance": manifest.detector.provenance,
        "detector_license": manifest.detector.license,
        "embedder_provenance": manifest.embedder.provenance,
        "embedder_license": manifest.embedder.license,
        "detector_external_data_allowed": manifest.detector.external_data_allowed,
        "embedder_external_data_allowed": manifest.embedder.external_data_allowed,
    });
    let bytes = serde_json::to_vec(&basis)
        .map_err(|e| IdentityError::new("manifest_invalid", e.to_string()))?;
    Ok(sha256_hex(&bytes))
}

fn validate_face(face: &Face, image_w: u32, image_h: u32) -> IdentityResult<()> {
    if !face.score.is_finite()
        || !face.bbox.iter().all(|v| v.is_finite())
        || !face.landmarks.iter().flatten().all(|v| v.is_finite())
    {
        return Err(IdentityError::new(
            "non_finite",
            "face geometry contains NaN or infinity",
        ));
    }
    let [x, y, w, h] = face.bbox;
    if w <= 1.0
        || h <= 1.0
        || x < 0.0
        || y < 0.0
        || x + w > image_w as f32 + 0.5
        || y + h > image_h as f32 + 0.5
    {
        return Err(IdentityError::new(
            "invalid_landmarks",
            "face box is outside the decoded image",
        ));
    }
    if face
        .landmarks
        .iter()
        .any(|p| p[0] < 0.0 || p[1] < 0.0 || p[0] >= image_w as f32 || p[1] >= image_h as f32)
    {
        return Err(IdentityError::new(
            "invalid_landmarks",
            "landmark is outside the decoded image",
        ));
    }
    let distance = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
    let eye_mid_y = (face.landmarks[0][1] + face.landmarks[1][1]) * 0.5;
    let mouth_mid_y = (face.landmarks[3][1] + face.landmarks[4][1]) * 0.5;
    if distance(face.landmarks[0], face.landmarks[1]) <= 1.0
        || distance(face.landmarks[3], face.landmarks[4]) <= 1.0
        || mouth_mid_y - eye_mid_y <= 1.0
    {
        return Err(IdentityError::new(
            "invalid_landmarks",
            "five-point landmarks are degenerate",
        ));
    }
    Ok(())
}

fn normalize_bbox(bbox: [f32; 4], image_w: u32, image_h: u32) -> [f32; 4] {
    [
        bbox[0] / image_w as f32,
        bbox[1] / image_h as f32,
        bbox[2] / image_w as f32,
        bbox[3] / image_h as f32,
    ]
}

fn normalize_landmarks(mut landmarks: [[f32; 2]; 5], image_w: u32, image_h: u32) -> [[f32; 2]; 5] {
    for point in &mut landmarks {
        point[0] /= image_w as f32;
        point[1] /= image_h as f32;
    }
    landmarks
}

fn normalize_embedding(embedding: &mut [f32]) -> IdentityResult<()> {
    if !embedding.iter().all(|value| value.is_finite()) {
        return Err(IdentityError::new(
            "non_finite",
            "embedding contains NaN or infinity",
        ));
    }
    let norm = embedding
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if !norm.is_finite() || norm <= 1e-12 {
        return Err(IdentityError::new(
            "zero_norm",
            "embedding has no finite magnitude",
        ));
    }
    for value in embedding {
        *value /= norm;
    }
    Ok(())
}

/// YuNet face detector (OpenCV Zoo 2023mar: 12 outputs, 3 strides [8,16,32],
/// 1 anchor/cell; cls[0..3], obj[3..6], bbox[6..9], kps[9..12]; raw-0-255 BGR).
struct Detector {
    model: Runnable,
}

impl Detector {
    /// Load a YuNet detector from raw ONNX bytes (file or bundled), with a
    /// startup self-check: the model must execute on a blank frame and expose
    /// the 12-output 2023mar layout our decode expects — a wrong export can
    /// never silently produce wrong geometry.
    fn load_from_bytes(bytes: &[u8]) -> IdentityResult<Self> {
        Self::load_for_runtime(bytes, RUNTIME_NAME)
    }

    fn load_for_runtime(bytes: &[u8], runtime_name: &str) -> IdentityResult<Self> {
        Self::load_progress(bytes, runtime_name, &mut |_| {})
    }
    fn load_progress(
        bytes: &[u8],
        runtime_name: &str,
        progress: &mut dyn FnMut(PreparationPhase),
    ) -> IdentityResult<Self> {
        progress(PreparationPhase::DetectorParse);
        let mut inference = onnx()
            .map_err(|e| IdentityError::new("runtime_unavailable", e.to_string()))?
            .load_buffer(bytes)
            .map_err(|e| {
                IdentityError::new(
                    "model_parse_rejected",
                    format!("detector ONNX rejected (external data is undeclared): {e}"),
                )
            })?;
        inference
            .set_input_fact(0, "1,3,640,640,f32")
            .map_err(|e| IdentityError::new("model_shape_invalid", e.to_string()))?;
        let model = inference
            .into_model()
            .map_err(|e| IdentityError::new("model_shape_invalid", e.to_string()))?;
        progress(PreparationPhase::DetectorPrepare);
        let model = runtime_for_name(runtime_name)
            .map_err(|e| IdentityError::new("runtime_unavailable", e.to_string()))?
            .prepare(model)
            .map_err(|e| IdentityError::new("model_prepare_failed", e.to_string()))?;
        let detector = Self { model };
        progress(PreparationPhase::DetectorStartup);
        detector.self_check()?;
        Ok(detector)
    }

    /// Run a blank frame through the model and verify the output layout.
    fn self_check(&self) -> IdentityResult<()> {
        let blob = vec![0f32; 3 * DET_INPUT * DET_INPUT];
        let input = Tensor::from_slice(&[1, 3, DET_INPUT, DET_INPUT], &blob)
            .map_err(|e| IdentityError::new("tensor_invalid", e.to_string()))?;
        let out = self
            .model
            .run([input])
            .map_err(|e| IdentityError::new("detector_inference", e.to_string()))?;
        if out.len() != 12 {
            return Err(IdentityError::new(
                "detector_output_malformed",
                format!("{} outputs, expected exactly 12", out.len()),
            ));
        }
        validate_detector_planes(&out)?;
        Ok(())
    }

    /// Detect every face with score >= `min_score`, decoded into the ORIGINAL
    /// image's pixel space, de-duplicated by greedy IoU NMS, sorted by score
    /// descending. Empty when no face clears the threshold.
    fn detect_all(&self, img: &RgbImage, min_score: f32) -> IdentityResult<Vec<Face>> {
        let (ow, oh) = img.dimensions();
        if ow == 0 || oh == 0 {
            return Err(IdentityError::new(
                "image_dimensions_invalid",
                "detector image is empty",
            ));
        }
        // Letterbox into DET_INPUT x DET_INPUT, preserving aspect ratio.
        let im_ratio = oh as f32 / ow as f32;
        let (new_w, new_h) = if im_ratio > 1.0 {
            let nh = DET_INPUT as f32;
            (((nh / im_ratio).round() as u32).max(1), nh as u32)
        } else {
            let nw = DET_INPUT as f32;
            (nw as u32, ((nw * im_ratio).round() as u32).max(1))
        };
        let det_scale = new_h as f32 / oh as f32;
        let resized =
            image::imageops::resize(img, new_w, new_h, image::imageops::FilterType::Triangle);

        let mut blob = vec![0f32; 3 * DET_INPUT * DET_INPUT];
        for y in 0..new_h.min(DET_INPUT as u32) {
            for x in 0..new_w.min(DET_INPUT as u32) {
                let px = resized.get_pixel(x, y);
                // YuNet expects raw 0-255 pixels in BGR channel order (OpenCV).
                let plane = DET_INPUT * DET_INPUT;
                let off = (y as usize) * DET_INPUT + (x as usize);
                blob[off] = px[2] as f32; // B
                blob[plane + off] = px[1] as f32; // G
                blob[2 * plane + off] = px[0] as f32; // R
            }
        }
        let input = Tensor::from_slice(&[1, 3, DET_INPUT, DET_INPUT], &blob)
            .map_err(|e| IdentityError::new("tensor_invalid", e.to_string()))?;
        let out = self
            .model
            .run([input])
            .map_err(|e| IdentityError::new("detector_inference", e.to_string()))?;
        if out.len() != 12 {
            return Err(IdentityError::new(
                "detector_output_malformed",
                format!("{} outputs, expected exactly 12", out.len()),
            ));
        }
        validate_detector_planes(&out)?;

        // YuNet 2023mar layout (confirmed via output shapes): per stride i in 0..3
        //   cls=out[i], obj=out[i+3], bbox=out[i+6], kps=out[i+9].
        // 1 anchor/cell; grid = sqrt(rows); stride = DET_INPUT / grid.
        // Decode (matches OpenCV FaceDetectorYN postprocess):
        //   score = sqrt(clamp01(cls)*clamp01(obj));
        //   cx = (col+dx)*stride; cy = (row+dy)*stride;       (linear)
        //   w  = exp(dw)*stride;  h  = exp(dh)*stride;         (EXPONENTIAL)
        //   landmark = (col/row + delta)*stride;
        // then divide by det_scale to return to original image pixels.
        let plane = |idx: usize| -> IdentityResult<Vec<f32>> {
            out[idx]
                .as_slice::<f32>()
                .map(|v| v.iter().copied().collect())
                .map_err(|e| IdentityError::new("detector_output_malformed", e.to_string()))
        };
        let floor = min_score.max(0.0);
        let mut faces: Vec<Face> = Vec::new();
        for i in 0..3 {
            let cls = plane(i)?;
            let obj = plane(i + 3)?;
            let bbox = plane(i + 6)?;
            let kps = plane(i + 9)?;
            let n = cls.len();
            if n == 0 || obj.len() < n || bbox.len() < n * 4 || kps.len() < n * 10 {
                return Err(IdentityError::new(
                    "detector_output_malformed",
                    "YuNet plane lengths do not match",
                ));
            }
            let grid = (n as f64).sqrt().round() as usize;
            // 1 anchor/cell on a square grid; skip if the layout isn't square.
            if grid == 0 || grid * grid != n {
                return Err(IdentityError::new(
                    "detector_output_malformed",
                    "YuNet score plane is not square",
                ));
            }
            let stride = (DET_INPUT / grid) as f32;
            for r in 0..n {
                let score = (cls[r].max(0.0).min(1.0) * obj[r].max(0.0).min(1.0)).sqrt();
                if score < floor {
                    continue;
                }
                let row = (r / grid) as f32;
                let col = (r % grid) as f32;
                let cx = (col + bbox[r * 4]) * stride;
                let cy = (row + bbox[r * 4 + 1]) * stride;
                let w = bbox[r * 4 + 2].exp() * stride;
                let h = bbox[r * 4 + 3].exp() * stride;
                if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
                    continue;
                }
                let mut pts = [[0f32; 2]; 5];
                for k in 0..5 {
                    pts[k][0] = (col + kps[r * 10 + 2 * k]) * stride / det_scale;
                    pts[k][1] = (row + kps[r * 10 + 2 * k + 1]) * stride / det_scale;
                }
                // box in original pixels, clamped to image bounds.
                let mut x = (cx - w / 2.0) / det_scale;
                let mut y = (cy - h / 2.0) / det_scale;
                let mut bw = w / det_scale;
                let mut bh = h / det_scale;
                if x < 0.0 {
                    bw += x;
                    x = 0.0;
                }
                if y < 0.0 {
                    bh += y;
                    y = 0.0;
                }
                bw = bw.min(ow as f32 - x).max(0.0);
                bh = bh.min(oh as f32 - y).max(0.0);
                if bw <= 0.0 || bh <= 0.0 {
                    continue;
                }
                faces.push(Face {
                    bbox: [x, y, bw, bh],
                    score,
                    landmarks: pts,
                });
            }
        }
        Ok(nms(faces, NMS_THRESHOLD))
    }
}

fn validate_detector_planes(outputs: &[Tensor]) -> IdentityResult<()> {
    if outputs.len() != 12 {
        return Err(IdentityError::new(
            "detector_output_malformed",
            format!("{} outputs, expected exactly 12", outputs.len()),
        ));
    }
    let cells = [6400usize, 1600, 400];
    for (index, tensor) in outputs.iter().enumerate() {
        let group = index / 3;
        let stride_index = index % 3;
        let width = match group {
            0 | 1 => 1,
            2 => 4,
            3 => 10,
            _ => unreachable!(),
        };
        let expected = [1usize, cells[stride_index], width];
        let shape = tensor
            .shape()
            .map_err(|e| IdentityError::new("detector_output_malformed", e.to_string()))?;
        if shape != expected {
            return Err(IdentityError::new(
                "detector_output_malformed",
                format!("output {index} shape {:?}, expected {:?}", shape, expected),
            ));
        }
        let values = tensor.as_slice::<f32>().map_err(|e| {
            IdentityError::new("detector_output_malformed", format!("output {index}: {e}"))
        })?;
        if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
            return Err(IdentityError::new(
                "detector_output_malformed",
                format!("output {index} is empty or non-finite"),
            ));
        }
    }
    Ok(())
}

/// Greedy IoU non-max suppression. Deterministic: sort by score descending with
/// a stable index tiebreak, then keep boxes that don't overlap a kept box by
/// more than `iou_threshold`.
fn nms(mut faces: Vec<Face>, iou_threshold: f32) -> Vec<Face> {
    // Stable sort by score desc; equal scores keep original (anchor) order.
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<Face> = Vec::new();
    for f in faces {
        if kept.iter().all(|k| iou(&k.bbox, &f.bbox) <= iou_threshold) {
            kept.push(f);
        }
    }
    kept
}

/// Intersection-over-union of two [x, y, w, h] boxes.
fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (ax2, ay2) = (a[0] + a[2], a[1] + a[3]);
    let (bx2, by2) = (b[0] + b[2], b[1] + b[3]);
    let ix1 = a[0].max(b[0]);
    let iy1 = a[1].max(b[1]);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Align a face to a 112x112 ArcFace crop using a least-squares similarity
/// transform from the 5 landmarks to the canonical template.
fn align_112(img: &RgbImage, landmarks: &[[f32; 2]; 5]) -> Option<RgbImage> {
    let [a, b, tx, ty] = solve_similarity(landmarks, &ARCFACE_DST)?;
    let det = a * a + b * b;
    if det.abs() < 1e-9 {
        return None;
    }
    let (w, h) = img.dimensions();
    let mut out = RgbImage::new(112, 112);
    for oy in 0..112u32 {
        for ox in 0..112u32 {
            // invert the similarity: src = L^-1 * (dst - t)
            let ddx = ox as f32 - tx;
            let ddy = oy as f32 - ty;
            let sx = (a * ddx + b * ddy) / det;
            let sy = (-b * ddx + a * ddy) / det;
            let px = bilinear(img, sx, sy, w, h);
            out.put_pixel(ox, oy, image::Rgb(px));
        }
    }
    Some(out)
}

/// Bilinear sample; out-of-bounds returns black.
fn bilinear(img: &RgbImage, sx: f32, sy: f32, w: u32, h: u32) -> [u8; 3] {
    if sx < 0.0 || sy < 0.0 || sx > (w - 1) as f32 || sy > (h - 1) as f32 {
        return [0, 0, 0];
    }
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = sx - x0 as f32;
    let fy = sy - y0 as f32;
    let p00 = img.get_pixel(x0, y0);
    let p10 = img.get_pixel(x1, y0);
    let p01 = img.get_pixel(x0, y1);
    let p11 = img.get_pixel(x1, y1);
    let mut out = [0u8; 3];
    for c in 0..3usize {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// Least-squares similarity transform mapping src -> dst as
/// (x,y) -> (a*x - b*y + tx, b*x + a*y + ty). Returns [a, b, tx, ty].
fn solve_similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> Option<[f32; 4]> {
    // Normal equations A^T A p = A^T b, where each point gives two rows:
    //   [ x, -y, 1, 0 ] . p = X
    //   [ y,  x, 0, 1 ] . p = Y
    let mut ata = [[0f64; 4]; 4];
    let mut atb = [0f64; 4];
    for i in 0..5 {
        let (x, y) = (src[i][0] as f64, src[i][1] as f64);
        let (xx, yy) = (dst[i][0] as f64, dst[i][1] as f64);
        let rows = [([x, -y, 1.0, 0.0], xx), ([y, x, 0.0, 1.0], yy)];
        for (coeff, rhs) in rows {
            for r in 0..4 {
                for c in 0..4 {
                    ata[r][c] += coeff[r] * coeff[c];
                }
                atb[r] += coeff[r] * rhs;
            }
        }
    }
    let p = solve4(ata, atb)?;
    Some([p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32])
}

/// Solve a 4x4 linear system by Gaussian elimination with partial pivoting.
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        // pivot
        let mut piv = col;
        for r in (col + 1)..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        // eliminate
        for r in 0..4 {
            if r == col {
                continue;
            }
            let f = a[r][col] / a[col][col];
            for c in col..4 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0f64; 4];
    for i in 0..4 {
        x[i] = b[i] / a[i][i];
    }
    Some(x)
}

// ---------------------------------------------------------------------------
// Curation metadata, wave 1 (WP-019): face-crop sharpness, coarse yaw bucket,
// hair-color heuristic. Sharpness/yaw derive from real detector geometry
// (`source: real`); the hair flag is an HSV heuristic (`source: proxy`).
// ---------------------------------------------------------------------------

/// Laplacian variance over a region (the standard focus measure): higher =
/// sharper. `bbox` (x, y, w, h in pixels) restricts to the face crop; `None`
/// measures the whole image.
pub fn laplacian_variance(img: &RgbImage, bbox: Option<[f32; 4]>) -> f32 {
    let (iw, ih) = img.dimensions();
    if iw < 3 || ih < 3 {
        return 0.0;
    }
    let (x0, y0, x1, y1) = match bbox {
        Some([bx, by, bw, bh]) => {
            let x0 = bx.max(0.0) as u32;
            let y0 = by.max(0.0) as u32;
            let x1 = ((bx + bw).min(iw as f32)) as u32;
            let y1 = ((by + bh).min(ih as f32)) as u32;
            (x0, y0, x1, y1)
        }
        None => (0, 0, iw, ih),
    };
    if x1.saturating_sub(x0) < 3 || y1.saturating_sub(y0) < 3 {
        return 0.0;
    }
    let gray = |x: u32, y: u32| -> f32 {
        let p = img.get_pixel(x, y);
        0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
    };
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for y in (y0 + 1)..(y1 - 1) {
        for x in (x0 + 1)..(x1 - 1) {
            let lap = 4.0 * gray(x, y)
                - gray(x - 1, y)
                - gray(x + 1, y)
                - gray(x, y - 1)
                - gray(x, y + 1);
            sum += lap as f64;
            sum_sq += (lap as f64) * (lap as f64);
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let mean = sum / n as f64;
    ((sum_sq / n as f64) - mean * mean).max(0.0) as f32
}

/// Coarse yaw bucket from the 5-point landmarks: compares the nose's
/// horizontal position between the eyes. Returns `(bucket, eye_nose_ratio)`
/// where ratio = min(d_left, d_right)/max(..) in [0,1] (1 = perfectly
/// frontal). Buckets only — 5 points cannot support precise pose angles.
pub fn yaw_bucket(landmarks: &[[f32; 2]; 5]) -> (&'static str, f32) {
    let left_eye = landmarks[0];
    let right_eye = landmarks[1];
    let nose = landmarks[2];
    let d_left = nose[0] - left_eye[0];
    let d_right = right_eye[0] - nose[0];
    // Nose outside the eye span = strong profile regardless of ratio.
    if d_left <= 0.0 || d_right <= 0.0 {
        return ("profile", 0.0);
    }
    let ratio = d_left.min(d_right) / d_left.max(d_right);
    if ratio >= 0.55 {
        ("frontal", ratio)
    } else if ratio >= 0.25 {
        ("quarter", ratio)
    } else {
        ("profile", ratio)
    }
}

/// Dominant hair-color flag from the strip above the face box (HSV heuristic,
/// `source: proxy`). Returns `(label, confidence)` where confidence is the
/// winning bucket's pixel share in [0,1]. Targets triage (e.g. surfacing pink
/// wigs), never gating.
pub fn hair_color_flag(img: &RgbImage, bbox: [f32; 4]) -> (&'static str, f32) {
    let (iw, ih) = img.dimensions();
    let [bx, by, bw, bh] = bbox;
    // Strip: face width +15% each side, from half a face-height above the box
    // down to the box top.
    let x0 = (bx - bw * 0.15).max(0.0) as u32;
    let x1 = ((bx + bw * 1.15).min(iw as f32)) as u32;
    let y0 = (by - bh * 0.5).max(0.0) as u32;
    let y1 = by.max(0.0).min(ih as f32) as u32;
    if x1 <= x0 || y1 <= y0 {
        return ("unknown", 0.0);
    }
    let mut counts: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut total = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = img.get_pixel(x, y);
            let (h, s, v) = rgb_to_hsv(p[0], p[1], p[2]);
            let label = classify_hair_pixel(h, s, v);
            *counts.entry(label).or_insert(0) += 1;
            total += 1;
        }
    }
    if total == 0 {
        return ("unknown", 0.0);
    }
    let (label, count) = counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .unwrap_or(("unknown", 0));
    (label, count as f32 / total as f32)
}

/// HSV: h in degrees [0,360), s and v in [0,1].
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max < 1e-6 { 0.0 } else { delta / max };
    (h, s, max)
}

/// One pixel's hair bucket. Saturated pixels classify by hue; desaturated by
/// value. Buckets chosen for curation triage (wig/dye detection), not beauty.
fn classify_hair_pixel(h: f32, s: f32, v: f32) -> &'static str {
    if v < 0.15 {
        return "black";
    }
    if s < 0.14 {
        return if v > 0.65 { "gray_white" } else { "brown" };
    }
    if s >= 0.18 {
        match h {
            x if (290.0..345.0).contains(&x) => return "pink_purple",
            x if !(15.0..345.0).contains(&x) => return "red",
            x if (15.0..45.0).contains(&x) && v < 0.55 => return "brown",
            x if (15.0..70.0).contains(&x) => return "blonde",
            x if (70.0..170.0).contains(&x) => return "green",
            x if (170.0..260.0).contains(&x) => return "blue",
            x if (260.0..290.0).contains(&x) => return "pink_purple",
            _ => {}
        }
    }
    if v < 0.45 {
        "brown"
    } else {
        "other"
    }
}

/// Generation-bound cosine similarity. A raw float slice is deliberately not
/// accepted at the production boundary, preventing callers from discarding
/// model-space provenance before comparison.
pub fn cosine_checked(a: &IdentityVector, b: &IdentityVector) -> IdentityResult<f32> {
    if a.generation.is_empty() || b.generation.is_empty() || a.generation != b.generation {
        return Err(IdentityError::new(
            "generation_mismatch",
            "embedding generations differ",
        ));
    }
    if a.dimension != EMBEDDING_DIM
        || b.dimension != EMBEDDING_DIM
        || a.values.len() != a.dimension
        || b.values.len() != b.dimension
    {
        return Err(IdentityError::new(
            "dimension_mismatch",
            format!(
                "expected {EMBEDDING_DIM}, declared {} and {}",
                a.dimension, b.dimension
            ),
        ));
    }
    if !a
        .values
        .iter()
        .chain(&b.values)
        .all(|value| value.is_finite())
    {
        return Err(IdentityError::new(
            "non_finite",
            "comparison vector contains NaN or infinity",
        ));
    }
    Ok(a.values
        .iter()
        .zip(&b.values)
        .map(|(left, right)| left * right)
        .sum())
}

/// Greedy threshold clustering over L2-normalized embeddings (WP-018).
/// Deterministic: inputs are visited in order; an item joins the FIRST
/// existing cluster whose representative (first member) it matches at
/// `cosine >= threshold`, else founds a new cluster. Returns the cluster
/// index per input, numbered by order of founding.
pub fn cluster_embeddings(
    embeddings: &[IdentityVector],
    threshold: f32,
) -> IdentityResult<Vec<usize>> {
    let mut assignment = Vec::with_capacity(embeddings.len());
    let mut representatives: Vec<usize> = Vec::new();
    for (i, emb) in embeddings.iter().enumerate() {
        let mut joined = None;
        for (cluster_idx, &rep) in representatives.iter().enumerate() {
            if cosine_checked(emb, &embeddings[rep])? >= threshold {
                joined = Some(cluster_idx);
                break;
            }
        }
        match joined {
            Some(cluster_idx) => assignment.push(cluster_idx),
            None => {
                representatives.push(i);
                assignment.push(representatives.len() - 1);
            }
        }
    }
    Ok(assignment)
}

// ---------- WP-082 strict recognition and calibration ----------

const CALIBRATION_SCHEMA_VERSION: u32 = 1;
const CALIBRATION_ARTIFACT_ID: &str = "VAL-WP-082-MATCH-CALIBRATION-V1";
const CALIBRATION_CONTRACT_IDENTITY: &str =
    "repo://governance/validation/wp-082-match-calibration-v1.yaml";
const CALIBRATION_APPROVAL_STATEMENT: &str =
    "I independently reviewed the reconstructed WP-082 evidence and approve this verdict.";
const CALIBRATION_VERIFIER_VERSION: &str = "match-calibration-verifier-v2";
const TRUSTED_REVIEWER_KEY_ID: Option<&str> = option_env!("FACIAL_MATCH_REVIEWER_KEY_ID");
const TRUSTED_REVIEWER_PUBLIC_KEY_HEX: Option<&str> =
    option_env!("FACIAL_MATCH_REVIEWER_ED25519_PUBLIC_KEY_HEX");
const CALIBRATION_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const CALIBRATION_MAX_MEDIA_BYTES: u64 = 256 * 1024 * 1024;
const CALIBRATION_MAX_TOTAL_MEDIA_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const CALIBRATION_MAX_RECORDS: usize = 1_000_000;
const CALIBRATION_MAX_SCALAR_BYTES: usize = 4096;
const AGGREGATE_TARGET: f64 = 0.0003;
const SLICE_TARGET: f64 = 0.001;
const AGGREGATE_MIN_INDEPENDENT: usize = 10_000;
const SLICE_MIN_INDEPENDENT: usize = 3_000;
pub const STRICT_ANN_ENGINE_VERSION: &str = "surrealdb-3.2.4";
pub const STRICT_SELECTION_POLICY: &str = "trusted-reference-authorization-v1";
pub const STRICT_ANN_BUILD_ORDER: &str = "membership-id-ascending-v1";
pub const STRICT_EXCLUDED_SLICE_ROUTER_VERSION: &str = "unavailable-wp082-fail-closed-v1";
pub const STRICT_ANN_TIE_BREAKING: &str = "distance-then-record-id-ascending-v1";
pub const STRICT_RERANK_TIE_BREAKING: &str = "similarity-desc-person-look-embedding-ascending-v1";
pub const STRICT_HNSW_M: usize = 12;
pub const STRICT_HNSW_EF_CONSTRUCTION: usize = 150;

pub fn strict_hnsw_ef_search(candidate_k: usize) -> usize {
    candidate_k.saturating_mul(4).clamp(40, 4000)
}

pub fn strict_runtime_configuration_digest(candidate_k: usize, rerank_k: usize) -> String {
    sha256_hex(
        format!(
            "selection_policy={STRICT_SELECTION_POLICY}\nindex_type=hnsw\nengine_version={STRICT_ANN_ENGINE_VERSION}\nalgorithm=surrealdb-hnsw\ndistance_metric=cosine\nquantization=f32\nbuild_seed=0\nbuild_order={STRICT_ANN_BUILD_ORDER}\nann_tie_breaking={STRICT_ANN_TIE_BREAKING}\nhnsw_m={STRICT_HNSW_M}\nhnsw_ef_construction={STRICT_HNSW_EF_CONSTRUCTION}\nhnsw_ef_search={}\ncandidate_k={candidate_k}\nrerank_k={rerank_k}\nrerank_distance_metric=cosine\nrerank_precision=f32\nrerank_tie_breaking={STRICT_RERANK_TIE_BREAKING}\naggregation=best-template-per-look-then-best-look-per-person\nalignment_required=true\npose_gate_required=true\ncannot_link_required=true\nexcluded_slice_router_version={STRICT_EXCLUDED_SLICE_ROUTER_VERSION}\nexcluded_slice_policy=reject-until-runtime-router-is-shipped\n",
            strict_hnsw_ef_search(candidate_k)
        )
        .as_bytes(),
    )
}
const REQUIRED_GATES: [&str; 5] = [
    "combined-wrong-committed-strict-automatic",
    "mated-wrong-person-committed-strict-automatic",
    "empirical-open-set-1-to-N-FPIR",
    "excluded-slice-router-miss-rate",
    "excluded-slice-final-auto-commit-escape-rate",
];
const REQUIRED_MANIFESTS: [&str; 11] = [
    "fixture-manifest",
    "person-registry",
    "acquisition-cluster-registry",
    "lineage-root-registry",
    "duplicate-family-registry",
    "hard-slice-registry",
    "partition-registry",
    "gallery-envelope-manifest",
    "gallery-composition-manifest",
    "probe-selection-manifest",
    "spent-activation-set-registry",
];
const REQUIRED_HARD_SLICES: [&str; 15] = [
    "lookalike_twin_impostor",
    "nonfrontal_pose",
    "known_Look_age_time_gap",
    "styling_makeup",
    "wig_hair_change",
    "glasses",
    "mask_partial_occlusion",
    "poor_exposure",
    "small_blurred_face",
    "compression_resize",
    "screenshot",
    "collage_poster_multiface",
    "synthetic_media",
    "duplicate_burst_video_family",
    "demographic_cohort",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationContract {
    artifact_id: String,
    artifact_kind: String,
    schema_version: u32,
    workpacket_id: String,
    status: String,
    updated_at: String,
    authority: Vec<String>,
    evaluation_root: serde_yaml::Value,
    planned_verifier: serde_yaml::Value,
    required_manifests: Vec<DeclaredCalibrationFile>,
    manifest_record_requirements: serde_yaml::Value,
    manifest_schemas: serde_yaml::Value,
    raw_run_record_schema: serde_yaml::Value,
    frozen_protocol: FrozenProtocol,
    gating_metrics: Vec<String>,
    diagnostic_metrics: Vec<String>,
    metric_coverage: serde_yaml::Value,
    required_metric_fields: Vec<String>,
    run_identity: CalibrationRunIdentity,
    results: CalibrationResults,
    independent_review: CalibrationReview,
    activation_gate: serde_yaml::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeclaredCalibrationFile {
    id: String,
    relative_path: Option<String>,
    sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenProtocol {
    confidence_method: String,
    aggregate_bound_max: f64,
    hard_slice_bound_max: f64,
    minimum_aggregate_people: usize,
    minimum_aggregate_acquisition_clusters: usize,
    minimum_slice_people: usize,
    minimum_slice_acquisition_clusters: usize,
    zero_effective_emissions_for_conditional_metric: String,
    partition_or_lineage_overlap: String,
    spent_test_reuse_without_predeclared_alpha_spending: String,
    required_hard_slice_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationRunIdentity {
    activation_run_id: Option<String>,
    app_version: Option<String>,
    git_commit: Option<String>,
    cargo_lock_sha256: Option<String>,
    model_generation: Option<String>,
    calibration_generation: Option<String>,
    fixture_manifest_sha256: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationResults {
    status: String,
    #[serde(default)]
    metrics: Vec<RecordedMetric>,
    raw_run_records_relative_path: Option<String>,
    raw_run_records_sha256: Option<String>,
    verifier_verdict: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationReview {
    reviewer: Option<String>,
    reviewer_key_id: Option<String>,
    approval_statement: Option<String>,
    signature_ed25519_hex: Option<String>,
    verdict: String,
    #[serde(default)]
    reviewed_manifest_hashes: Vec<String>,
    #[serde(default)]
    reviewed_metric_ids: Vec<String>,
    evidence_digest: Option<String>,
    digest_contract: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordedMetric {
    metric_id: String,
    scope: String,
    denominator: usize,
    errors: usize,
    distinct_people: usize,
    distinct_acquisition_clusters: usize,
    point_estimate: f64,
    one_sided_95_ucb: Option<f64>,
    target: f64,
    sufficient: bool,
    #[serde(default)]
    insufficiency_reasons: Vec<String>,
    verdict: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationFixture {
    fixture_id: String,
    media_relative_path: String,
    media_sha256: String,
    source_asset_id: String,
    ground_truth_person_id: String,
    acquisition_cluster_id: String,
    lineage_root_id: String,
    duplicate_family_id: String,
    partition_id: String,
    #[serde(default)]
    capture_session_id: String,
    #[serde(default)]
    burst_id: String,
    #[serde(default)]
    video_track_id: String,
    #[serde(default)]
    hard_slice_ids: Vec<String>,
    #[serde(default)]
    lookalike_rival_group_id: Option<String>,
    #[serde(default)]
    slice_evidence: HardSliceGroundTruth,
}

/// Frozen, typed ground truth used by the verifier to reconstruct slice
/// membership. The evaluated model never produces or modifies these fields.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HardSliceGroundTruth {
    #[serde(default)]
    look_id: String,
    #[serde(default)]
    capture_day: Option<i64>,
    #[serde(default)]
    yaw_degrees: Option<f64>,
    #[serde(default)]
    pitch_degrees: Option<f64>,
    #[serde(default)]
    major_styling_or_makeup_change: bool,
    #[serde(default)]
    wig_or_major_hair_change: bool,
    #[serde(default)]
    glasses: Option<bool>,
    #[serde(default)]
    face_occlusion_fraction: Option<f64>,
    #[serde(default)]
    mask_or_partial_occlusion: bool,
    #[serde(default)]
    exposure_label: String,
    #[serde(default)]
    face_box_width: Option<u32>,
    #[serde(default)]
    face_box_height: Option<u32>,
    #[serde(default)]
    blur_label: String,
    #[serde(default)]
    lossy_reencode: bool,
    #[serde(default)]
    source_short_edge_before_upscale: Option<u32>,
    #[serde(default)]
    source_kind: String,
    #[serde(default)]
    ground_truth_face_count: Option<u32>,
    #[serde(default)]
    origin_kind: String,
    #[serde(default)]
    demographic_cohort_ids: Vec<String>,
    #[serde(default)]
    annotation_provenance: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationPerson {
    person_id: String,
    partition_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionCluster {
    acquisition_cluster_id: String,
    member_fixture_ids: Vec<String>,
    closure_basis: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LineageRoot {
    lineage_root_id: String,
    member_fixture_ids: Vec<String>,
    derivation_basis: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DuplicateFamily {
    duplicate_family_id: String,
    member_fixture_ids: Vec<String>,
    family_method: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HardSlice {
    slice_id: String,
    membership_predicate: String,
    automatic_eligibility: bool,
    runtime_router_predicate: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CalibrationPartition {
    partition_id: String,
    purpose: String,
    frozen_at: String,
    member_person_ids: Vec<String>,
    member_fixture_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GalleryEnvelope {
    pub manifest_sha256: String,
    pub people_max: usize,
    pub looks_per_person_max: usize,
    pub trusted_templates_per_look_max: usize,
    pub total_templates_max: usize,
    pub selection_policy: String,
    pub thresholds: CalibrationThresholds,
    pub ann_configuration: CalibrationAnnConfiguration,
    pub exact_rerank_configuration: CalibrationRerankConfiguration,
    pub aggregation: String,
    pub margins: CalibrationMargins,
    pub quality_gates: CalibrationQualityGates,
    pub cannot_link_gates: CalibrationCannotLinkGates,
    pub model_generation: String,
    pub calibration_generation: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationThresholds {
    pub automatic_commit: f64,
    pub suggestion: f64,
    pub unnamed_cluster: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationAnnConfiguration {
    pub index_type: String,
    pub engine_version: String,
    pub algorithm: String,
    pub distance_metric: String,
    pub quantization: String,
    pub build_seed: u64,
    pub build_order: String,
    pub tie_breaking: String,
    pub hnsw_m: usize,
    pub hnsw_ef_construction: usize,
    pub hnsw_ef_search: usize,
    pub candidate_k: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationRerankConfiguration {
    pub rerank_k: usize,
    pub distance_metric: String,
    pub precision: String,
    pub tie_breaking: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationMargins {
    pub runner_up_minimum: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationQualityGates {
    pub minimum_quality: f64,
    pub alignment_required: bool,
    pub pose_gate_required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationCannotLinkGates {
    pub face_to_person_exclusion_required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GalleryMember {
    person_id: String,
    look_id: String,
    trusted_template_set_id: String,
    observation_id: String,
    fixture_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProbeKind {
    Mated,
    NonMated,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SelectedProbe {
    probe_id: String,
    fixture_id: String,
    probe_kind: ProbeKind,
    slice_ids: Vec<String>,
    predeclared_before_run: bool,
}

impl Default for ProbeKind {
    fn default() -> Self {
        Self::Mated
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SelectedPair {
    pair_id: String,
    left_probe_id: String,
    right_probe_id: String,
    predeclared_before_run: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SpentSet {
    activation_run_id: String,
    #[serde(default)]
    person_ids: Vec<String>,
    #[serde(default)]
    acquisition_cluster_ids: Vec<String>,
    fixture_manifest_sha256: String,
    observed_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMetricDocument {
    schema_version: u32,
    records: Vec<RawProbeOutcome>,
    pairwise_records: Vec<RawPairwiseOutcome>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EmittedAssignmentState {
    Unidentified,
    Suggestion,
    CommittedStrictAutomatic,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenPipelineProvenance {
    model_generation: String,
    calibration_generation: String,
    gallery_envelope_sha256: String,
    pipeline_configuration_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawProbeOutcome {
    probe_id: String,
    terminal_outcome: String,
    emitted_assignment_state: EmittedAssignmentState,
    assigned_person_id: Option<String>,
    router_abstained: bool,
    observed_at: String,
    provenance: FrozenPipelineProvenance,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPairwiseOutcome {
    pair_id: String,
    left_probe_id: String,
    right_probe_id: String,
    terminal_outcome: String,
    accepted: bool,
    similarity: f64,
    observed_at: String,
    provenance: FrozenPipelineProvenance,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifestDocument {
    schema_version: u32,
    fixtures: Vec<CalibrationFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonRegistryDocument {
    schema_version: u32,
    people: Vec<CalibrationPerson>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionRegistryDocument {
    schema_version: u32,
    acquisition_clusters: Vec<AcquisitionCluster>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LineageRegistryDocument {
    schema_version: u32,
    lineage_roots: Vec<LineageRoot>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DuplicateRegistryDocument {
    schema_version: u32,
    duplicate_families: Vec<DuplicateFamily>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HardSliceRegistryDocument {
    schema_version: u32,
    hard_slices: Vec<HardSlice>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartitionRegistryDocument {
    schema_version: u32,
    partitions: Vec<CalibrationPartition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GalleryEnvelopeDocument {
    schema_version: u32,
    gallery_envelope: GalleryEnvelope,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GalleryCompositionDocument {
    schema_version: u32,
    gallery_members: Vec<GalleryMember>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeSelectionDocument {
    schema_version: u32,
    selected_probes: Vec<SelectedProbe>,
    selected_pairs: Vec<SelectedPair>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpentSetRegistryDocument {
    schema_version: u32,
    spent_sets: Vec<SpentSet>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct CalibrationEvidenceGraph {
    fixtures: Vec<CalibrationFixture>,
    people: Vec<CalibrationPerson>,
    acquisition_clusters: Vec<AcquisitionCluster>,
    lineage_roots: Vec<LineageRoot>,
    duplicate_families: Vec<DuplicateFamily>,
    hard_slices: Vec<HardSlice>,
    partitions: Vec<CalibrationPartition>,
    gallery_envelope: Option<GalleryEnvelope>,
    gallery_members: Vec<GalleryMember>,
    selected_probes: Vec<SelectedProbe>,
    selected_pairs: Vec<SelectedPair>,
    spent_sets: Vec<SpentSet>,
}

fn parse_calibration_manifest(
    id: &str,
    bytes: &[u8],
    graph: &mut CalibrationEvidenceGraph,
) -> Result<(), ()> {
    macro_rules! parse {
        ($ty:ty) => {
            serde_yaml::from_slice::<$ty>(bytes).map_err(|_| ())?
        };
    }
    match id {
        "fixture-manifest" => {
            let doc = parse!(FixtureManifestDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.fixtures = doc.fixtures;
        }
        "person-registry" => {
            let doc = parse!(PersonRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.people = doc.people;
        }
        "acquisition-cluster-registry" => {
            let doc = parse!(AcquisitionRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.acquisition_clusters = doc.acquisition_clusters;
        }
        "lineage-root-registry" => {
            let doc = parse!(LineageRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.lineage_roots = doc.lineage_roots;
        }
        "duplicate-family-registry" => {
            let doc = parse!(DuplicateRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.duplicate_families = doc.duplicate_families;
        }
        "hard-slice-registry" => {
            let doc = parse!(HardSliceRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.hard_slices = doc.hard_slices;
        }
        "partition-registry" => {
            let doc = parse!(PartitionRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.partitions = doc.partitions;
        }
        "gallery-envelope-manifest" => {
            let doc = parse!(GalleryEnvelopeDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.gallery_envelope = Some(doc.gallery_envelope);
        }
        "gallery-composition-manifest" => {
            let doc = parse!(GalleryCompositionDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.gallery_members = doc.gallery_members;
        }
        "probe-selection-manifest" => {
            let doc = parse!(ProbeSelectionDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.selected_probes = doc.selected_probes;
            graph.selected_pairs = doc.selected_pairs;
        }
        "spent-activation-set-registry" => {
            let doc = parse!(SpentSetRegistryDocument);
            (doc.schema_version == CALIBRATION_SCHEMA_VERSION)
                .then_some(())
                .ok_or(())?;
            graph.spent_sets = doc.spent_sets;
        }
        _ => return Err(()),
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct CalibrationVerification {
    pub artifact_id: String,
    pub verdict: String,
    pub strict_automatic_enabled: bool,
    pub failure_codes: Vec<String>,
    pub verified_manifest_hashes: BTreeMap<String, String>,
    pub reconstructed_metrics: Vec<CalibrationMetricVerdict>,
    pub privacy: CalibrationPrivacy,
    #[serde(skip_serializing)]
    verified_claim: Option<VerifiedCalibrationClaim>,
    #[serde(skip_serializing)]
    observed_trial_claim: Option<ObservedCalibrationClaim>,
}

/// Private capability proving that a signed calibration evaluation observed
/// the listed People/acquisition clusters. It can be issued for a statistically
/// failing run so failed candidates are still irreversibly spent.
#[derive(Clone, Debug)]
pub struct ObservedCalibrationClaim {
    activation_run_id: String,
    fixture_manifest_sha256: String,
    evidence_digest: String,
    observed_person_id_hashes: Vec<String>,
    observed_acquisition_cluster_id_hashes: Vec<String>,
}

impl ObservedCalibrationClaim {
    pub fn activation_run_id(&self) -> &str {
        &self.activation_run_id
    }
    pub fn fixture_manifest_sha256(&self) -> &str {
        &self.fixture_manifest_sha256
    }
    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }
    pub fn observed_person_id_hashes(&self) -> &[String] {
        &self.observed_person_id_hashes
    }
    pub fn observed_acquisition_cluster_id_hashes(&self) -> &[String] {
        &self.observed_acquisition_cluster_id_hashes
    }
}

/// Non-forgeable runtime capability issued only by the independent verifier.
/// Fields are intentionally private and this type is not deserializable.
#[derive(Clone, Debug)]
pub struct VerifiedCalibrationClaim {
    activation_run_id: String,
    calibration_generation: String,
    model_generation: String,
    envelope_hash: String,
    runtime_configuration_digest: String,
    gallery_composition_hash: String,
    gallery_members_digest: String,
    artifact_id: String,
    contract_sha256: String,
    raw_records_sha256: String,
    review_digest: String,
    evidence_digest: String,
    fixture_manifest_sha256: String,
    observed_person_id_hashes: Vec<String>,
    observed_acquisition_cluster_id_hashes: Vec<String>,
    spent_person_id_hashes: Vec<String>,
    spent_acquisition_cluster_id_hashes: Vec<String>,
    automatic_threshold: f64,
    suggestion_threshold: f64,
    runner_up_margin: f64,
    minimum_quality: f64,
    candidate_k: usize,
    rerank_k: usize,
    people_max: usize,
    looks_per_person_max: usize,
    trusted_templates_per_look_max: usize,
    total_templates_max: usize,
}

impl VerifiedCalibrationClaim {
    pub fn activation_run_id(&self) -> &str {
        &self.activation_run_id
    }
    pub fn calibration_generation(&self) -> &str {
        &self.calibration_generation
    }
    pub fn model_generation(&self) -> &str {
        &self.model_generation
    }
    pub fn envelope_hash(&self) -> &str {
        &self.envelope_hash
    }
    pub fn runtime_configuration_digest(&self) -> &str {
        &self.runtime_configuration_digest
    }
    pub fn gallery_composition_hash(&self) -> &str {
        &self.gallery_composition_hash
    }
    /// SHA-256 of UTF-8 tuples sorted lexically and joined with LF. Each tuple
    /// is `person_id\0look_id\0trusted_template_set_id\0observation_id`.
    pub fn gallery_members_digest(&self) -> &str {
        &self.gallery_members_digest
    }
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }
    pub fn contract_sha256(&self) -> &str {
        &self.contract_sha256
    }
    pub fn raw_records_sha256(&self) -> &str {
        &self.raw_records_sha256
    }
    pub fn review_digest(&self) -> &str {
        &self.review_digest
    }
    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }
    pub fn fixture_manifest_sha256(&self) -> &str {
        &self.fixture_manifest_sha256
    }
    pub fn observed_person_id_hashes(&self) -> &[String] {
        &self.observed_person_id_hashes
    }
    pub fn observed_acquisition_cluster_id_hashes(&self) -> &[String] {
        &self.observed_acquisition_cluster_id_hashes
    }
    pub fn spent_person_id_hashes(&self) -> &[String] {
        &self.spent_person_id_hashes
    }
    pub fn spent_acquisition_cluster_id_hashes(&self) -> &[String] {
        &self.spent_acquisition_cluster_id_hashes
    }
    pub fn automatic_threshold(&self) -> f64 {
        self.automatic_threshold
    }
    pub fn suggestion_threshold(&self) -> f64 {
        self.suggestion_threshold
    }
    pub fn runner_up_margin(&self) -> f64 {
        self.runner_up_margin
    }
    pub fn minimum_quality(&self) -> f64 {
        self.minimum_quality
    }
    pub fn candidate_k(&self) -> usize {
        self.candidate_k
    }
    pub fn rerank_k(&self) -> usize {
        self.rerank_k
    }
    pub fn people_max(&self) -> usize {
        self.people_max
    }
    pub fn looks_per_person_max(&self) -> usize {
        self.looks_per_person_max
    }
    pub fn trusted_templates_per_look_max(&self) -> usize {
        self.trusted_templates_per_look_max
    }
    pub fn total_templates_max(&self) -> usize {
        self.total_templates_max
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        model_generation: &str,
        calibration_generation: &str,
        gallery_members_digest: &str,
    ) -> Self {
        Self {
            activation_run_id: "test-run".to_string(),
            calibration_generation: calibration_generation.to_string(),
            model_generation: model_generation.to_string(),
            envelope_hash: "e".repeat(64),
            runtime_configuration_digest: strict_runtime_configuration_digest(100, 50),
            gallery_composition_hash: "c".repeat(64),
            gallery_members_digest: gallery_members_digest.to_string(),
            artifact_id: CALIBRATION_ARTIFACT_ID.to_string(),
            contract_sha256: "a".repeat(64),
            raw_records_sha256: "b".repeat(64),
            review_digest: "f".repeat(64),
            evidence_digest: "f".repeat(64),
            fixture_manifest_sha256: "1".repeat(64),
            observed_person_id_hashes: vec!["2".repeat(64)],
            observed_acquisition_cluster_id_hashes: vec!["3".repeat(64)],
            spent_person_id_hashes: vec!["4".repeat(64)],
            spent_acquisition_cluster_id_hashes: vec!["5".repeat(64)],
            automatic_threshold: 0.9,
            suggestion_threshold: 0.8,
            runner_up_margin: 0.1,
            minimum_quality: 0.7,
            candidate_k: 100,
            rerank_k: 50,
            people_max: 10_000,
            looks_per_person_max: 8,
            trusted_templates_per_look_max: 16,
            total_templates_max: 100_000,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_oversized_candidate_set(
        model_generation: &str,
        calibration_generation: &str,
        gallery_members_digest: &str,
    ) -> Self {
        let mut claim = Self::for_test(
            model_generation,
            calibration_generation,
            gallery_members_digest,
        );
        claim.activation_run_id = "test-run-oversized-candidate-set".to_string();
        claim.fixture_manifest_sha256 = "6".repeat(64);
        claim.observed_person_id_hashes = vec!["7".repeat(64)];
        claim.observed_acquisition_cluster_id_hashes = vec!["8".repeat(64)];
        claim.candidate_k = 1_001;
        claim.runtime_configuration_digest = strict_runtime_configuration_digest(1_001, 50);
        claim
    }
}

impl CalibrationVerification {
    pub fn verified_claim(&self) -> Option<&VerifiedCalibrationClaim> {
        self.verified_claim.as_ref()
    }

    pub fn observed_trial_claim(&self) -> Option<&ObservedCalibrationClaim> {
        self.observed_trial_claim.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(claim: VerifiedCalibrationClaim) -> Self {
        let observed_trial_claim = ObservedCalibrationClaim {
            activation_run_id: claim.activation_run_id.clone(),
            fixture_manifest_sha256: claim.fixture_manifest_sha256.clone(),
            evidence_digest: claim.evidence_digest.clone(),
            observed_person_id_hashes: claim.observed_person_id_hashes.clone(),
            observed_acquisition_cluster_id_hashes: claim
                .observed_acquisition_cluster_id_hashes
                .clone(),
        };
        Self {
            artifact_id: claim.artifact_id.clone(),
            verdict: "pass".to_string(),
            strict_automatic_enabled: true,
            failure_codes: Vec::new(),
            verified_manifest_hashes: BTreeMap::new(),
            reconstructed_metrics: Vec::new(),
            privacy: CalibrationPrivacy {
                evaluation_root_redacted: true,
                fixture_paths_emitted: false,
                raw_records_emitted: false,
            },
            verified_claim: Some(claim),
            observed_trial_claim: Some(observed_trial_claim),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CalibrationMetricVerdict {
    pub metric_id: String,
    pub scope: String,
    pub denominator: usize,
    pub errors: usize,
    pub distinct_people: usize,
    pub distinct_acquisition_clusters: usize,
    pub point_estimate: f64,
    pub one_sided_95_ucb: Option<f64>,
    pub target: f64,
    pub sufficient: bool,
    pub verdict: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CalibrationPrivacy {
    pub evaluation_root_redacted: bool,
    pub fixture_paths_emitted: bool,
    pub raw_records_emitted: bool,
}

fn push_failure(failures: &mut BTreeSet<String>, code: &str) {
    failures.insert(code.to_string());
}

#[cfg(windows)]
pub(crate) fn opened_file_final_path(file: &File, role: &str) -> Result<PathBuf, String> {
    let handle = file.as_raw_handle() as HANDLE;
    let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, flags) };
    if required == 0 {
        return Err(format!("{role}_handle_identity_failed"));
    }
    let mut buffer = vec![0_u16; required as usize + 1];
    let written = unsafe {
        GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
    };
    if written == 0 || written as usize >= buffer.len() {
        return Err(format!("{role}_handle_identity_failed"));
    }
    Ok(PathBuf::from(OsString::from_wide(
        &buffer[..written as usize],
    )))
}

#[cfg(all(not(windows), unix))]
pub(crate) fn opened_file_final_path(file: &File, role: &str) -> Result<PathBuf, String> {
    let link = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let target = fs::read_link(link).map_err(|_| format!("{role}_handle_identity_failed"))?;
    fs::canonicalize(target).map_err(|_| format!("{role}_handle_identity_failed"))
}

#[cfg(all(not(windows), not(unix)))]
pub(crate) fn opened_file_final_path(_file: &File, role: &str) -> Result<PathBuf, String> {
    Err(format!("{role}_stable_handle_identity_unsupported"))
}

fn read_opened_bounded_file(
    mut file: File,
    expected_path: &Path,
    root: Option<&Path>,
    role: &str,
) -> Result<Vec<u8>, String> {
    let before_path = opened_file_final_path(&file, role)?;
    if before_path != expected_path || root.is_some_and(|root| !before_path.starts_with(root)) {
        return Err(format!("{role}_handle_identity_mismatch"));
    }
    let before = file.metadata().map_err(|_| format!("{role}_read_failed"))?;
    if !before.is_file() || before.len() == 0 || before.len() > CALIBRATION_MAX_FILE_BYTES {
        return Err(format!("{role}_invalid_size"));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(CALIBRATION_MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| format!("{role}_read_failed"))?;
    let after = file.metadata().map_err(|_| format!("{role}_read_failed"))?;
    let after_path = opened_file_final_path(&file, role)?;
    if bytes.len() as u64 != before.len()
        || after.len() != before.len()
        || after_path != before_path
    {
        return Err(format!("{role}_changed_during_read"));
    }
    Ok(bytes)
}

fn read_bounded_regular_file(path: &Path, role: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("{role}_missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{role}_unsafe_file_type"));
    }
    let canonical = fs::canonicalize(path).map_err(|_| format!("{role}_missing"))?;
    let file = OpenOptions::new()
        .read(true)
        .open(&canonical)
        .map_err(|_| format!("{role}_read_failed"))?;
    read_opened_bounded_file(file, &canonical, None, role)
}

fn secure_calibration_file(root: &Path, relative: &str, role: &str) -> Result<Vec<u8>, String> {
    let relative =
        validate_relative_path(relative, role).map_err(|_| format!("{role}_unsafe_path"))?;
    let target = root.join(relative);
    let target_metadata = fs::symlink_metadata(&target).map_err(|_| format!("{role}_missing"))?;
    if target_metadata.file_type().is_symlink() || !target_metadata.is_file() {
        return Err(format!("{role}_unsafe_file_type"));
    }
    let canonical = fs::canonicalize(&target).map_err(|_| format!("{role}_missing"))?;
    if !canonical.starts_with(root) {
        return Err(format!("{role}_root_escape"));
    }
    let file = OpenOptions::new()
        .read(true)
        .open(&canonical)
        .map_err(|_| format!("{role}_read_failed"))?;
    read_opened_bounded_file(file, &canonical, Some(root), role)
}

fn verify_fixture_media(
    root: &Path,
    relative: &str,
    expected_sha256: &str,
    total_bytes: &mut u64,
) -> Result<(), String> {
    let relative = validate_relative_path(relative, "fixture_media")
        .map_err(|_| "fixture_media_unsafe_path".to_string())?;
    let target = root.join(relative);
    let path_metadata =
        fs::symlink_metadata(&target).map_err(|_| "fixture_media_missing".to_string())?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err("fixture_media_unsafe_file_type".to_string());
    }
    let canonical = fs::canonicalize(&target).map_err(|_| "fixture_media_missing".to_string())?;
    if !canonical.starts_with(root) {
        return Err("fixture_media_root_escape".to_string());
    }
    // The opened handle, not a later path lookup, is the hash authority. A
    // concurrent rename/replacement therefore cannot redirect the stream.
    let mut file = OpenOptions::new()
        .read(true)
        .open(&canonical)
        .map_err(|_| "fixture_media_read_failed".to_string())?;
    let opened_path = opened_file_final_path(&file, "fixture_media")?;
    if opened_path != canonical || !opened_path.starts_with(root) {
        return Err("fixture_media_handle_identity_mismatch".to_string());
    }
    let before = file
        .metadata()
        .map_err(|_| "fixture_media_read_failed".to_string())?;
    if !before.is_file() || before.len() == 0 || before.len() > CALIBRATION_MAX_MEDIA_BYTES {
        return Err("fixture_media_invalid_size".to_string());
    }
    *total_bytes = total_bytes
        .checked_add(before.len())
        .ok_or_else(|| "fixture_media_total_size_overflow".to_string())?;
    if *total_bytes > CALIBRATION_MAX_TOTAL_MEDIA_BYTES {
        return Err("fixture_media_total_size_exceeded".to_string());
    }
    let mut hasher = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| "fixture_media_read_failed".to_string())?;
        if count == 0 {
            break;
        }
        observed = observed
            .checked_add(count as u64)
            .ok_or_else(|| "fixture_media_size_overflow".to_string())?;
        if observed > before.len() || observed > CALIBRATION_MAX_MEDIA_BYTES {
            return Err("fixture_media_changed_during_read".to_string());
        }
        hasher.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .map_err(|_| "fixture_media_read_failed".to_string())?;
    let after_path = opened_file_final_path(&file, "fixture_media")?;
    if observed != before.len() || after.len() != before.len() || after_path != opened_path {
        return Err("fixture_media_changed_during_read".to_string());
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != expected_sha256 {
        return Err("fixture_media_hash_mismatch".to_string());
    }
    Ok(())
}

/// Exact one-sided Clopper-Pearson 95% upper confidence bound.
pub fn clopper_pearson_upper_95(errors: usize, trials: usize) -> Option<f64> {
    if trials == 0 || errors > trials {
        return None;
    }
    if errors == trials {
        return Some(1.0);
    }
    if errors == 0 {
        return Some(-((0.05_f64).ln() / trials as f64).exp_m1());
    }
    Beta::new((errors + 1) as f64, (trials - errors) as f64)
        .ok()
        .map(|distribution| distribution.inverse_cdf(0.95))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_lower_hex(value: &str, expected_bytes: usize) -> Option<Vec<u8>> {
    if value.len() != expected_bytes.checked_mul(2)?
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            Some((nibble(pair[0])? << 4) | nibble(pair[1])?)
        })
        .collect()
}

fn independent_review_signature_valid(review: &CalibrationReview, digest: &str) -> bool {
    let (Some(trusted_key_id), Some(public_key_hex)) =
        (TRUSTED_REVIEWER_KEY_ID, TRUSTED_REVIEWER_PUBLIC_KEY_HEX)
    else {
        return false;
    };
    if review.reviewer_key_id.as_deref() != Some(trusted_key_id) {
        return false;
    }
    review
        .signature_ed25519_hex
        .as_deref()
        .is_some_and(|signature| {
            ed25519_signature_valid(public_key_hex, signature, digest.as_bytes())
        })
}

fn ed25519_signature_valid(public_key_hex: &str, signature_hex: &str, message: &[u8]) -> bool {
    let Some(public_key) = decode_lower_hex(public_key_hex, 32) else {
        return false;
    };
    let Some(signature) = decode_lower_hex(signature_hex, 64) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, &signature)
        .is_ok()
}

fn canonical_contract_location(path: &Path) -> bool {
    let Ok(canonical) = fs::canonicalize(path) else {
        return false;
    };
    trusted_runtime_repo_root().is_some_and(|repo_root| {
        fs::canonicalize(
            repo_root
                .join("governance")
                .join("validation")
                .join("wp-082-match-calibration-v1.yaml"),
        )
        .is_ok_and(|trusted| trusted == canonical)
    })
}

fn trusted_runtime_repo_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    executable.ancestors().find_map(|candidate| {
        (candidate.join("CODEX.md").is_file()
            && candidate.join("topology.yaml").is_file()
            && candidate.join("product").join("Cargo.toml").is_file())
        .then(|| candidate.to_path_buf())
    })
}

fn unique_nonempty<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    values.all(|value| !value.trim().is_empty() && seen.insert(value))
}

#[derive(Clone, Debug)]
struct VerifiedManifestGraph {
    envelope: GalleryEnvelope,
    gallery_members_digest: String,
}

fn bounded_scalar(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= CALIBRATION_MAX_SCALAR_BYTES
}

fn all_string_scalars_bounded(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => value.len() <= CALIBRATION_MAX_SCALAR_BYTES,
        serde_json::Value::Array(values) => values.iter().all(all_string_scalars_bounded),
        serde_json::Value::Object(values) => values.values().all(all_string_scalars_bounded),
        _ => true,
    }
}

fn manifest_record_count(graph: &CalibrationEvidenceGraph) -> Option<usize> {
    graph
        .fixtures
        .len()
        .checked_add(graph.people.len())?
        .checked_add(graph.acquisition_clusters.len())?
        .checked_add(graph.lineage_roots.len())?
        .checked_add(graph.duplicate_families.len())?
        .checked_add(graph.hard_slices.len())?
        .checked_add(graph.partitions.len())?
        .checked_add(graph.gallery_members.len())?
        .checked_add(graph.selected_probes.len())?
        .checked_add(graph.selected_pairs.len())?
        .checked_add(graph.spent_sets.len())
}

fn derived_hard_slice_ids(
    fixture: &CalibrationFixture,
    members: &[GalleryMember],
    fixture_by_id: &HashMap<&str, &CalibrationFixture>,
    duplicate_family_sizes: &HashMap<&str, usize>,
) -> Result<HashSet<String>, ()> {
    let evidence = &fixture.slice_evidence;
    if !bounded_scalar(&evidence.annotation_provenance)
        || evidence
            .yaw_degrees
            .is_some_and(|value| !value.is_finite() || !(-180.0..=180.0).contains(&value))
        || evidence
            .pitch_degrees
            .is_some_and(|value| !value.is_finite() || !(-90.0..=90.0).contains(&value))
        || evidence
            .face_occlusion_fraction
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        || (!evidence.exposure_label.is_empty()
            && !matches!(
                evidence.exposure_label.as_str(),
                "normal" | "underexposed" | "overexposed"
            ))
        || (!evidence.blur_label.is_empty()
            && !matches!(evidence.blur_label.as_str(), "acceptable" | "poor"))
        || (!evidence.source_kind.is_empty()
            && !matches!(
                evidence.source_kind.as_str(),
                "camera" | "screenshot" | "collage" | "poster"
            ))
        || (!evidence.origin_kind.is_empty()
            && !matches!(
                evidence.origin_kind.as_str(),
                "captured" | "synthetic" | "ai_rendered"
            ))
        || evidence
            .demographic_cohort_ids
            .iter()
            .any(|id| !bounded_scalar(id) || id.contains(':'))
    {
        return Err(());
    }

    let mut derived = HashSet::new();
    let gallery_references = members.iter().filter_map(|member| {
        let candidate = fixture_by_id.get(member.fixture_id.as_str())?;
        (candidate.ground_truth_person_id == fixture.ground_truth_person_id)
            .then_some((member, *candidate))
    });
    let references = gallery_references.collect::<Vec<_>>();
    if fixture
        .lookalike_rival_group_id
        .as_deref()
        .is_some_and(|group| {
            bounded_scalar(group)
                && members.iter().any(|member| {
                    member.person_id != fixture.ground_truth_person_id
                        && fixture_by_id
                            .get(member.fixture_id.as_str())
                            .and_then(|candidate| candidate.lookalike_rival_group_id.as_deref())
                            == Some(group)
                })
        })
    {
        derived.insert("lookalike_twin_impostor".to_string());
    }
    if evidence
        .yaw_degrees
        .is_some_and(|value| value.abs() >= 30.0)
        || evidence
            .pitch_degrees
            .is_some_and(|value| value.abs() >= 20.0)
    {
        derived.insert("nonfrontal_pose".to_string());
    }
    if references.iter().any(|(member, reference)| {
        (!evidence.look_id.is_empty() && member.look_id != evidence.look_id)
            || evidence
                .capture_day
                .zip(reference.slice_evidence.capture_day)
                .is_some_and(|(probe_day, reference_day)| probe_day.abs_diff(reference_day) >= 365)
    }) {
        derived.insert("known_Look_age_time_gap".to_string());
    }
    if evidence.major_styling_or_makeup_change {
        derived.insert("styling_makeup".to_string());
    }
    if evidence.wig_or_major_hair_change {
        derived.insert("wig_hair_change".to_string());
    }
    if evidence.glasses == Some(true)
        || references.iter().any(|(_, reference)| {
            evidence.glasses.is_some()
                && reference.slice_evidence.glasses.is_some()
                && evidence.glasses != reference.slice_evidence.glasses
        })
    {
        derived.insert("glasses".to_string());
    }
    if evidence.mask_or_partial_occlusion
        || evidence
            .face_occlusion_fraction
            .is_some_and(|value| value >= 0.20)
    {
        derived.insert("mask_partial_occlusion".to_string());
    }
    if matches!(
        evidence.exposure_label.as_str(),
        "underexposed" | "overexposed"
    ) {
        derived.insert("poor_exposure".to_string());
    }
    if evidence.face_box_width.is_some_and(|value| value < 64)
        || evidence.face_box_height.is_some_and(|value| value < 64)
        || evidence.blur_label == "poor"
    {
        derived.insert("small_blurred_face".to_string());
    }
    if evidence.lossy_reencode
        || evidence
            .source_short_edge_before_upscale
            .is_some_and(|value| value < 256)
    {
        derived.insert("compression_resize".to_string());
    }
    if evidence.source_kind == "screenshot" {
        derived.insert("screenshot".to_string());
    }
    if matches!(evidence.source_kind.as_str(), "collage" | "poster")
        || evidence
            .ground_truth_face_count
            .is_some_and(|value| value >= 4)
    {
        derived.insert("collage_poster_multiface".to_string());
    }
    if matches!(evidence.origin_kind.as_str(), "synthetic" | "ai_rendered") {
        derived.insert("synthetic_media".to_string());
    }
    if duplicate_family_sizes
        .get(fixture.duplicate_family_id.as_str())
        .is_some_and(|count| *count >= 2)
        || !fixture.burst_id.is_empty()
        || !fixture.video_track_id.is_empty()
    {
        derived.insert("duplicate_burst_video_family".to_string());
    }
    if !evidence.demographic_cohort_ids.is_empty() {
        derived.insert("demographic_cohort".to_string());
        for cohort in &evidence.demographic_cohort_ids {
            derived.insert(format!("demographic_cohort:{cohort}"));
        }
    }
    Ok(derived)
}

fn semantic_gallery_members_digest(members: &[GalleryMember]) -> String {
    let mut tuples = members
        .iter()
        .map(|member| {
            format!(
                "{}\0{}\0{}\0{}",
                member.person_id,
                member.look_id,
                member.trusted_template_set_id,
                member.observation_id
            )
        })
        .collect::<Vec<_>>();
    tuples.sort_unstable();
    sha256_hex(tuples.join("\n").as_bytes())
}

fn canonical_slice_membership_predicate(slice_id: &str) -> Option<&'static str> {
    Some(match slice_id {
        "lookalike_twin_impostor" => "shared-nonnull-rival-group-with-different-enrolled-person-v1",
        "nonfrontal_pose" => "abs-yaw-gte-30-or-abs-pitch-gte-20-v1",
        "known_Look_age_time_gap" => "different-look-or-capture-gap-gte-365-days-v1",
        "styling_makeup" => "major-styling-or-makeup-annotation-v1",
        "wig_hair_change" => "wig-or-major-hair-change-annotation-v1",
        "glasses" => "probe-glasses-or-mated-state-difference-v1",
        "mask_partial_occlusion" => "mask-or-occlusion-fraction-gte-0.20-v1",
        "poor_exposure" => "underexposed-or-overexposed-v1",
        "small_blurred_face" => "face-side-lt-64-or-poor-blur-v1",
        "compression_resize" => "lossy-reencode-or-upscale-source-edge-lt-256-v1",
        "screenshot" => "source-kind-screenshot-v1",
        "collage_poster_multiface" => "collage-poster-or-face-count-gte-4-v1",
        "synthetic_media" => "origin-kind-synthetic-or-ai-rendered-v1",
        "duplicate_burst_video_family" => "duplicate-family-gte-2-or-burst-or-track-v1",
        "demographic_cohort" => "one-or-more-predeclared-ground-truth-cohorts-v1",
        id if id.starts_with("demographic_cohort:") => "ground-truth-cohort-equals-slice-suffix-v1",
        _ => return None,
    })
}

fn canonical_router_predicate(automatic_eligibility: bool) -> &'static str {
    if automatic_eligibility {
        "allow-after-slice-specific-statistical-gates-v1"
    } else {
        "abstain-on-derived-slice-membership-v1"
    }
}

fn verify_manifest_graph(
    graph: &CalibrationEvidenceGraph,
    hashes: &BTreeMap<String, String>,
    contract: &CalibrationContract,
    root: &Path,
    failures: &mut BTreeSet<String>,
) -> Option<VerifiedManifestGraph> {
    let fixtures = &graph.fixtures;
    let people = &graph.people;
    let acquisitions = &graph.acquisition_clusters;
    let lineages = &graph.lineage_roots;
    let duplicates = &graph.duplicate_families;
    let slices = &graph.hard_slices;
    let partitions = &graph.partitions;
    let members = &graph.gallery_members;
    let probes = &graph.selected_probes;
    let spent = &graph.spent_sets;
    let envelope = graph.gallery_envelope.clone()?;

    let total_records = fixtures
        .len()
        .checked_add(people.len())?
        .checked_add(acquisitions.len())?
        .checked_add(lineages.len())?
        .checked_add(duplicates.len())?
        .checked_add(slices.len())?
        .checked_add(partitions.len())?
        .checked_add(members.len())?
        .checked_add(probes.len())?
        .checked_add(graph.selected_pairs.len())?
        .checked_add(spent.len())?;
    if total_records == 0 || total_records > CALIBRATION_MAX_RECORDS {
        push_failure(failures, "evidence_record_bound_exceeded");
    }

    if !unique_nonempty(fixtures.iter().map(|r| r.fixture_id.as_str()))
        || !unique_nonempty(people.iter().map(|r| r.person_id.as_str()))
        || !unique_nonempty(slices.iter().map(|r| r.slice_id.as_str()))
        || !unique_nonempty(partitions.iter().map(|r| r.partition_id.as_str()))
        || !unique_nonempty(probes.iter().map(|r| r.probe_id.as_str()))
    {
        push_failure(failures, "manifest_duplicate_or_empty_id");
    }
    if !unique_nonempty(
        acquisitions
            .iter()
            .map(|r| r.acquisition_cluster_id.as_str()),
    ) || !unique_nonempty(lineages.iter().map(|r| r.lineage_root_id.as_str()))
        || !unique_nonempty(duplicates.iter().map(|r| r.duplicate_family_id.as_str()))
    {
        push_failure(failures, "registry_duplicate_or_empty_id");
    }

    let fixture_ids: HashSet<_> = fixtures.iter().map(|r| r.fixture_id.as_str()).collect();
    let person_ids: HashSet<_> = people.iter().map(|r| r.person_id.as_str()).collect();
    let partition_ids: HashSet<_> = partitions.iter().map(|r| r.partition_id.as_str()).collect();
    let slice_ids: HashSet<_> = slices.iter().map(|r| r.slice_id.as_str()).collect();
    let fixture_by_id: HashMap<_, _> = fixtures
        .iter()
        .map(|fixture| (fixture.fixture_id.as_str(), fixture))
        .collect();
    let person_by_id: HashMap<_, _> = people
        .iter()
        .map(|person| (person.person_id.as_str(), person))
        .collect();
    let duplicate_family_sizes: HashMap<_, _> = duplicates
        .iter()
        .map(|family| {
            (
                family.duplicate_family_id.as_str(),
                family.member_fixture_ids.len(),
            )
        })
        .collect();
    let mut total_media_bytes = 0_u64;
    let mut observed_media_hashes = HashSet::new();
    for fixture in fixtures {
        let structurally_complete = bounded_scalar(&fixture.fixture_id)
            && bounded_scalar(&fixture.media_relative_path)
            && valid_sha256(&fixture.media_sha256)
            && bounded_scalar(&fixture.source_asset_id)
            && bounded_scalar(&fixture.ground_truth_person_id)
            && bounded_scalar(&fixture.acquisition_cluster_id)
            && bounded_scalar(&fixture.lineage_root_id)
            && bounded_scalar(&fixture.duplicate_family_id)
            && bounded_scalar(&fixture.partition_id)
            && bounded_scalar(&fixture.capture_session_id)
            && bounded_scalar(&fixture.burst_id)
            && bounded_scalar(&fixture.video_track_id)
            && fixture
                .lookalike_rival_group_id
                .as_deref()
                .is_none_or(bounded_scalar)
            && person_ids.contains(fixture.ground_truth_person_id.as_str())
            && partition_ids.contains(fixture.partition_id.as_str())
            && fixture
                .hard_slice_ids
                .iter()
                .all(|id| bounded_scalar(id) && slice_ids.contains(id.as_str()));
        if !structurally_complete {
            push_failure(failures, "fixture_invalid_or_incomplete");
        }
        if !observed_media_hashes.insert(fixture.media_sha256.as_str()) {
            push_failure(failures, "duplicate_fixture_media_hash");
        } else if validate_relative_path(&fixture.media_relative_path, "fixture_media").is_err() {
            push_failure(failures, "fixture_media_unsafe_path");
        } else if let Err(code) = verify_fixture_media(
            root,
            &fixture.media_relative_path,
            &fixture.media_sha256,
            &mut total_media_bytes,
        ) {
            push_failure(failures, &code);
        }
    }
    let mut acquisition_membership = HashMap::new();
    for record in acquisitions {
        if !bounded_scalar(&record.closure_basis)
            || record.member_fixture_ids.is_empty()
            || !unique_nonempty(record.member_fixture_ids.iter().map(String::as_str))
        {
            push_failure(failures, "registry_membership_invalid");
        }
        for member in &record.member_fixture_ids {
            if fixture_by_id.get(member.as_str()).is_none_or(|fixture| {
                fixture.acquisition_cluster_id != record.acquisition_cluster_id
            }) || acquisition_membership
                .insert(member.as_str(), record.acquisition_cluster_id.as_str())
                .is_some()
            {
                push_failure(failures, "registry_membership_mismatch");
            }
        }
    }
    let mut lineage_membership = HashMap::new();
    for record in lineages {
        if !bounded_scalar(&record.derivation_basis)
            || record.member_fixture_ids.is_empty()
            || !unique_nonempty(record.member_fixture_ids.iter().map(String::as_str))
        {
            push_failure(failures, "registry_membership_invalid");
        }
        for member in &record.member_fixture_ids {
            if fixture_by_id
                .get(member.as_str())
                .is_none_or(|fixture| fixture.lineage_root_id != record.lineage_root_id)
                || lineage_membership
                    .insert(member.as_str(), record.lineage_root_id.as_str())
                    .is_some()
            {
                push_failure(failures, "registry_membership_mismatch");
            }
        }
    }
    let mut duplicate_membership = HashMap::new();
    for record in duplicates {
        if !bounded_scalar(&record.family_method)
            || record.member_fixture_ids.is_empty()
            || !unique_nonempty(record.member_fixture_ids.iter().map(String::as_str))
        {
            push_failure(failures, "registry_membership_invalid");
        }
        for member in &record.member_fixture_ids {
            if fixture_by_id
                .get(member.as_str())
                .is_none_or(|fixture| fixture.duplicate_family_id != record.duplicate_family_id)
                || duplicate_membership
                    .insert(member.as_str(), record.duplicate_family_id.as_str())
                    .is_some()
            {
                push_failure(failures, "registry_membership_mismatch");
            }
        }
    }
    if acquisition_membership.len() != fixtures.len()
        || lineage_membership.len() != fixtures.len()
        || duplicate_membership.len() != fixtures.len()
    {
        push_failure(failures, "registry_membership_mismatch");
    }

    // An acquisition cluster is the transitive closure of every provenance
    // dimension that can correlate trials. A manifest cannot manufacture
    // independence by assigning different cluster IDs to fixtures that share
    // a source/session/burst/track/family/lineage root.
    let mut acquisition_by_dimension: HashMap<(&str, &str), &str> = HashMap::new();
    for fixture in fixtures {
        for (dimension, value) in [
            ("source", fixture.source_asset_id.as_str()),
            ("session", fixture.capture_session_id.as_str()),
            ("burst", fixture.burst_id.as_str()),
            ("track", fixture.video_track_id.as_str()),
            ("family", fixture.duplicate_family_id.as_str()),
            ("lineage", fixture.lineage_root_id.as_str()),
        ] {
            if value.is_empty() {
                continue;
            }
            if acquisition_by_dimension
                .insert((dimension, value), fixture.acquisition_cluster_id.as_str())
                .is_some_and(|previous| previous != fixture.acquisition_cluster_id)
            {
                push_failure(failures, "acquisition_cluster_not_closed");
            }
        }
    }

    for person in people {
        if !bounded_scalar(&person.person_id)
            || !bounded_scalar(&person.partition_id)
            || !partition_ids.contains(person.partition_id.as_str())
        {
            push_failure(failures, "person_partition_missing");
        }
    }
    let mut partition_person_count: HashMap<&str, usize> = HashMap::new();
    let mut partition_fixture_count: HashMap<&str, usize> = HashMap::new();
    for partition in partitions {
        if !matches!(partition.purpose.as_str(), "calibration" | "test")
            || !bounded_scalar(&partition.frozen_at)
            || !unique_nonempty(partition.member_person_ids.iter().map(String::as_str))
            || !unique_nonempty(partition.member_fixture_ids.iter().map(String::as_str))
            || partition
                .member_person_ids
                .iter()
                .any(|id| !person_ids.contains(id.as_str()))
            || partition
                .member_fixture_ids
                .iter()
                .any(|id| !fixture_ids.contains(id.as_str()))
        {
            push_failure(failures, "partition_membership_invalid");
        }
        for id in &partition.member_person_ids {
            *partition_person_count.entry(id).or_default() += 1;
            if people
                .iter()
                .find(|p| &p.person_id == id)
                .map(|p| &p.partition_id)
                != Some(&partition.partition_id)
            {
                push_failure(failures, "partition_person_mismatch");
            }
        }
        for id in &partition.member_fixture_ids {
            *partition_fixture_count.entry(id).or_default() += 1;
            if fixtures
                .iter()
                .find(|f| &f.fixture_id == id)
                .map(|f| &f.partition_id)
                != Some(&partition.partition_id)
            {
                push_failure(failures, "partition_fixture_mismatch");
            }
        }
    }
    if people
        .iter()
        .any(|person| partition_person_count.get(person.person_id.as_str()) != Some(&1))
        || fixtures.iter().any(|fixture| {
            partition_fixture_count.get(fixture.fixture_id.as_str()) != Some(&1)
                || person_by_id
                    .get(fixture.ground_truth_person_id.as_str())
                    .is_none_or(|person| person.partition_id != fixture.partition_id)
        })
    {
        push_failure(failures, "partition_coverage_or_person_fixture_mismatch");
    }

    let mut owner_by_dimension: HashMap<(&str, &str), &str> = HashMap::new();
    for fixture in fixtures {
        let purpose = partitions
            .iter()
            .find(|p| p.partition_id == fixture.partition_id)
            .map(|p| p.purpose.as_str())
            .unwrap_or("");
        for (dimension, value) in [
            ("person", fixture.ground_truth_person_id.as_str()),
            ("source", fixture.source_asset_id.as_str()),
            ("session", fixture.capture_session_id.as_str()),
            ("burst", fixture.burst_id.as_str()),
            ("track", fixture.video_track_id.as_str()),
            ("family", fixture.duplicate_family_id.as_str()),
            ("lineage", fixture.lineage_root_id.as_str()),
        ] {
            if value.is_empty() {
                if matches!(dimension, "session" | "burst" | "track") {
                    push_failure(failures, "partition_provenance_incomplete");
                }
                continue;
            }
            if let Some(previous) = owner_by_dimension.insert((dimension, value), purpose) {
                if previous != purpose {
                    push_failure(failures, "calibration_test_leakage");
                }
            }
        }
    }

    let required_slices: HashSet<_> = contract
        .frozen_protocol
        .required_hard_slice_ids
        .iter()
        .map(String::as_str)
        .collect();
    let canonical_slices: HashSet<_> = REQUIRED_HARD_SLICES.into_iter().collect();
    if slice_ids != required_slices
        || !canonical_slices.is_subset(&slice_ids)
        || !slice_ids
            .iter()
            .any(|id| id.starts_with("demographic_cohort:"))
        || slice_ids.iter().any(|id| {
            !canonical_slices.contains(id)
                && !id
                    .strip_prefix("demographic_cohort:")
                    .is_some_and(bounded_scalar)
        })
        || slices.iter().any(|slice| {
            !bounded_scalar(&slice.slice_id)
                || canonical_slice_membership_predicate(&slice.slice_id)
                    != Some(slice.membership_predicate.as_str())
                || slice.runtime_router_predicate
                    != canonical_router_predicate(slice.automatic_eligibility)
        })
    {
        push_failure(failures, "hard_slice_registry_incomplete");
    }
    if slices.iter().any(|slice| !slice.automatic_eligibility) {
        // WP-084 owns the production excluded-slice router. WP-082 must not
        // activate evidence for a policy that the strict write path cannot yet
        // execute from the exact same implementation.
        push_failure(failures, "excluded_slice_runtime_router_unavailable");
    }

    let gallery_fixture_ids: HashSet<_> = members.iter().map(|m| m.fixture_id.as_str()).collect();
    let gallery_people: HashSet<_> = members.iter().map(|m| m.person_id.as_str()).collect();
    if !unique_nonempty(probes.iter().map(|probe| probe.fixture_id.as_str())) {
        push_failure(failures, "probe_fixture_reused");
    }
    if !unique_nonempty(members.iter().map(|m| m.observation_id.as_str()))
        || members.iter().any(|m| {
            !bounded_scalar(&m.person_id)
                || !bounded_scalar(&m.look_id)
                || !bounded_scalar(&m.trusted_template_set_id)
                || !bounded_scalar(&m.observation_id)
                || !bounded_scalar(&m.fixture_id)
                || !fixture_ids.contains(m.fixture_id.as_str())
                || !person_ids.contains(m.person_id.as_str())
                || fixture_by_id
                    .get(m.fixture_id.as_str())
                    .is_none_or(|fixture| fixture.ground_truth_person_id != m.person_id)
        })
    {
        push_failure(failures, "gallery_member_invalid");
    }
    let mut selected_probe_by_id = HashMap::new();
    for probe in probes {
        selected_probe_by_id.insert(probe.probe_id.as_str(), probe);
        let expected_slices = fixture_by_id
            .get(probe.fixture_id.as_str())
            .and_then(|fixture| {
                derived_hard_slice_ids(fixture, members, &fixture_by_id, &duplicate_family_sizes)
                    .ok()
            });
        let declared_fixture_slices: HashSet<_> = fixture_by_id
            .get(probe.fixture_id.as_str())
            .map(|fixture| fixture.hard_slice_ids.iter().cloned().collect())
            .unwrap_or_default();
        let observed_slices: HashSet<_> = probe.slice_ids.iter().cloned().collect();
        if !bounded_scalar(&probe.probe_id)
            || !bounded_scalar(&probe.fixture_id)
            || probe.slice_ids.iter().any(|id| !bounded_scalar(id))
            || !probe.predeclared_before_run
            || !fixture_ids.contains(probe.fixture_id.as_str())
            || probe
                .slice_ids
                .iter()
                .any(|id| !slice_ids.contains(id.as_str()))
            || gallery_fixture_ids.contains(probe.fixture_id.as_str())
            || expected_slices.as_ref() != Some(&declared_fixture_slices)
            || declared_fixture_slices != observed_slices
        {
            push_failure(failures, "probe_selection_invalid");
        }
        if let Some(fixture) = fixtures.iter().find(|f| f.fixture_id == probe.fixture_id) {
            if probe.probe_kind == ProbeKind::NonMated
                && gallery_people.contains(fixture.ground_truth_person_id.as_str())
            {
                push_failure(failures, "nonmated_person_present_in_gallery");
            }
            if probe.probe_kind == ProbeKind::Mated
                && !gallery_people.contains(fixture.ground_truth_person_id.as_str())
            {
                push_failure(failures, "mated_person_absent_from_gallery");
            }
            if probe
                .slice_ids
                .iter()
                .any(|id| id == "lookalike_twin_impostor")
            {
                let rival_group = fixture
                    .lookalike_rival_group_id
                    .as_deref()
                    .filter(|value| bounded_scalar(value));
                let has_enrolled_rival = rival_group.is_some_and(|group| {
                    members.iter().any(|member| {
                        member.person_id != fixture.ground_truth_person_id
                            && fixture_by_id
                                .get(member.fixture_id.as_str())
                                .and_then(|candidate| candidate.lookalike_rival_group_id.as_deref())
                                == Some(group)
                    })
                });
                if !has_enrolled_rival {
                    push_failure(failures, "lookalike_rival_group_evidence_missing");
                }
            }
            for member in members {
                if let Some(gallery_fixture) =
                    fixtures.iter().find(|f| f.fixture_id == member.fixture_id)
                {
                    if gallery_fixture.ground_truth_person_id == fixture.ground_truth_person_id
                        && [
                            &gallery_fixture.source_asset_id == &fixture.source_asset_id,
                            &gallery_fixture.capture_session_id == &fixture.capture_session_id,
                            &gallery_fixture.burst_id == &fixture.burst_id,
                            &gallery_fixture.video_track_id == &fixture.video_track_id,
                            &gallery_fixture.duplicate_family_id == &fixture.duplicate_family_id,
                            &gallery_fixture.lineage_root_id == &fixture.lineage_root_id,
                        ]
                        .into_iter()
                        .any(|same| same)
                    {
                        push_failure(failures, "gallery_probe_leakage");
                    }
                }
            }
        }
    }

    let mut unordered_pairs = HashSet::new();
    if !unique_nonempty(
        graph
            .selected_pairs
            .iter()
            .map(|pair| pair.pair_id.as_str()),
    ) {
        push_failure(failures, "pair_selection_invalid");
    }
    for pair in &graph.selected_pairs {
        let Some(left) = selected_probe_by_id.get(pair.left_probe_id.as_str()) else {
            push_failure(failures, "pair_selection_invalid");
            continue;
        };
        let Some(right) = selected_probe_by_id.get(pair.right_probe_id.as_str()) else {
            push_failure(failures, "pair_selection_invalid");
            continue;
        };
        let Some(left_fixture) = fixture_by_id.get(left.fixture_id.as_str()) else {
            continue;
        };
        let Some(right_fixture) = fixture_by_id.get(right.fixture_id.as_str()) else {
            continue;
        };
        let ordered = if pair.left_probe_id < pair.right_probe_id {
            (pair.left_probe_id.as_str(), pair.right_probe_id.as_str())
        } else {
            (pair.right_probe_id.as_str(), pair.left_probe_id.as_str())
        };
        if !bounded_scalar(&pair.pair_id)
            || !bounded_scalar(&pair.left_probe_id)
            || !bounded_scalar(&pair.right_probe_id)
            || !pair.predeclared_before_run
            || pair.left_probe_id == pair.right_probe_id
            || left_fixture.ground_truth_person_id == right_fixture.ground_truth_person_id
            || !unordered_pairs.insert(ordered)
        {
            push_failure(failures, "pair_selection_invalid");
        }
    }

    if !valid_sha256(&envelope.manifest_sha256)
        || hashes.get("gallery-composition-manifest") != Some(&envelope.manifest_sha256)
        || envelope.people_max == 0
        || envelope.looks_per_person_max == 0
        || envelope.trusted_templates_per_look_max == 0
        || envelope.total_templates_max == 0
        || envelope.selection_policy != STRICT_SELECTION_POLICY
        || ![
            envelope.thresholds.automatic_commit,
            envelope.thresholds.suggestion,
            envelope.thresholds.unnamed_cluster,
        ]
        .into_iter()
        .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        || envelope.thresholds.suggestion > envelope.thresholds.automatic_commit
        || envelope.aggregation != "best-template-per-look-then-best-look-per-person"
        || !envelope.margins.runner_up_minimum.is_finite()
        || !(0.0..=2.0).contains(&envelope.margins.runner_up_minimum)
        || !envelope.quality_gates.minimum_quality.is_finite()
        || !(0.0..=1.0).contains(&envelope.quality_gates.minimum_quality)
        || !envelope.quality_gates.alignment_required
        || !envelope.quality_gates.pose_gate_required
        || !envelope.cannot_link_gates.face_to_person_exclusion_required
        || envelope.ann_configuration.index_type != "hnsw"
        || envelope.ann_configuration.algorithm != "surrealdb-hnsw"
        || envelope.ann_configuration.distance_metric != "cosine"
        || envelope.ann_configuration.quantization != "f32"
        || envelope.ann_configuration.engine_version != STRICT_ANN_ENGINE_VERSION
        || envelope.ann_configuration.build_seed != 0
        || envelope.ann_configuration.build_order != STRICT_ANN_BUILD_ORDER
        || envelope.ann_configuration.tie_breaking != STRICT_ANN_TIE_BREAKING
        || envelope.ann_configuration.hnsw_m != STRICT_HNSW_M
        || envelope.ann_configuration.hnsw_ef_construction != STRICT_HNSW_EF_CONSTRUCTION
        || envelope.ann_configuration.hnsw_ef_search
            != strict_hnsw_ef_search(envelope.ann_configuration.candidate_k)
        || envelope.ann_configuration.candidate_k == 0
        || envelope.ann_configuration.candidate_k > 100_000
        || envelope.exact_rerank_configuration.rerank_k == 0
        || envelope.exact_rerank_configuration.rerank_k > envelope.ann_configuration.candidate_k
        || envelope.exact_rerank_configuration.distance_metric != "cosine"
        || envelope.exact_rerank_configuration.precision != "f32"
        || envelope.exact_rerank_configuration.tie_breaking != STRICT_RERANK_TIE_BREAKING
        || !bounded_scalar(&envelope.ann_configuration.engine_version)
        || !bounded_scalar(&envelope.ann_configuration.build_order)
        || !bounded_scalar(&envelope.ann_configuration.tie_breaking)
        || !bounded_scalar(&envelope.exact_rerank_configuration.tie_breaking)
        || !bounded_scalar(&envelope.model_generation)
        || !bounded_scalar(&envelope.calibration_generation)
    {
        push_failure(failures, "gallery_envelope_incomplete_or_drifted");
    }
    let mut looks_by_person: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut templates_by_look: HashMap<(&str, &str), usize> = HashMap::new();
    for member in members {
        looks_by_person
            .entry(&member.person_id)
            .or_default()
            .insert(&member.look_id);
        *templates_by_look
            .entry((&member.person_id, &member.look_id))
            .or_default() += 1;
    }
    if gallery_people.len() > envelope.people_max
        || members.len() > envelope.total_templates_max
        || looks_by_person
            .values()
            .any(|looks| looks.len() > envelope.looks_per_person_max)
        || templates_by_look
            .values()
            .any(|count| *count > envelope.trusted_templates_per_look_max)
    {
        push_failure(failures, "gallery_envelope_limit_exceeded");
    }

    if contract.run_identity.model_generation.as_deref() != Some(&envelope.model_generation)
        || contract.run_identity.calibration_generation.as_deref()
            != Some(&envelope.calibration_generation)
        || contract.run_identity.fixture_manifest_sha256.as_ref() != hashes.get("fixture-manifest")
    {
        push_failure(failures, "run_identity_envelope_mismatch");
    }

    if let Some(run_id) = contract.run_identity.activation_run_id.as_deref() {
        let current_people: HashSet<_> = probes
            .iter()
            .filter_map(|probe| fixtures.iter().find(|f| f.fixture_id == probe.fixture_id))
            .map(|fixture| fixture.ground_truth_person_id.as_str())
            .collect();
        let current_clusters: HashSet<_> = probes
            .iter()
            .filter_map(|probe| fixtures.iter().find(|f| f.fixture_id == probe.fixture_id))
            .map(|fixture| fixture.acquisition_cluster_id.as_str())
            .collect();
        for previous in spent {
            if !bounded_scalar(&previous.activation_run_id)
                || !bounded_scalar(&previous.observed_at)
                || !valid_sha256(&previous.fixture_manifest_sha256)
                || !unique_nonempty(previous.person_ids.iter().map(String::as_str))
                || !unique_nonempty(previous.acquisition_cluster_ids.iter().map(String::as_str))
            {
                push_failure(failures, "spent_set_invalid");
            }
            if previous.activation_run_id == run_id
                || previous.fixture_manifest_sha256
                    == contract
                        .run_identity
                        .fixture_manifest_sha256
                        .clone()
                        .unwrap_or_default()
                || previous
                    .person_ids
                    .iter()
                    .any(|id| current_people.contains(id.as_str()))
                || previous
                    .acquisition_cluster_ids
                    .iter()
                    .any(|id| current_clusters.contains(id.as_str()))
            {
                push_failure(failures, "spent_activation_evidence_reused");
            }
        }
    } else {
        push_failure(failures, "run_identity_incomplete");
    }
    Some(VerifiedManifestGraph {
        gallery_members_digest: semantic_gallery_members_digest(members),
        envelope,
    })
}

#[derive(Clone, Debug, Default)]
struct MetricReconstruction {
    metrics: Vec<CalibrationMetricVerdict>,
    effective_person_ids: BTreeSet<String>,
    effective_acquisition_cluster_ids: BTreeSet<String>,
}

#[derive(Clone, Copy)]
struct DerivedTrial<'a> {
    probe_id: &'a str,
    fixture: &'a CalibrationFixture,
    error: bool,
}

fn pipeline_configuration_digest(envelope: &GalleryEnvelope) -> String {
    sha256_hex(&serde_json::to_vec(envelope).expect("closed envelope serializes"))
}

fn provenance_matches(
    provenance: &FrozenPipelineProvenance,
    envelope: &GalleryEnvelope,
    envelope_hash: &str,
    pipeline_digest: &str,
) -> bool {
    provenance.model_generation == envelope.model_generation
        && provenance.calibration_generation == envelope.calibration_generation
        && provenance.gallery_envelope_sha256 == envelope_hash
        && provenance.pipeline_configuration_sha256 == pipeline_digest
}

fn reconstruct_metrics(
    raw: &RawMetricDocument,
    graph: &CalibrationEvidenceGraph,
    envelope_hash: &str,
    failures: &mut BTreeSet<String>,
) -> MetricReconstruction {
    if raw.schema_version != CALIBRATION_SCHEMA_VERSION
        || raw.records.len() > CALIBRATION_MAX_RECORDS
        || raw.pairwise_records.len() > CALIBRATION_MAX_RECORDS
        || raw.records.len().saturating_add(raw.pairwise_records.len()) > CALIBRATION_MAX_RECORDS
    {
        push_failure(failures, "raw_records_bound_or_schema_invalid");
    }
    let Some(envelope) = graph.gallery_envelope.as_ref() else {
        push_failure(failures, "manifest_graph_incomplete");
        return MetricReconstruction::default();
    };
    let pipeline_digest = pipeline_configuration_digest(envelope);
    let fixture_by_id: HashMap<_, _> = graph
        .fixtures
        .iter()
        .map(|fixture| (fixture.fixture_id.as_str(), fixture))
        .collect();
    let probe_by_id: HashMap<_, _> = graph
        .selected_probes
        .iter()
        .map(|probe| (probe.probe_id.as_str(), probe))
        .collect();
    let outcome_by_probe: HashMap<_, _> = raw
        .records
        .iter()
        .map(|outcome| (outcome.probe_id.as_str(), outcome))
        .collect();
    if outcome_by_probe.len() != raw.records.len()
        || outcome_by_probe.len() != graph.selected_probes.len()
        || graph
            .selected_probes
            .iter()
            .any(|probe| !outcome_by_probe.contains_key(probe.probe_id.as_str()))
    {
        push_failure(failures, "raw_probe_coverage_mismatch");
    }
    for outcome in &raw.records {
        let assigned_consistent = match outcome.emitted_assignment_state {
            EmittedAssignmentState::CommittedStrictAutomatic => outcome
                .assigned_person_id
                .as_deref()
                .is_some_and(bounded_scalar),
            EmittedAssignmentState::Unidentified | EmittedAssignmentState::Suggestion => {
                outcome.assigned_person_id.is_none()
            }
        };
        if outcome.terminal_outcome != "completed"
            || !bounded_scalar(&outcome.observed_at)
            || !assigned_consistent
            || !provenance_matches(
                &outcome.provenance,
                envelope,
                envelope_hash,
                &pipeline_digest,
            )
            || !probe_by_id.contains_key(outcome.probe_id.as_str())
        {
            push_failure(failures, "raw_probe_outcome_invalid");
        }
    }

    let pair_by_id: HashMap<_, _> = graph
        .selected_pairs
        .iter()
        .map(|pair| (pair.pair_id.as_str(), pair))
        .collect();
    let outcome_by_pair: HashMap<_, _> = raw
        .pairwise_records
        .iter()
        .map(|outcome| (outcome.pair_id.as_str(), outcome))
        .collect();
    if outcome_by_pair.len() != raw.pairwise_records.len()
        || outcome_by_pair.len() != graph.selected_pairs.len()
        || graph
            .selected_pairs
            .iter()
            .any(|pair| !outcome_by_pair.contains_key(pair.pair_id.as_str()))
    {
        push_failure(failures, "raw_pairwise_coverage_mismatch");
    }
    for outcome in &raw.pairwise_records {
        if outcome.terminal_outcome != "completed"
            || !bounded_scalar(&outcome.observed_at)
            || !outcome.similarity.is_finite()
            || !(-1.0..=1.0).contains(&outcome.similarity)
            || !provenance_matches(
                &outcome.provenance,
                envelope,
                envelope_hash,
                &pipeline_digest,
            )
            || pair_by_id.get(outcome.pair_id.as_str()).is_none_or(|pair| {
                pair.left_probe_id != outcome.left_probe_id
                    || pair.right_probe_id != outcome.right_probe_id
            })
        {
            push_failure(failures, "raw_pairwise_outcome_invalid");
        }
    }

    let slice_by_id: HashMap<_, _> = graph
        .hard_slices
        .iter()
        .map(|slice| (slice.slice_id.as_str(), slice))
        .collect();
    let mut grouped: BTreeMap<(String, String), Vec<DerivedTrial<'_>>> = BTreeMap::new();
    for probe in &graph.selected_probes {
        let (Some(fixture), Some(outcome)) = (
            fixture_by_id.get(probe.fixture_id.as_str()).copied(),
            outcome_by_probe.get(probe.probe_id.as_str()).copied(),
        ) else {
            continue;
        };
        let strict =
            outcome.emitted_assignment_state == EmittedAssignmentState::CommittedStrictAutomatic;
        let wrong_person = strict
            && outcome.assigned_person_id.as_deref()
                != Some(fixture.ground_truth_person_id.as_str());
        let mut scopes = vec!["aggregate"];
        scopes.extend(probe.slice_ids.iter().map(String::as_str));
        for scope in scopes {
            let excluded = scope != "aggregate"
                && slice_by_id
                    .get(scope)
                    .is_some_and(|slice| !slice.automatic_eligibility);
            if excluded {
                grouped
                    .entry((REQUIRED_GATES[3].into(), scope.into()))
                    .or_default()
                    .push(DerivedTrial {
                        probe_id: &probe.probe_id,
                        fixture,
                        error: !outcome.router_abstained,
                    });
                grouped
                    .entry((REQUIRED_GATES[4].into(), scope.into()))
                    .or_default()
                    .push(DerivedTrial {
                        probe_id: &probe.probe_id,
                        fixture,
                        error: strict,
                    });
                continue;
            }
            if strict {
                grouped
                    .entry((REQUIRED_GATES[0].into(), scope.into()))
                    .or_default()
                    .push(DerivedTrial {
                        probe_id: &probe.probe_id,
                        fixture,
                        error: probe.probe_kind == ProbeKind::NonMated || wrong_person,
                    });
            }
            match probe.probe_kind {
                ProbeKind::Mated if strict => {
                    grouped
                        .entry((REQUIRED_GATES[1].into(), scope.into()))
                        .or_default()
                        .push(DerivedTrial {
                            probe_id: &probe.probe_id,
                            fixture,
                            error: wrong_person,
                        });
                }
                ProbeKind::NonMated => {
                    grouped
                        .entry((REQUIRED_GATES[2].into(), scope.into()))
                        .or_default()
                        .push(DerivedTrial {
                            probe_id: &probe.probe_id,
                            fixture,
                            error: strict,
                        });
                }
                _ => {}
            }
        }
    }

    let mut expected_keys = BTreeSet::new();
    for metric in &REQUIRED_GATES[..3] {
        expected_keys.insert(((*metric).to_string(), "aggregate".to_string()));
    }
    expected_keys.insert((
        "pairwise-impostor-FMR-diagnostic".to_string(),
        "aggregate".to_string(),
    ));
    for slice in &graph.hard_slices {
        let required = if slice.automatic_eligibility {
            &REQUIRED_GATES[..3]
        } else {
            &REQUIRED_GATES[3..]
        };
        for metric in required {
            expected_keys.insert(((*metric).to_string(), slice.slice_id.clone()));
        }
    }

    let mut reconstruction = MetricReconstruction::default();
    for (metric_id, scope) in expected_keys {
        if metric_id == "pairwise-impostor-FMR-diagnostic" {
            continue;
        }
        let mut trials = grouped
            .remove(&(metric_id.clone(), scope.clone()))
            .unwrap_or_default();
        trials.sort_by(|left, right| left.probe_id.cmp(right.probe_id));
        let mut people = HashSet::new();
        let mut clusters = HashSet::new();
        let mut sources = HashSet::new();
        let mut families = HashSet::new();
        let mut lineages = HashSet::new();
        let mut accepted = Vec::new();
        for trial in trials {
            if people.contains(trial.fixture.ground_truth_person_id.as_str())
                || clusters.contains(trial.fixture.acquisition_cluster_id.as_str())
                || sources.contains(trial.fixture.source_asset_id.as_str())
                || families.contains(trial.fixture.duplicate_family_id.as_str())
                || lineages.contains(trial.fixture.lineage_root_id.as_str())
            {
                continue;
            }
            people.insert(trial.fixture.ground_truth_person_id.as_str());
            clusters.insert(trial.fixture.acquisition_cluster_id.as_str());
            sources.insert(trial.fixture.source_asset_id.as_str());
            families.insert(trial.fixture.duplicate_family_id.as_str());
            lineages.insert(trial.fixture.lineage_root_id.as_str());
            reconstruction
                .effective_person_ids
                .insert(trial.fixture.ground_truth_person_id.clone());
            reconstruction
                .effective_acquisition_cluster_ids
                .insert(trial.fixture.acquisition_cluster_id.clone());
            accepted.push(trial);
        }
        let denominator = accepted.len();
        let errors = accepted.iter().filter(|trial| trial.error).count();
        let aggregate = scope == "aggregate";
        let target = if aggregate {
            AGGREGATE_TARGET
        } else {
            SLICE_TARGET
        };
        let minimum = if aggregate {
            AGGREGATE_MIN_INDEPENDENT
        } else {
            SLICE_MIN_INDEPENDENT
        };
        let bound = clopper_pearson_upper_95(errors, denominator);
        let sufficient = denominator > 0
            && people.len() >= minimum
            && clusters.len() >= minimum
            && bound.is_some();
        let verdict = if sufficient && bound.is_some_and(|value| value <= target) {
            "pass"
        } else if sufficient {
            "fail"
        } else {
            "insufficient_evidence"
        };
        reconstruction.metrics.push(CalibrationMetricVerdict {
            metric_id,
            scope,
            denominator,
            errors,
            distinct_people: people.len(),
            distinct_acquisition_clusters: clusters.len(),
            point_estimate: if denominator == 0 {
                0.0
            } else {
                errors as f64 / denominator as f64
            },
            one_sided_95_ucb: bound,
            target,
            sufficient,
            verdict: verdict.to_string(),
        });
    }

    let mut pair_rows = graph
        .selected_pairs
        .iter()
        .filter_map(|pair| {
            outcome_by_pair
                .get(pair.pair_id.as_str())
                .map(|outcome| (pair, *outcome))
        })
        .collect::<Vec<_>>();
    pair_rows.sort_by(|left, right| left.0.pair_id.cmp(&right.0.pair_id));
    let mut pair_people = HashSet::new();
    let mut pair_clusters = HashSet::new();
    let mut pair_sources = HashSet::new();
    let mut pair_families = HashSet::new();
    let mut pair_lineages = HashSet::new();
    let mut pair_errors = 0usize;
    let mut pair_denominator = 0usize;
    for (pair, outcome) in pair_rows {
        let Some(left_probe) = probe_by_id.get(pair.left_probe_id.as_str()) else {
            continue;
        };
        let Some(right_probe) = probe_by_id.get(pair.right_probe_id.as_str()) else {
            continue;
        };
        let Some(left) = fixture_by_id.get(left_probe.fixture_id.as_str()).copied() else {
            continue;
        };
        let Some(right) = fixture_by_id.get(right_probe.fixture_id.as_str()).copied() else {
            continue;
        };
        let people = [
            left.ground_truth_person_id.as_str(),
            right.ground_truth_person_id.as_str(),
        ];
        let clusters = [
            left.acquisition_cluster_id.as_str(),
            right.acquisition_cluster_id.as_str(),
        ];
        let sources = [
            left.source_asset_id.as_str(),
            right.source_asset_id.as_str(),
        ];
        let families = [
            left.duplicate_family_id.as_str(),
            right.duplicate_family_id.as_str(),
        ];
        let lineages = [
            left.lineage_root_id.as_str(),
            right.lineage_root_id.as_str(),
        ];
        if people.iter().any(|id| pair_people.contains(id))
            || clusters.iter().any(|id| pair_clusters.contains(id))
            || sources.iter().any(|id| pair_sources.contains(id))
            || families.iter().any(|id| pair_families.contains(id))
            || lineages.iter().any(|id| pair_lineages.contains(id))
        {
            continue;
        }
        pair_people.extend(people);
        pair_clusters.extend(clusters);
        pair_sources.extend(sources);
        pair_families.extend(families);
        pair_lineages.extend(lineages);
        pair_denominator += 1;
        pair_errors += usize::from(outcome.accepted);
    }
    reconstruction.metrics.push(CalibrationMetricVerdict {
        metric_id: "pairwise-impostor-FMR-diagnostic".to_string(),
        scope: "aggregate".to_string(),
        denominator: pair_denominator,
        errors: pair_errors,
        distinct_people: pair_people.len(),
        distinct_acquisition_clusters: pair_clusters.len(),
        point_estimate: if pair_denominator == 0 {
            0.0
        } else {
            pair_errors as f64 / pair_denominator as f64
        },
        one_sided_95_ucb: clopper_pearson_upper_95(pair_errors, pair_denominator),
        target: AGGREGATE_TARGET,
        sufficient: pair_denominator > 0,
        verdict: "diagnostic".to_string(),
    });
    reconstruction.metrics.sort_by(|left, right| {
        left.metric_id
            .cmp(&right.metric_id)
            .then_with(|| left.scope.cmp(&right.scope))
    });
    reconstruction
}

fn metric_matches(recorded: &RecordedMetric, rebuilt: &CalibrationMetricVerdict) -> bool {
    recorded.metric_id == rebuilt.metric_id
        && recorded.scope == rebuilt.scope
        && recorded.denominator == rebuilt.denominator
        && recorded.errors == rebuilt.errors
        && recorded.distinct_people == rebuilt.distinct_people
        && recorded.distinct_acquisition_clusters == rebuilt.distinct_acquisition_clusters
        && (recorded.point_estimate - rebuilt.point_estimate).abs() <= 1e-12
        && match (recorded.one_sided_95_ucb, rebuilt.one_sided_95_ucb) {
            (Some(left), Some(right)) => (left - right).abs() <= 1e-12,
            (None, None) => true,
            _ => false,
        }
        && (recorded.target - rebuilt.target).abs() <= 1e-12
        && recorded.sufficient == rebuilt.sufficient
        && recorded.verdict == rebuilt.verdict
        && (recorded.sufficient || !recorded.insufficiency_reasons.is_empty())
}

#[derive(Serialize)]
struct ReviewEvidence<'a> {
    verifier_version: &'static str,
    contract_identity: &'static str,
    artifact_id: &'a str,
    artifact_kind: &'a str,
    schema_version: u32,
    workpacket_id: &'a str,
    status: &'a str,
    updated_at: &'a str,
    authority: &'a [String],
    evaluation_root: &'a serde_yaml::Value,
    planned_verifier: &'a serde_yaml::Value,
    frozen_protocol: &'a FrozenProtocol,
    run_identity: &'a CalibrationRunIdentity,
    declared_manifests: &'a [DeclaredCalibrationFile],
    manifest_record_requirements: &'a serde_yaml::Value,
    manifest_schemas: &'a serde_yaml::Value,
    raw_run_record_schema: &'a serde_yaml::Value,
    gating_metrics: &'a [String],
    diagnostic_metrics: &'a [String],
    metric_coverage: &'a serde_yaml::Value,
    required_metric_fields: &'a [String],
    activation_gate: &'a serde_yaml::Value,
    observed_manifest_hashes: &'a BTreeMap<String, String>,
    raw_records_sha256: &'a str,
    reconstructed_metrics: &'a [CalibrationMetricVerdict],
    envelope_hash: &'a str,
    gallery_composition_hash: &'a str,
    gallery_members_digest: &'a str,
    envelope: &'a GalleryEnvelope,
    reviewer: &'a str,
    reviewer_key_id: &'a str,
    approval_statement: &'a str,
    review_verdict: &'a str,
    review_digest_contract: &'a str,
}

fn canonical_review_evidence_digest(
    contract: &CalibrationContract,
    manifest_hashes: &BTreeMap<String, String>,
    raw_records_sha256: &str,
    metrics: &[CalibrationMetricVerdict],
    envelope_hash: &str,
    gallery_members_digest: &str,
    envelope: &GalleryEnvelope,
) -> String {
    let evidence = ReviewEvidence {
        verifier_version: CALIBRATION_VERIFIER_VERSION,
        contract_identity: CALIBRATION_CONTRACT_IDENTITY,
        artifact_id: &contract.artifact_id,
        artifact_kind: &contract.artifact_kind,
        schema_version: contract.schema_version,
        workpacket_id: &contract.workpacket_id,
        status: &contract.status,
        updated_at: &contract.updated_at,
        authority: &contract.authority,
        evaluation_root: &contract.evaluation_root,
        planned_verifier: &contract.planned_verifier,
        frozen_protocol: &contract.frozen_protocol,
        run_identity: &contract.run_identity,
        declared_manifests: &contract.required_manifests,
        manifest_record_requirements: &contract.manifest_record_requirements,
        manifest_schemas: &contract.manifest_schemas,
        raw_run_record_schema: &contract.raw_run_record_schema,
        gating_metrics: &contract.gating_metrics,
        diagnostic_metrics: &contract.diagnostic_metrics,
        metric_coverage: &contract.metric_coverage,
        required_metric_fields: &contract.required_metric_fields,
        activation_gate: &contract.activation_gate,
        observed_manifest_hashes: manifest_hashes,
        raw_records_sha256,
        reconstructed_metrics: metrics,
        envelope_hash,
        gallery_composition_hash: &envelope.manifest_sha256,
        gallery_members_digest,
        envelope,
        reviewer: contract
            .independent_review
            .reviewer
            .as_deref()
            .unwrap_or(""),
        reviewer_key_id: contract
            .independent_review
            .reviewer_key_id
            .as_deref()
            .unwrap_or(""),
        approval_statement: contract
            .independent_review
            .approval_statement
            .as_deref()
            .unwrap_or(""),
        review_verdict: &contract.independent_review.verdict,
        review_digest_contract: &contract.independent_review.digest_contract,
    };
    sha256_hex(&serde_json::to_vec(&evidence).expect("closed review evidence serializes"))
}

fn privacy_safe_id_hash(domain: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Independently validate the frozen Match activation artifact. Every returned
/// failure is fail-closed; the receipt never exposes the external root or rows.
pub fn verify_match_calibration(
    contract_path: &Path,
    evaluation_root: &Path,
) -> CalibrationVerification {
    let mut failures = BTreeSet::new();
    if !canonical_contract_location(contract_path) {
        push_failure(&mut failures, "noncanonical_contract_location");
    }
    let contract_bytes = match read_bounded_regular_file(contract_path, "contract") {
        Ok(bytes) => bytes,
        Err(code) => {
            push_failure(&mut failures, &code);
            return calibration_receipt(
                "unknown",
                failures,
                BTreeMap::new(),
                Vec::new(),
                None,
                None,
            );
        }
    };
    let contract: CalibrationContract = match serde_yaml::from_slice(&contract_bytes) {
        Ok(contract) => contract,
        Err(_) => {
            push_failure(&mut failures, "contract_parse_failed");
            return calibration_receipt(
                "unknown",
                failures,
                BTreeMap::new(),
                Vec::new(),
                None,
                None,
            );
        }
    };
    if serde_json::to_value(&contract)
        .ok()
        .is_none_or(|value| !all_string_scalars_bounded(&value))
    {
        push_failure(&mut failures, "contract_scalar_bound_exceeded");
    }
    if contract.schema_version != CALIBRATION_SCHEMA_VERSION
        || contract.artifact_id != CALIBRATION_ARTIFACT_ID
        || contract.frozen_protocol.confidence_method
            != "one-sided-95-percent-Clopper-Pearson-exact"
        || contract.frozen_protocol.aggregate_bound_max != AGGREGATE_TARGET
        || contract.frozen_protocol.hard_slice_bound_max != SLICE_TARGET
        || contract.frozen_protocol.minimum_aggregate_people != AGGREGATE_MIN_INDEPENDENT
        || contract
            .frozen_protocol
            .minimum_aggregate_acquisition_clusters
            != AGGREGATE_MIN_INDEPENDENT
        || contract.frozen_protocol.minimum_slice_people != SLICE_MIN_INDEPENDENT
        || contract.frozen_protocol.minimum_slice_acquisition_clusters != SLICE_MIN_INDEPENDENT
        || contract
            .run_identity
            .app_version
            .as_deref()
            .is_none_or(|value| !bounded_scalar(value))
        || contract
            .run_identity
            .git_commit
            .as_deref()
            .is_none_or(|value| !bounded_scalar(value))
        || contract
            .run_identity
            .cargo_lock_sha256
            .as_deref()
            .is_none_or(|value| !valid_sha256(value))
    {
        push_failure(&mut failures, "frozen_protocol_mismatch");
    }
    if contract.status != "verified" || contract.results.status != "complete" {
        push_failure(&mut failures, "calibration_results_pending");
    }
    let root = match fs::canonicalize(evaluation_root) {
        Ok(root) if root.is_dir() => root,
        _ => {
            push_failure(&mut failures, "evaluation_root_unavailable");
            return calibration_receipt(
                &contract.artifact_id,
                failures,
                BTreeMap::new(),
                Vec::new(),
                None,
                None,
            );
        }
    };
    let declared_ids: HashSet<_> = contract
        .required_manifests
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    let required_ids: HashSet<_> = REQUIRED_MANIFESTS.into_iter().collect();
    if declared_ids != required_ids || declared_ids.len() != contract.required_manifests.len() {
        push_failure(&mut failures, "required_manifest_set_mismatch");
    }
    let mut hashes = BTreeMap::new();
    let mut graph = CalibrationEvidenceGraph::default();
    for declared in &contract.required_manifests {
        let (Some(relative), Some(expected_hash)) = (
            declared.relative_path.as_deref(),
            declared.sha256.as_deref(),
        ) else {
            push_failure(&mut failures, "manifest_declaration_incomplete");
            continue;
        };
        if !valid_sha256(expected_hash) {
            push_failure(&mut failures, "manifest_hash_invalid");
            continue;
        }
        let bytes = match secure_calibration_file(&root, relative, "manifest") {
            Ok(bytes) => bytes,
            Err(code) => {
                push_failure(&mut failures, &code);
                continue;
            }
        };
        let observed = sha256_hex(&bytes);
        if !observed.eq_ignore_ascii_case(expected_hash) {
            push_failure(&mut failures, "manifest_hash_mismatch");
            continue;
        }
        if parse_calibration_manifest(&declared.id, &bytes, &mut graph).is_ok() {
            hashes.insert(declared.id.clone(), observed);
        } else {
            push_failure(&mut failures, "manifest_parse_failed");
        }
    }
    if serde_json::to_value(&graph)
        .ok()
        .is_none_or(|value| !all_string_scalars_bounded(&value))
    {
        push_failure(&mut failures, "manifest_scalar_bound_exceeded");
    }
    let verified_graph = verify_manifest_graph(&graph, &hashes, &contract, &root, &mut failures);

    let mut reconstruction = MetricReconstruction::default();
    let mut raw_records_hash = String::new();
    let mut raw_records_authenticated = false;
    let raw_declaration = contract
        .results
        .raw_run_records_relative_path
        .as_deref()
        .zip(contract.results.raw_run_records_sha256.as_deref());
    if let Some((relative, expected_hash)) = raw_declaration {
        if !valid_sha256(expected_hash) {
            push_failure(&mut failures, "raw_records_hash_invalid");
        } else if let Ok(bytes) = secure_calibration_file(&root, relative, "raw_records") {
            raw_records_hash = sha256_hex(&bytes);
            if raw_records_hash != expected_hash.to_ascii_lowercase() {
                push_failure(&mut failures, "raw_records_hash_mismatch");
            } else if let Ok(raw) = serde_yaml::from_slice::<RawMetricDocument>(&bytes) {
                if serde_json::to_value(&raw)
                    .ok()
                    .is_none_or(|value| !all_string_scalars_bounded(&value))
                {
                    push_failure(&mut failures, "raw_record_scalar_bound_exceeded");
                }
                let combined_records = manifest_record_count(&graph)
                    .and_then(|count| count.checked_add(raw.records.len()))
                    .and_then(|count| count.checked_add(raw.pairwise_records.len()));
                if combined_records.is_none_or(|count| count > CALIBRATION_MAX_RECORDS) {
                    push_failure(&mut failures, "combined_evidence_record_bound_exceeded");
                }
                raw_records_authenticated = true;
                let envelope_hash = hashes
                    .get("gallery-envelope-manifest")
                    .map(String::as_str)
                    .unwrap_or("");
                reconstruction = reconstruct_metrics(&raw, &graph, envelope_hash, &mut failures);
                let recorded_keys: HashSet<_> = contract
                    .results
                    .metrics
                    .iter()
                    .map(|metric| (metric.metric_id.as_str(), metric.scope.as_str()))
                    .collect();
                if contract.results.metrics.len() != reconstruction.metrics.len()
                    || recorded_keys.len() != contract.results.metrics.len()
                    || reconstruction.metrics.iter().any(|rebuilt| {
                        contract
                            .results
                            .metrics
                            .iter()
                            .find(|recorded| {
                                rebuilt.metric_id == recorded.metric_id
                                    && rebuilt.scope == recorded.scope
                            })
                            .map(|recorded| !metric_matches(recorded, rebuilt))
                            .unwrap_or(true)
                    })
                {
                    push_failure(&mut failures, "recorded_metric_mismatch");
                }
            } else {
                push_failure(&mut failures, "raw_records_parse_failed");
            }
        } else {
            push_failure(&mut failures, "raw_records_unavailable");
        }
    } else {
        push_failure(&mut failures, "raw_records_declaration_incomplete");
    }

    if verified_graph.is_none() {
        push_failure(&mut failures, "manifest_graph_incomplete");
    }
    if reconstruction.metrics.iter().any(|metric| {
        metric.metric_id != "pairwise-impostor-FMR-diagnostic" && metric.verdict != "pass"
    }) {
        push_failure(&mut failures, "metric_gate_not_passed");
    }
    let mut required_review_hashes: Vec<_> = hashes.values().map(String::as_str).collect();
    required_review_hashes.sort_unstable();
    let mut reviewed_hashes: Vec<_> = contract
        .independent_review
        .reviewed_manifest_hashes
        .iter()
        .map(String::as_str)
        .collect();
    reviewed_hashes.sort_unstable();
    let metric_ids: HashSet<_> = reconstruction
        .metrics
        .iter()
        .map(|metric| metric.metric_id.as_str())
        .collect();
    let reviewed_metric_ids: HashSet<_> = contract
        .independent_review
        .reviewed_metric_ids
        .iter()
        .map(String::as_str)
        .collect();
    let review_digest = verified_graph.as_ref().and_then(|verified| {
        let envelope_hash = hashes.get("gallery-envelope-manifest")?;
        Some(canonical_review_evidence_digest(
            &contract,
            &hashes,
            &raw_records_hash,
            &reconstruction.metrics,
            envelope_hash,
            &verified.gallery_members_digest,
            &verified.envelope,
        ))
    });
    let review_authentic = contract
        .independent_review
        .reviewer
        .as_deref()
        .is_some_and(bounded_scalar)
        && contract.independent_review.approval_statement.as_deref()
            == Some(CALIBRATION_APPROVAL_STATEMENT)
        && reviewed_hashes == required_review_hashes
        && reviewed_metric_ids == metric_ids
        && contract.independent_review.evidence_digest.as_deref() == review_digest.as_deref()
        && review_digest.as_deref().is_some_and(|digest| {
            independent_review_signature_valid(&contract.independent_review, digest)
        });
    if !review_authentic || contract.independent_review.verdict != "pass" {
        push_failure(&mut failures, "independent_review_incomplete");
    }
    if contract.results.verifier_verdict != "pass" {
        push_failure(&mut failures, "recorded_verifier_verdict_not_pass");
    }
    let fixture_by_id: HashMap<_, _> = graph
        .fixtures
        .iter()
        .map(|fixture| (fixture.fixture_id.as_str(), fixture))
        .collect();
    let observed_people = graph
        .selected_probes
        .iter()
        .filter_map(|probe| fixture_by_id.get(probe.fixture_id.as_str()))
        .map(|fixture| {
            privacy_safe_id_hash(
                "facial-match-observed-person-v1",
                &fixture.ground_truth_person_id,
            )
        })
        .collect::<BTreeSet<_>>();
    let observed_acquisitions = graph
        .selected_probes
        .iter()
        .filter_map(|probe| fixture_by_id.get(probe.fixture_id.as_str()))
        .map(|fixture| {
            privacy_safe_id_hash(
                "facial-match-observed-acquisition-v1",
                &fixture.acquisition_cluster_id,
            )
        })
        .collect::<BTreeSet<_>>();
    let observed_trial_claim = if review_authentic && raw_records_authenticated {
        contract
            .run_identity
            .activation_run_id
            .clone()
            .zip(hashes.get("fixture-manifest").cloned())
            .zip(review_digest.clone())
            .map(
                |((activation_run_id, fixture_manifest_sha256), evidence_digest)| {
                    ObservedCalibrationClaim {
                        activation_run_id,
                        fixture_manifest_sha256,
                        evidence_digest,
                        observed_person_id_hashes: observed_people.iter().cloned().collect(),
                        observed_acquisition_cluster_id_hashes: observed_acquisitions
                            .iter()
                            .cloned()
                            .collect(),
                    }
                },
            )
    } else {
        None
    };
    let claim = if failures.is_empty() {
        let verified = verified_graph
            .as_ref()
            .expect("zero failures require verified graph");
        let envelope_hash = hashes
            .get("gallery-envelope-manifest")
            .expect("zero failures require envelope hash")
            .clone();
        let mut spent_people = graph
            .spent_sets
            .iter()
            .flat_map(|set| set.person_ids.iter())
            .map(|id| privacy_safe_id_hash("facial-match-spent-person-v1", id))
            .collect::<BTreeSet<_>>();
        let mut spent_clusters = graph
            .spent_sets
            .iter()
            .flat_map(|set| set.acquisition_cluster_ids.iter())
            .map(|id| privacy_safe_id_hash("facial-match-spent-acquisition-v1", id))
            .collect::<BTreeSet<_>>();
        // Preserve deterministic order even when an upstream registry repeats
        // a prior spent ID (which is already separately rejected).
        let spent_person_id_hashes = spent_people.iter().cloned().collect();
        let spent_acquisition_cluster_id_hashes = spent_clusters.iter().cloned().collect();
        spent_people.clear();
        spent_clusters.clear();
        Some(VerifiedCalibrationClaim {
            activation_run_id: contract.run_identity.activation_run_id.clone().unwrap(),
            calibration_generation: verified.envelope.calibration_generation.clone(),
            model_generation: verified.envelope.model_generation.clone(),
            envelope_hash,
            runtime_configuration_digest: strict_runtime_configuration_digest(
                verified.envelope.ann_configuration.candidate_k,
                verified.envelope.exact_rerank_configuration.rerank_k,
            ),
            gallery_composition_hash: verified.envelope.manifest_sha256.clone(),
            gallery_members_digest: verified.gallery_members_digest.clone(),
            artifact_id: contract.artifact_id.clone(),
            contract_sha256: sha256_hex(&contract_bytes),
            raw_records_sha256: raw_records_hash.clone(),
            review_digest: review_digest.clone().unwrap(),
            evidence_digest: review_digest.clone().unwrap(),
            fixture_manifest_sha256: hashes.get("fixture-manifest").unwrap().clone(),
            observed_person_id_hashes: observed_people.into_iter().collect(),
            observed_acquisition_cluster_id_hashes: observed_acquisitions.into_iter().collect(),
            spent_person_id_hashes,
            spent_acquisition_cluster_id_hashes,
            automatic_threshold: verified.envelope.thresholds.automatic_commit,
            suggestion_threshold: verified.envelope.thresholds.suggestion,
            runner_up_margin: verified.envelope.margins.runner_up_minimum,
            minimum_quality: verified.envelope.quality_gates.minimum_quality,
            candidate_k: verified.envelope.ann_configuration.candidate_k,
            rerank_k: verified.envelope.exact_rerank_configuration.rerank_k,
            people_max: verified.envelope.people_max,
            looks_per_person_max: verified.envelope.looks_per_person_max,
            trusted_templates_per_look_max: verified.envelope.trusted_templates_per_look_max,
            total_templates_max: verified.envelope.total_templates_max,
        })
    } else {
        None
    };
    calibration_receipt(
        &contract.artifact_id,
        failures,
        hashes,
        reconstruction.metrics,
        claim,
        observed_trial_claim,
    )
}

fn calibration_receipt(
    artifact_id: &str,
    failures: BTreeSet<String>,
    hashes: BTreeMap<String, String>,
    metrics: Vec<CalibrationMetricVerdict>,
    verified_claim: Option<VerifiedCalibrationClaim>,
    observed_trial_claim: Option<ObservedCalibrationClaim>,
) -> CalibrationVerification {
    let enabled = failures.is_empty();
    CalibrationVerification {
        artifact_id: artifact_id.to_string(),
        verdict: if enabled { "pass" } else { "fail_closed" }.to_string(),
        strict_automatic_enabled: enabled,
        failure_codes: failures.into_iter().collect(),
        verified_manifest_hashes: hashes,
        reconstructed_metrics: metrics,
        privacy: CalibrationPrivacy {
            evaluation_root_redacted: true,
            fixture_paths_emitted: false,
            raw_records_emitted: false,
        },
        verified_claim,
        observed_trial_claim,
    }
}

#[derive(Clone, Debug)]
pub struct TrustedTemplate {
    pub observation_id: String,
    pub look_id: String,
    pub vector: IdentityVector,
    pub trusted_authorized: bool,
    pub operator_confirmed: bool,
    pub quality: f32,
}

#[derive(Clone, Debug)]
pub struct PersonReferences {
    pub person_id: String,
    pub templates: Vec<TrustedTemplate>,
}

#[derive(Clone, Debug)]
pub struct StrictRecognitionPolicy {
    pub model_generation: String,
    pub calibration_generation: String,
    pub envelope_hash: String,
    pub strict_automatic_enabled: bool,
    pub automatic_threshold: f32,
    pub suggestion_threshold: f32,
    pub runner_up_margin: f32,
    pub minimum_quality: f32,
    pub people_max: usize,
    pub looks_per_person_max: usize,
    pub templates_per_look_max: usize,
    pub total_templates_max: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentState {
    Unidentified,
    Suggestion,
    CommittedStrictAutomatic,
    OperatorConfirmed,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecognitionDecision {
    pub state: AssignmentState,
    pub person_id: Option<String>,
    pub winning_look_id: Option<String>,
    pub winning_observation_id: Option<String>,
    pub similarity: Option<f32>,
    pub runner_up_margin: Option<f32>,
    pub model_generation: String,
    pub calibration_generation: String,
    pub reasons: Vec<String>,
}

/// Score bounded, explicitly trusted per-Look templates. No Person centroid is
/// computed. Strict activation is tied to the frozen model/calibration/envelope.
pub fn recognize_strict(
    query: &IdentityVector,
    quality: f32,
    people: &[PersonReferences],
    cannot_links: &HashSet<String>,
    policy: &StrictRecognitionPolicy,
    observed_envelope_hash: &str,
) -> IdentityResult<RecognitionDecision> {
    let mut reasons = Vec::new();
    let mut scored: Vec<(f32, &str, &str, &str)> = Vec::new();
    let mut total_templates = 0usize;
    let mut envelope_ok = people.len() <= policy.people_max;
    for person in people {
        let mut looks: HashMap<&str, usize> = HashMap::new();
        for template in &person.templates {
            if !template.trusted_authorized
                || !template.operator_confirmed
                || template.quality < policy.minimum_quality
                || template.vector.generation() != policy.model_generation
            {
                continue;
            }
            *looks.entry(&template.look_id).or_default() += 1;
            total_templates += 1;
            if cannot_links.contains(&person.person_id) {
                continue;
            }
            scored.push((
                cosine_checked(query, &template.vector)?,
                &person.person_id,
                &template.look_id,
                &template.observation_id,
            ));
        }
        envelope_ok &= looks.len() <= policy.looks_per_person_max
            && looks
                .values()
                .all(|count| *count <= policy.templates_per_look_max);
    }
    envelope_ok &= total_templates <= policy.total_templates_max
        && observed_envelope_hash == policy.envelope_hash
        && query.generation() == policy.model_generation;
    scored.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(right.1))
            .then_with(|| left.2.cmp(right.2))
            .then_with(|| left.3.cmp(right.3))
    });
    let Some(best) = scored.first().copied() else {
        reasons.push("no_eligible_trusted_template".to_string());
        return Ok(RecognitionDecision {
            state: AssignmentState::Unidentified,
            person_id: None,
            winning_look_id: None,
            winning_observation_id: None,
            similarity: None,
            runner_up_margin: None,
            model_generation: policy.model_generation.clone(),
            calibration_generation: policy.calibration_generation.clone(),
            reasons,
        });
    };
    let runner_up = scored
        .iter()
        .find(|entry| entry.1 != best.1)
        .map(|entry| entry.0);
    let margin = runner_up
        .map(|score| best.0 - score)
        .unwrap_or(f32::INFINITY);
    let state = if quality < policy.minimum_quality || best.0 < policy.suggestion_threshold {
        reasons.push("quality_or_similarity_below_suggestion_gate".to_string());
        AssignmentState::Unidentified
    } else if policy.strict_automatic_enabled
        && envelope_ok
        && best.0 >= policy.automatic_threshold
        && margin >= policy.runner_up_margin
    {
        AssignmentState::CommittedStrictAutomatic
    } else {
        if !envelope_ok {
            reasons.push("frozen_envelope_or_generation_mismatch".to_string());
        }
        if margin < policy.runner_up_margin {
            reasons.push("runner_up_margin_too_small".to_string());
        }
        AssignmentState::Suggestion
    };
    Ok(RecognitionDecision {
        person_id: (state != AssignmentState::Unidentified).then(|| best.1.to_string()),
        winning_look_id: Some(best.2.to_string()),
        winning_observation_id: Some(best.3.to_string()),
        similarity: Some(best.0),
        runner_up_margin: Some(margin),
        state,
        model_generation: policy.model_generation.clone(),
        calibration_generation: policy.calibration_generation.clone(),
        reasons,
    })
}

#[derive(Clone, Debug)]
pub struct ClusterCandidate {
    pub observation_id: String,
    pub duplicate_family_id: String,
    pub quality: f32,
    pub operator_confirmed: bool,
    pub vector: IdentityVector,
}

/// Conservative unnamed clustering. A duplicate family contributes one density
/// vote, and a cannot-link between any two members blocks an indirect merge.
pub fn cluster_unnamed_conservative(
    candidates: &[ClusterCandidate],
    similarity_threshold: f32,
    minimum_quality: f32,
    minimum_independent_families: usize,
    cannot_links: &HashSet<(String, String)>,
) -> IdentityResult<Vec<Option<usize>>> {
    let blocked = |left: &str, right: &str| {
        cannot_links.contains(&(left.to_string(), right.to_string()))
            || cannot_links.contains(&(right.to_string(), left.to_string()))
    };
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut assignment = vec![None; candidates.len()];
    let mut stable_order: Vec<_> = (0..candidates.len()).collect();
    stable_order.sort_by(|left, right| {
        candidates[*left]
            .observation_id
            .cmp(&candidates[*right].observation_id)
    });
    for index in stable_order {
        let candidate = &candidates[index];
        if candidate.quality < minimum_quality || candidate.operator_confirmed {
            continue;
        }
        let mut selected = None;
        for (cluster_id, members) in clusters.iter().enumerate() {
            if members.iter().any(|member| {
                blocked(
                    &candidate.observation_id,
                    &candidates[*member].observation_id,
                )
            }) {
                continue;
            }
            let mut seen_families = HashSet::new();
            let representatives: Vec<_> = members
                .iter()
                .copied()
                .filter(|member| {
                    seen_families.insert(candidates[*member].duplicate_family_id.as_str())
                })
                .collect();
            let mut matches_every_family = true;
            for member in representatives {
                if cosine_checked(&candidate.vector, &candidates[member].vector)?
                    < similarity_threshold
                {
                    matches_every_family = false;
                    break;
                }
            }
            if matches_every_family {
                selected = Some(cluster_id);
                break;
            }
        }
        let cluster_id = selected.unwrap_or_else(|| {
            clusters.push(Vec::new());
            clusters.len() - 1
        });
        clusters[cluster_id].push(index);
        assignment[index] = Some(cluster_id);
    }
    for (cluster_id, members) in clusters.iter().enumerate() {
        let support: HashSet<_> = members
            .iter()
            .map(|member| candidates[*member].duplicate_family_id.as_str())
            .collect();
        if support.len() < minimum_independent_families {
            for member in members {
                assignment[*member] = None;
            }
        } else {
            for member in members {
                assignment[*member] = Some(cluster_id);
            }
        }
    }
    Ok(assignment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    #[test]
    fn wp086_persisted_vectors_cannot_launder_nonunit_or_invalid_evidence() {
        let mut values = vec![0.0; EMBEDDING_DIM];
        values[0] = 1.0;
        assert!(IdentityVector::from_persisted(values.clone(), "current-generation").is_ok());
        values[0] = 2.0;
        assert!(IdentityVector::from_persisted(values.clone(), "current-generation").is_err());
        values[0] = f32::NAN;
        assert!(IdentityVector::from_persisted(values, "current-generation").is_err());
        assert!(
            IdentityVector::from_persisted(vec![0.0; EMBEDDING_DIM], "current-generation").is_err()
        );
        assert!(IdentityVector::from_persisted(vec![1.0], "current-generation").is_err());
        let mut values = vec![0.0; EMBEDDING_DIM];
        values[0] = 1.0;
        assert!(IdentityVector::from_persisted(values, "invalid\ngeneration").is_err());
    }

    fn unit_vector(x: f32, y: f32, generation: &str) -> IdentityVector {
        let mut values = vec![0.0; EMBEDDING_DIM];
        values[0] = x;
        values[1] = y;
        normalize_embedding(&mut values).unwrap();
        IdentityVector::new(values, generation)
    }

    fn trusted(observation: &str, look: &str, vector: IdentityVector) -> TrustedTemplate {
        TrustedTemplate {
            observation_id: observation.to_string(),
            look_id: look.to_string(),
            vector,
            trusted_authorized: true,
            operator_confirmed: true,
            quality: 0.95,
        }
    }

    #[test]
    fn clopper_pearson_exact_boundaries_gate_wp082_targets() {
        let aggregate = clopper_pearson_upper_95(0, 10_000).unwrap();
        let slice = clopper_pearson_upper_95(0, 3_000).unwrap();
        assert!((aggregate - 0.0002995283597766).abs() < 1e-12);
        assert!((slice - 0.0009980790119970).abs() < 1e-12);
        assert!(aggregate <= AGGREGATE_TARGET);
        assert!(slice <= SLICE_TARGET);
        assert_eq!(clopper_pearson_upper_95(1, 1), Some(1.0));
        assert_eq!(clopper_pearson_upper_95(0, 0), None);
        assert_eq!(clopper_pearson_upper_95(2, 1), None);
    }

    #[test]
    fn canonical_pending_contract_fails_closed_without_leaking_root() {
        let contract = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("governance")
            .join("validation")
            .join("wp-082-match-calibration-v1.yaml");
        let root =
            std::env::temp_dir().join(format!("facial-wp082-pending-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let receipt = verify_match_calibration(&contract, &root);
        assert_eq!(receipt.verdict, "fail_closed");
        assert!(!receipt.strict_automatic_enabled);
        assert!(receipt
            .failure_codes
            .contains(&"calibration_results_pending".to_string()));
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert!(!serialized.contains(root.to_string_lossy().as_ref()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copied_contract_shape_is_not_a_trusted_contract_location() {
        let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("governance")
            .join("validation")
            .join("wp-082-match-calibration-v1.yaml");
        assert!(canonical_contract_location(&canonical));

        let copied_root = std::env::temp_dir().join(format!(
            "facial-wp082-copied-contract-{}",
            uuid::Uuid::new_v4()
        ));
        let copied = copied_root
            .join("governance")
            .join("validation")
            .join("wp-082-match-calibration-v1.yaml");
        fs::create_dir_all(copied.parent().unwrap()).unwrap();
        fs::copy(&canonical, &copied).unwrap();
        fs::write(copied_root.join("CODEX.md"), "copied").unwrap();
        fs::write(copied_root.join("topology.yaml"), "copied: true").unwrap();
        fs::create_dir_all(copied_root.join("product")).unwrap();
        fs::write(copied_root.join("product").join("Cargo.toml"), "[package]").unwrap();
        assert!(!canonical_contract_location(&copied));
        fs::remove_dir_all(copied_root).unwrap();
    }

    #[test]
    fn calibration_contract_rejects_unknown_fields_at_every_typed_object_depth() {
        fn canonical_value() -> serde_yaml::Value {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("governance")
                .join("validation")
                .join("wp-082-match-calibration-v1.yaml");
            serde_yaml::from_slice(&fs::read(path).unwrap()).unwrap()
        }
        fn key<'a>(value: &'a mut serde_yaml::Value, name: &str) -> &'a mut serde_yaml::Value {
            value
                .as_mapping_mut()
                .unwrap()
                .get_mut(&serde_yaml::Value::String(name.to_string()))
                .unwrap()
        }
        fn add_unknown(value: &mut serde_yaml::Value) {
            value.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String("unexpected_semantic_override".to_string()),
                serde_yaml::Value::String("must-not-be-discarded".to_string()),
            );
        }
        fn rejected(value: serde_yaml::Value) {
            assert!(serde_yaml::from_value::<CalibrationContract>(value).is_err());
        }

        let canonical = canonical_value();
        serde_yaml::from_value::<CalibrationContract>(canonical.clone()).unwrap();

        let mut root = canonical.clone();
        add_unknown(&mut root);
        rejected(root);

        let mut manifest = canonical.clone();
        add_unknown(
            key(&mut manifest, "required_manifests")
                .as_sequence_mut()
                .unwrap()
                .first_mut()
                .unwrap(),
        );
        rejected(manifest);

        for object in [
            "frozen_protocol",
            "run_identity",
            "results",
            "independent_review",
        ] {
            let mut value = canonical.clone();
            add_unknown(key(&mut value, object));
            rejected(value);
        }

        let mut metric = serde_yaml::to_value(RecordedMetric {
            metric_id: REQUIRED_GATES[0].to_string(),
            scope: "aggregate".to_string(),
            denominator: 10_000,
            errors: 0,
            distinct_people: 10_000,
            distinct_acquisition_clusters: 10_000,
            point_estimate: 0.0,
            one_sided_95_ucb: Some(0.000_299_528_359_776_6),
            target: AGGREGATE_TARGET,
            sufficient: true,
            insufficiency_reasons: Vec::new(),
            verdict: "pass".to_string(),
        })
        .unwrap();
        add_unknown(&mut metric);
        let mut recorded_metric = canonical;
        key(key(&mut recorded_metric, "results"), "metrics")
            .as_sequence_mut()
            .unwrap()
            .push(metric);
        rejected(recorded_metric);
    }

    #[test]
    fn recursive_scalar_and_combined_record_bounds_are_enforceable() {
        assert!(all_string_scalars_bounded(&serde_json::json!({
            "nested": ["ok", {"value": "x"}]
        })));
        assert!(!all_string_scalars_bounded(&serde_json::json!({
            "nested": "x".repeat(CALIBRATION_MAX_SCALAR_BYTES + 1)
        })));
        let graph = CalibrationEvidenceGraph::default();
        assert_eq!(manifest_record_count(&graph), Some(0));
    }

    #[test]
    fn calibration_paths_reject_traversal_and_hashes_are_exact() {
        let root = std::env::temp_dir().join(format!("facial-wp082-path-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert_eq!(
            secure_calibration_file(&root, "../escape.yaml", "manifest").unwrap_err(),
            "manifest_unsafe_path"
        );
        assert!(!valid_sha256(&"A".repeat(63)));
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(valid_sha256(&"a".repeat(64)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ed25519_review_verification_matches_rfc8032_and_rejects_tampering() {
        let public_key = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
        let signature = concat!(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da",
            "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
        );
        assert!(ed25519_signature_valid(public_key, signature, b"r"));
        assert!(!ed25519_signature_valid(public_key, signature, b"tampered"));
        assert!(!ed25519_signature_valid(&"0".repeat(64), signature, b"r"));
    }

    #[test]
    fn hard_slice_membership_is_derived_from_typed_ground_truth() {
        let base = |fixture_id: &str, person_id: &str| CalibrationFixture {
            fixture_id: fixture_id.into(),
            media_relative_path: format!("media/{fixture_id}.png"),
            media_sha256: "a".repeat(64),
            source_asset_id: format!("source-{fixture_id}"),
            ground_truth_person_id: person_id.into(),
            acquisition_cluster_id: format!("acq-{fixture_id}"),
            lineage_root_id: format!("lineage-{fixture_id}"),
            duplicate_family_id: "duplicate-family".into(),
            partition_id: "test".into(),
            capture_session_id: format!("session-{fixture_id}"),
            burst_id: String::new(),
            video_track_id: String::new(),
            hard_slice_ids: Vec::new(),
            lookalike_rival_group_id: Some("rival-group".into()),
            slice_evidence: HardSliceGroundTruth {
                annotation_provenance: "frozen-human-ground-truth-v1".into(),
                ..HardSliceGroundTruth::default()
            },
        };
        let mut probe = base("probe", "person-a");
        probe.slice_evidence = HardSliceGroundTruth {
            look_id: "look-new".into(),
            capture_day: Some(500),
            yaw_degrees: Some(31.0),
            pitch_degrees: Some(0.0),
            major_styling_or_makeup_change: true,
            wig_or_major_hair_change: true,
            glasses: Some(true),
            face_occlusion_fraction: Some(0.25),
            mask_or_partial_occlusion: false,
            exposure_label: "underexposed".into(),
            face_box_width: Some(63),
            face_box_height: Some(100),
            blur_label: "poor".into(),
            lossy_reencode: true,
            source_short_edge_before_upscale: Some(128),
            source_kind: "collage".into(),
            ground_truth_face_count: Some(4),
            origin_kind: "ai_rendered".into(),
            demographic_cohort_ids: vec!["cohort-a".into()],
            annotation_provenance: "frozen-human-ground-truth-v1".into(),
        };
        let mut reference = base("reference", "person-a");
        reference.slice_evidence.look_id = "look-old".into();
        reference.slice_evidence.capture_day = Some(0);
        reference.slice_evidence.glasses = Some(false);
        let rival = base("rival", "person-b");
        let fixtures = [&probe, &reference, &rival]
            .into_iter()
            .map(|fixture| (fixture.fixture_id.as_str(), fixture))
            .collect::<HashMap<_, _>>();
        let members = vec![
            GalleryMember {
                person_id: "person-a".into(),
                look_id: "look-old".into(),
                trusted_template_set_id: "set-a".into(),
                observation_id: "reference".into(),
                fixture_id: "reference".into(),
            },
            GalleryMember {
                person_id: "person-b".into(),
                look_id: "look-rival".into(),
                trusted_template_set_id: "set-b".into(),
                observation_id: "rival".into(),
                fixture_id: "rival".into(),
            },
        ];
        let duplicate_sizes = HashMap::from([("duplicate-family", 2_usize)]);
        let derived =
            derived_hard_slice_ids(&probe, &members, &fixtures, &duplicate_sizes).unwrap();
        for required in REQUIRED_HARD_SLICES {
            if required != "screenshot" {
                assert!(
                    derived.contains(required),
                    "missing derived slice {required}"
                );
            }
        }
        assert!(derived.contains("demographic_cohort:cohort-a"));

        drop(fixtures);
        probe.slice_evidence.source_kind = "screenshot".into();
        let screenshot_fixtures = [&probe, &reference, &rival]
            .into_iter()
            .map(|fixture| (fixture.fixture_id.as_str(), fixture))
            .collect::<HashMap<_, _>>();
        assert!(
            derived_hard_slice_ids(&probe, &members, &screenshot_fixtures, &duplicate_sizes,)
                .unwrap()
                .contains("screenshot")
        );

        drop(screenshot_fixtures);
        probe.slice_evidence.yaw_degrees = Some(f64::NAN);
        let invalid_fixtures = [&probe, &reference, &rival]
            .into_iter()
            .map(|fixture| (fixture.fixture_id.as_str(), fixture))
            .collect::<HashMap<_, _>>();
        assert!(
            derived_hard_slice_ids(&probe, &members, &invalid_fixtures, &duplicate_sizes).is_err()
        );
    }

    fn calibration_test_envelope() -> GalleryEnvelope {
        GalleryEnvelope {
            manifest_sha256: "c".repeat(64),
            people_max: 50_000,
            looks_per_person_max: 8,
            trusted_templates_per_look_max: 16,
            total_templates_max: 100_000,
            selection_policy: STRICT_SELECTION_POLICY.into(),
            thresholds: CalibrationThresholds {
                automatic_commit: 0.9,
                suggestion: 0.8,
                unnamed_cluster: 0.75,
            },
            ann_configuration: CalibrationAnnConfiguration {
                index_type: "hnsw".into(),
                engine_version: "surrealdb-3.2.4".into(),
                algorithm: "surrealdb-hnsw".into(),
                distance_metric: "cosine".into(),
                quantization: "f32".into(),
                build_seed: 0,
                build_order: STRICT_ANN_BUILD_ORDER.into(),
                tie_breaking: STRICT_ANN_TIE_BREAKING.into(),
                hnsw_m: STRICT_HNSW_M,
                hnsw_ef_construction: STRICT_HNSW_EF_CONSTRUCTION,
                hnsw_ef_search: strict_hnsw_ef_search(100),
                candidate_k: 100,
            },
            exact_rerank_configuration: CalibrationRerankConfiguration {
                rerank_k: 50,
                distance_metric: "cosine".into(),
                precision: "f32".into(),
                tie_breaking: STRICT_RERANK_TIE_BREAKING.into(),
            },
            aggregation: "best-template-per-look-then-best-look-per-person".into(),
            margins: CalibrationMargins {
                runner_up_minimum: 0.1,
            },
            quality_gates: CalibrationQualityGates {
                minimum_quality: 0.7,
                alignment_required: true,
                pose_gate_required: true,
            },
            cannot_link_gates: CalibrationCannotLinkGates {
                face_to_person_exclusion_required: true,
            },
            model_generation: "model-1".into(),
            calibration_generation: "cal-1".into(),
        }
    }

    fn frozen_metric_fixture() -> (CalibrationEvidenceGraph, RawMetricDocument, String) {
        let count = AGGREGATE_MIN_INDEPENDENT * 2;
        let automatic = "automatic-slice".to_string();
        let fixtures = (0..count)
            .map(|index| CalibrationFixture {
                fixture_id: format!("fixture-{index}"),
                media_relative_path: format!("media/{index}.png"),
                media_sha256: "a".repeat(64),
                source_asset_id: format!("source-{index}"),
                ground_truth_person_id: format!("person-{index}"),
                acquisition_cluster_id: format!("acquisition-{index}"),
                lineage_root_id: format!("lineage-{index}"),
                duplicate_family_id: format!("family-{index}"),
                partition_id: "test".into(),
                capture_session_id: format!("session-{index}"),
                burst_id: format!("burst-{index}"),
                video_track_id: format!("track-{index}"),
                hard_slice_ids: if index < SLICE_MIN_INDEPENDENT
                    || (AGGREGATE_MIN_INDEPENDENT
                        ..AGGREGATE_MIN_INDEPENDENT + SLICE_MIN_INDEPENDENT)
                        .contains(&index)
                {
                    vec![automatic.clone()]
                } else {
                    Vec::new()
                },
                lookalike_rival_group_id: None,
                slice_evidence: HardSliceGroundTruth::default(),
            })
            .collect::<Vec<_>>();
        let selected_probes = fixtures
            .iter()
            .enumerate()
            .map(|(index, fixture)| SelectedProbe {
                probe_id: format!("probe-{index}"),
                fixture_id: fixture.fixture_id.clone(),
                probe_kind: if index < AGGREGATE_MIN_INDEPENDENT {
                    ProbeKind::Mated
                } else {
                    ProbeKind::NonMated
                },
                slice_ids: fixture.hard_slice_ids.clone(),
                predeclared_before_run: true,
            })
            .collect::<Vec<_>>();
        let selected_pairs = (0..AGGREGATE_MIN_INDEPENDENT)
            .map(|index| SelectedPair {
                pair_id: format!("pair-{index}"),
                left_probe_id: format!("probe-{index}"),
                right_probe_id: format!("probe-{}", index + AGGREGATE_MIN_INDEPENDENT),
                predeclared_before_run: true,
            })
            .collect::<Vec<_>>();
        let envelope = calibration_test_envelope();
        let envelope_hash = "e".repeat(64);
        let provenance = || FrozenPipelineProvenance {
            model_generation: envelope.model_generation.clone(),
            calibration_generation: envelope.calibration_generation.clone(),
            gallery_envelope_sha256: envelope_hash.clone(),
            pipeline_configuration_sha256: pipeline_configuration_digest(&envelope),
        };
        let records = selected_probes
            .iter()
            .map(|probe| RawProbeOutcome {
                probe_id: probe.probe_id.clone(),
                terminal_outcome: "completed".into(),
                emitted_assignment_state: if probe.probe_kind == ProbeKind::Mated {
                    EmittedAssignmentState::CommittedStrictAutomatic
                } else {
                    EmittedAssignmentState::Unidentified
                },
                assigned_person_id: (probe.probe_kind == ProbeKind::Mated).then(|| {
                    fixtures
                        .iter()
                        .find(|fixture| fixture.fixture_id == probe.fixture_id)
                        .unwrap()
                        .ground_truth_person_id
                        .clone()
                }),
                router_abstained: true,
                observed_at: "2026-08-23T00:00:00Z".into(),
                provenance: provenance(),
            })
            .collect();
        let pairwise_records = selected_pairs
            .iter()
            .map(|pair| RawPairwiseOutcome {
                pair_id: pair.pair_id.clone(),
                left_probe_id: pair.left_probe_id.clone(),
                right_probe_id: pair.right_probe_id.clone(),
                terminal_outcome: "completed".into(),
                accepted: false,
                similarity: 0.1,
                observed_at: "2026-08-23T00:00:00Z".into(),
                provenance: provenance(),
            })
            .collect();
        (
            CalibrationEvidenceGraph {
                fixtures,
                hard_slices: vec![HardSlice {
                    slice_id: automatic,
                    membership_predicate: "frozen".into(),
                    automatic_eligibility: true,
                    runtime_router_predicate: "automatic".into(),
                }],
                gallery_envelope: Some(envelope),
                selected_probes,
                selected_pairs,
                ..Default::default()
            },
            RawMetricDocument {
                schema_version: 1,
                records,
                pairwise_records,
            },
            envelope_hash,
        )
    }

    #[test]
    fn calibration_metrics_reconstruct_exact_minima_and_keep_pairwise_descriptive() {
        let (graph, raw, envelope_hash) = frozen_metric_fixture();
        let mut failures = BTreeSet::new();
        let rebuilt = reconstruct_metrics(&raw, &graph, &envelope_hash, &mut failures).metrics;
        assert!(failures.is_empty(), "unexpected failures: {failures:?}");
        assert_eq!(rebuilt.len(), 7);
        assert!(rebuilt
            .iter()
            .filter(|metric| metric.metric_id != "pairwise-impostor-FMR-diagnostic")
            .all(|metric| metric.verdict == "pass"));
        let diagnostic = rebuilt
            .iter()
            .find(|metric| metric.metric_id == "pairwise-impostor-FMR-diagnostic")
            .unwrap();
        assert_eq!(diagnostic.denominator, AGGREGATE_MIN_INDEPENDENT);
        assert_eq!(diagnostic.distinct_people, AGGREGATE_MIN_INDEPENDENT * 2);
        assert_eq!(diagnostic.errors, 0);
        assert_eq!(diagnostic.verdict, "diagnostic");

        let recorded = RecordedMetric {
            metric_id: rebuilt[0].metric_id.clone(),
            scope: rebuilt[0].scope.clone(),
            denominator: rebuilt[0].denominator + 1,
            errors: rebuilt[0].errors,
            distinct_people: rebuilt[0].distinct_people,
            distinct_acquisition_clusters: rebuilt[0].distinct_acquisition_clusters,
            point_estimate: rebuilt[0].point_estimate,
            one_sided_95_ucb: rebuilt[0].one_sided_95_ucb,
            target: rebuilt[0].target,
            sufficient: rebuilt[0].sufficient,
            insufficiency_reasons: Vec::new(),
            verdict: rebuilt[0].verdict.clone(),
        };
        assert!(!metric_matches(&recorded, &rebuilt[0]));
    }

    #[test]
    fn raw_outcomes_derive_errors_and_reject_forged_effective_error_fields() {
        let (graph, mut raw, envelope_hash) = frozen_metric_fixture();
        raw.records[0].assigned_person_id = Some("wrong-person".into());
        let mut failures = BTreeSet::new();
        let rebuilt = reconstruct_metrics(&raw, &graph, &envelope_hash, &mut failures);
        let mated = rebuilt
            .metrics
            .iter()
            .find(|metric| metric.metric_id == REQUIRED_GATES[1] && metric.scope == "aggregate")
            .unwrap();
        assert_eq!(
            mated.errors, 1,
            "the verifier must derive wrong-Person error"
        );
        let forged = format!(
            "schema_version: 1\nrecords:\n  - probe_id: p\n    terminal_outcome: completed\n    emitted_assignment_state: unidentified\n    assigned_person_id: null\n    router_abstained: true\n    observed_at: now\n    effective: false\n    error: false\n    provenance:\n      model_generation: m\n      calibration_generation: c\n      gallery_envelope_sha256: {}\n      pipeline_configuration_sha256: {}\npairwise_records: []\n",
            "a".repeat(64), "b".repeat(64)
        );
        assert!(serde_yaml::from_str::<RawMetricDocument>(&forged).is_err());
    }

    #[test]
    fn strict_recognition_keeps_looks_and_uses_different_person_runner_up() {
        let query = unit_vector(1.0, 0.0, "model-1");
        let people = vec![
            PersonReferences {
                person_id: "person-a".into(),
                templates: vec![
                    trusted("a-front", "front", unit_vector(1.0, 0.0, "model-1")),
                    trusted("a-side", "side", unit_vector(0.99, 0.1, "model-1")),
                ],
            },
            PersonReferences {
                person_id: "person-b".into(),
                templates: vec![trusted(
                    "b-front",
                    "front",
                    unit_vector(0.8, 0.6, "model-1"),
                )],
            },
        ];
        let policy = StrictRecognitionPolicy {
            model_generation: "model-1".into(),
            calibration_generation: "cal-1".into(),
            envelope_hash: "envelope-1".into(),
            strict_automatic_enabled: true,
            automatic_threshold: 0.95,
            suggestion_threshold: 0.7,
            runner_up_margin: 0.15,
            minimum_quality: 0.7,
            people_max: 2,
            looks_per_person_max: 2,
            templates_per_look_max: 1,
            total_templates_max: 3,
        };
        let decision = recognize_strict(
            &query,
            0.95,
            &people,
            &HashSet::new(),
            &policy,
            "envelope-1",
        )
        .unwrap();
        assert_eq!(decision.state, AssignmentState::CommittedStrictAutomatic);
        assert_eq!(decision.person_id.as_deref(), Some("person-a"));
        assert_eq!(decision.winning_look_id.as_deref(), Some("front"));
        assert!((decision.runner_up_margin.unwrap() - 0.2).abs() < 1e-5);
    }

    #[test]
    fn strict_recognition_generation_envelope_margin_and_cannot_link_fail_closed() {
        let query = unit_vector(1.0, 0.0, "model-1");
        let people = vec![PersonReferences {
            person_id: "person-a".into(),
            templates: vec![trusted(
                "a-front",
                "front",
                unit_vector(1.0, 0.0, "model-1"),
            )],
        }];
        let policy = StrictRecognitionPolicy {
            model_generation: "model-1".into(),
            calibration_generation: "cal-1".into(),
            envelope_hash: "expected".into(),
            strict_automatic_enabled: true,
            automatic_threshold: 0.95,
            suggestion_threshold: 0.7,
            runner_up_margin: 0.1,
            minimum_quality: 0.7,
            people_max: 1,
            looks_per_person_max: 1,
            templates_per_look_max: 1,
            total_templates_max: 1,
        };
        let drift =
            recognize_strict(&query, 0.95, &people, &HashSet::new(), &policy, "changed").unwrap();
        assert_eq!(drift.state, AssignmentState::Suggestion);
        let blocked = recognize_strict(
            &query,
            0.95,
            &people,
            &HashSet::from(["person-a".to_string()]),
            &policy,
            "expected",
        )
        .unwrap();
        assert_eq!(blocked.state, AssignmentState::Unidentified);
    }

    #[test]
    fn conservative_clustering_blocks_chains_constraints_and_duplicate_density() {
        let candidate = |id: &str, family: &str, x: f32, y: f32| ClusterCandidate {
            observation_id: id.into(),
            duplicate_family_id: family.into(),
            quality: 0.95,
            operator_confirmed: false,
            vector: unit_vector(x, y, "model-1"),
        };
        let chain = vec![
            candidate("a", "fa", 1.0, 0.0),
            candidate("b", "fb", 0.8660254, 0.5),
            candidate("c", "fc", 0.5, 0.8660254),
        ];
        let assignment =
            cluster_unnamed_conservative(&chain, 0.8, 0.7, 1, &HashSet::new()).unwrap();
        assert_eq!(assignment[0], assignment[1]);
        assert_ne!(assignment[0], assignment[2]);

        let blocked = cluster_unnamed_conservative(
            &chain[..2],
            0.8,
            0.7,
            1,
            &HashSet::from([("a".to_string(), "b".to_string())]),
        )
        .unwrap();
        assert_ne!(blocked[0], blocked[1]);

        let duplicates = vec![
            candidate("a", "same-family", 1.0, 0.0),
            candidate("b", "same-family", 1.0, 0.0),
            candidate("c", "same-family", 1.0, 0.0),
        ];
        let no_density =
            cluster_unnamed_conservative(&duplicates, 0.8, 0.7, 2, &HashSet::new()).unwrap();
        assert_eq!(no_density, vec![None, None, None]);
    }

    fn provisioned_embedder() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("w600k_r50.onnx");
        path.is_file().then_some(path)
    }

    fn face_at(x: f32, y: f32) -> Face {
        Face {
            bbox: [x, y, 96.0, 112.0],
            score: 0.95,
            landmarks: [
                [x + 28.0, y + 40.0],
                [x + 68.0, y + 40.0],
                [x + 48.0, y + 63.0],
                [x + 31.0, y + 86.0],
                [x + 65.0, y + 86.0],
            ],
        }
    }

    #[test]
    fn bundled_yunet_loads_and_passes_self_check() {
        // Compiled-in YuNet must parse under tract, execute on a blank frame,
        // and expose the 12-output 2023mar layout (WP-020 startup guard).
        let det = Detector::load_from_bytes(BUNDLED_YUNET);
        assert!(det.is_ok(), "bundled YuNet failed: {:?}", det.err());
    }

    #[test]
    fn comparison_rejects_dimension_generation_and_non_finite_inputs() {
        let valid = IdentityVector::new(vec![0.0; EMBEDDING_DIM], "g1");
        let other = IdentityVector::new(vec![0.0; EMBEDDING_DIM], "g2");
        assert_eq!(
            cosine_checked(&valid, &other).unwrap_err().code,
            "generation_mismatch"
        );
        let short = IdentityVector::new(vec![1.0], "g1");
        assert_eq!(
            cosine_checked(&valid, &short).unwrap_err().code,
            "dimension_mismatch"
        );
        let mut non_finite = valid.clone();
        non_finite.values[0] = f32::NAN;
        assert_eq!(
            cosine_checked(&valid, &non_finite).unwrap_err().code,
            "non_finite"
        );
    }

    #[test]
    fn malicious_manifest_paths_hashes_and_empty_files_fail_closed() {
        let root = std::env::temp_dir().join(format!("facial-wp080-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let bytes = b"declared-model-bytes";
        std::fs::write(root.join("model.onnx"), bytes).unwrap();
        let artifact = ModelArtifactManifest {
            role: "embedder".to_string(),
            relative_path: Some("model.onnx".to_string()),
            sha256: "0".repeat(64),
            bytes: bytes.len() as u64,
            provenance: "fixture".to_string(),
            license: "fixture".to_string(),
            external_data_allowed: false,
        };
        assert_eq!(
            secure_read_declared(&canonical_root, &artifact)
                .unwrap_err()
                .code,
            "artifact_hash_mismatch"
        );
        let mut traversal = artifact.clone();
        traversal.relative_path = Some("../outside.onnx".to_string());
        assert_eq!(
            secure_read_declared(&canonical_root, &traversal)
                .unwrap_err()
                .code,
            "unsafe_artifact_path"
        );
        std::fs::write(root.join("empty.onnx"), []).unwrap();
        let mut empty = artifact;
        empty.relative_path = Some("empty.onnx".to_string());
        empty.bytes = 0;
        empty.sha256 = sha256_hex(&[]);
        assert_eq!(
            secure_read_declared(&canonical_root, &empty)
                .unwrap_err()
                .code,
            "model_empty"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_and_external_data_onnx_are_rejected_before_inference() {
        assert_eq!(
            load_embedder(b"not-an-onnx-model").unwrap_err().code,
            "model_parse_rejected"
        );

        use tract_onnx::pb::{
            tensor_proto, GraphProto, ModelProto, OperatorSetIdProto, StringStringEntryProto,
            TensorProto,
        };
        let external = TensorProto {
            dims: vec![1],
            data_type: tensor_proto::DataType::Float as i32,
            name: "external_weight".to_string(),
            data_location: Some(tensor_proto::DataLocation::External as i32),
            external_data: vec![StringStringEntryProto {
                key: "location".to_string(),
                value: "../undeclared.bin".to_string(),
            }],
            ..Default::default()
        };
        let model = ModelProto {
            ir_version: 8,
            opset_import: vec![OperatorSetIdProto {
                domain: String::new(),
                version: 13,
            }],
            graph: Some(GraphProto {
                name: "external-data-fixture".to_string(),
                initializer: vec![external],
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = model.encode_to_vec();
        let error = load_embedder(&bytes).unwrap_err();
        assert_eq!(error.code, "model_parse_rejected");
        assert!(
            error.message.to_ascii_lowercase().contains("external"),
            "{error}"
        );
    }

    #[test]
    fn generation_binds_detection_and_policy_semantics() {
        let Some(model) = provisioned_embedder() else {
            return;
        };
        let root =
            std::env::temp_dir().join(format!("facial-wp080-generation-{}", uuid::Uuid::new_v4()));
        let manifest_path = root.join("manifest.json");
        let engine = IdentityEngine::provision(&model, None, &manifest_path).unwrap();
        let baseline = engine.generation().to_string();
        let mut changed = engine.manifest().clone();
        changed.detection_threshold += 0.01;
        assert_ne!(baseline, generation_for_manifest(&changed).unwrap());
        changed = engine.manifest().clone();
        changed.external_data_policy = "different-policy".to_string();
        assert_ne!(baseline, generation_for_manifest(&changed).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_reprovision_preserves_last_known_good_manifest_and_restart() {
        let Some(model) = provisioned_embedder() else {
            return;
        };
        let root =
            std::env::temp_dir().join(format!("facial-wp080-reprovision-{}", uuid::Uuid::new_v4()));
        let manifest_path = root.join("manifest.json");
        let accepted = IdentityEngine::provision(&model, None, &manifest_path).unwrap();
        let accepted_generation = accepted.generation().to_string();
        let accepted_manifest = std::fs::read(&manifest_path).unwrap();

        let malformed = root.join("malformed.onnx");
        let malformed_bytes = b"not-an-onnx-model";
        std::fs::write(&malformed, malformed_bytes).unwrap();
        let orphan = root.join(format!("embedder-{}.onnx", sha256_hex(malformed_bytes)));
        let error = match IdentityEngine::provision(&malformed, None, &manifest_path) {
            Ok(_) => panic!("malformed reprovision unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.code, "model_parse_rejected");
        assert_eq!(std::fs::read(&manifest_path).unwrap(), accepted_manifest);
        assert!(!orphan.exists());

        let restarted = IdentityEngine::load_manifest(&manifest_path).unwrap();
        assert_eq!(restarted.generation(), accepted_generation);
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.contains(".candidate-"))
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn normalization_rejects_zero_and_non_finite_outputs() {
        assert_eq!(
            normalize_embedding(&mut [0.0, 0.0]).unwrap_err().code,
            "zero_norm"
        );
        assert_eq!(
            normalize_embedding(&mut [f32::INFINITY, 0.0])
                .unwrap_err()
                .code,
            "non_finite"
        );
    }

    #[test]
    fn invalid_landmarks_are_structured() {
        let mut invalid = face_at(10.0, 10.0);
        invalid.landmarks[2][0] = f32::NAN;
        assert_eq!(
            validate_face(&invalid, 320, 240).unwrap_err().code,
            "non_finite"
        );
        invalid = face_at(10.0, 10.0);
        invalid.landmarks[1] = invalid.landmarks[0];
        assert_eq!(
            validate_face(&invalid, 320, 240).unwrap_err().code,
            "invalid_landmarks"
        );
    }

    #[test]
    fn provisioned_model_embeds_each_valid_detection_deterministically() {
        let Some(model) = provisioned_embedder() else {
            return;
        };
        let root = std::env::temp_dir().join(format!("facial-wp080-{}", uuid::Uuid::new_v4()));
        let manifest = root.join("match-inference-manifest-v1.json");
        let engine = IdentityEngine::provision(&model, None, &manifest)
            .expect("provision embedder manifest");
        let original_generation = generation_for_manifest(engine.manifest()).unwrap();
        let mut renamed_manifest = engine.manifest().clone();
        renamed_manifest.embedder.relative_path = Some("renamed.onnx".to_string());
        assert_eq!(
            original_generation,
            generation_for_manifest(&renamed_manifest).unwrap()
        );
        let image = image::ImageBuffer::from_fn(320, 240, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        });
        let mut invalid = face_at(115.0, 60.0);
        invalid.landmarks[0][0] = f32::NAN;
        let detections = vec![face_at(10.0, 20.0), invalid, face_at(210.0, 80.0)];
        let first = engine
            .embed_detections(image.clone(), detections.clone())
            .unwrap();
        let second = engine.embed_detections(image, detections).unwrap();
        assert_eq!(first.faces.len(), 2);
        assert_eq!(first.failures.len(), 1);
        assert_eq!(first.failures[0].code, "non_finite");
        assert!(first.faces.iter().all(|face| {
            face.embedding_dim == EMBEDDING_DIM
                && face.embedding.values.len() == EMBEDDING_DIM
                && face.generation == engine.generation()
                && face
                    .bbox_normalized
                    .iter()
                    .all(|value| (0.0..=1.0).contains(value))
                && face
                    .landmarks_normalized
                    .iter()
                    .flatten()
                    .all(|value| (0.0..=1.0).contains(value))
        }));
        assert_eq!(first.faces[0].embedding, second.faces[0].embedding);
        assert_eq!(first.faces[1].embedding, second.faces[1].embedding);
        assert_eq!(first.faces[0].generation, first.faces[1].generation);
        assert_eq!(
            engine
                .embed_detections(RgbImage::new(8, 8), Vec::new())
                .unwrap_err()
                .code,
            "missing_face"
        );
    }

    #[test]
    fn cluster_embeddings_groups_by_threshold() {
        let vector = |a: f32, b: f32, c: f32| {
            let mut values = vec![0.0; EMBEDDING_DIM];
            values[0] = a;
            values[1] = b;
            values[2] = c;
            IdentityVector::new(values, "g1")
        };
        let e = vec![
            vector(1.0, 0.0, 0.0),
            vector(0.999, 0.0447, 0.0),
            vector(0.0, 1.0, 0.0),
            vector(0.0, 0.9988, 0.0499),
            vector(0.0, 0.0, 1.0),
        ];
        let clusters = cluster_embeddings(&e, 0.95).unwrap();
        assert_eq!(clusters, vec![0, 0, 1, 1, 2]);
        // Stricter threshold splits everything.
        let clusters = cluster_embeddings(&e, 0.9999).unwrap();
        assert_eq!(clusters, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn laplacian_variance_orders_sharp_above_blurred() {
        // Checkerboard = maximal high-frequency energy; flat = zero.
        let mut sharp = RgbImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let v = if (x + y) % 2 == 0 { 255u8 } else { 0u8 };
                sharp.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        let flat = RgbImage::from_pixel(32, 32, image::Rgb([128, 128, 128]));
        let s = laplacian_variance(&sharp, None);
        let f = laplacian_variance(&flat, None);
        assert!(s > 10_000.0, "checkerboard variance was {s}");
        assert_eq!(f, 0.0);
        // Restricting to a bbox works and stays finite.
        let cropped = laplacian_variance(&sharp, Some([4.0, 4.0, 16.0, 16.0]));
        assert!(cropped > 10_000.0);
        // Degenerate bbox -> 0, no panic.
        assert_eq!(
            laplacian_variance(&sharp, Some([30.0, 30.0, 1.0, 1.0])),
            0.0
        );
    }

    #[test]
    fn yaw_bucket_classifies_geometry() {
        // Frontal: nose centered between the eyes.
        let frontal = [
            [30.0, 40.0],
            [70.0, 40.0],
            [50.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        assert_eq!(yaw_bucket(&frontal).0, "frontal");
        // Quarter: nose clearly off-center.
        let quarter = [
            [30.0, 40.0],
            [70.0, 40.0],
            [38.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        assert_eq!(yaw_bucket(&quarter).0, "quarter");
        // Profile: nose outside the eye span.
        let profile = [
            [30.0, 40.0],
            [70.0, 40.0],
            [25.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        let (bucket, ratio) = yaw_bucket(&profile);
        assert_eq!(bucket, "profile");
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn hair_color_flag_spots_the_pink_wig() {
        // Image: pink strip above the face box, skin-ish inside it.
        let mut img = RgbImage::from_pixel(100, 100, image::Rgb([230, 120, 190])); // pink
        for y in 50..100 {
            for x in 20..80 {
                img.put_pixel(x, y, image::Rgb([210, 170, 140])); // skin-ish
            }
        }
        let (label, confidence) = hair_color_flag(&img, [20.0, 50.0, 60.0, 50.0]);
        assert_eq!(label, "pink_purple");
        assert!(confidence > 0.9, "confidence was {confidence}");

        // Black hair.
        let mut img = RgbImage::from_pixel(100, 100, image::Rgb([18, 16, 15]));
        for y in 50..100 {
            for x in 20..80 {
                img.put_pixel(x, y, image::Rgb([210, 170, 140]));
            }
        }
        let (label, _) = hair_color_flag(&img, [20.0, 50.0, 60.0, 50.0]);
        assert_eq!(label, "black");

        // Degenerate box at the image top -> unknown, no panic.
        let (label, confidence) = hair_color_flag(&img, [0.0, 0.0, 10.0, 0.0]);
        assert_eq!(label, "unknown");
        assert_eq!(confidence, 0.0);
    }

    #[test]
    fn rgb_to_hsv_sanity() {
        let (h, s, v) = rgb_to_hsv(255, 0, 0);
        assert!(h.abs() < 1.0 && (s - 1.0).abs() < 1e-5 && (v - 1.0).abs() < 1e-5);
        let (_, s, v) = rgb_to_hsv(128, 128, 128);
        assert!(s < 1e-5 && (v - 0.50196).abs() < 1e-3);
    }
}
