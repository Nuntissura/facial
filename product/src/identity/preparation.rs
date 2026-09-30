//! Worker-owned checkpoints. Each call remains subject to the supervisor deadline.
use super::*;

const CHUNK: usize = 4 * 1024 * 1024;
pub(crate) const PREPARATION_BUFFER_BYTES: u64 = 240 * 1024 * 1024;
const ARTIFACT_LIMIT: u64 = PREPARATION_BUFFER_BYTES;

struct ArtifactReader {
    file: File,
    path: PathBuf,
    declared: ModelArtifactManifest,
    bytes: Vec<u8>,
    hash: Sha256,
}
fn error(code: &str, value: impl ToString) -> IdentityError {
    IdentityError::new(code, value.to_string())
}
fn pinned_file(path: &Path) -> IdentityResult<(PathBuf, File)> {
    let meta = fs::symlink_metadata(path).map_err(|e| error("model_missing", e))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(error(
            "unsafe_artifact_path",
            "regular non-symlink file required",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|e| error("unsafe_artifact_path", e))?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    let file = options
        .open(&canonical)
        .map_err(|e| error("model_read_failed", e))?;
    if opened_file_final_path(&file, "model").map_err(|e| error("unsafe_artifact_path", e))?
        != canonical
        || !file
            .metadata()
            .map_err(|e| error("model_read_failed", e))?
            .is_file()
    {
        return Err(error(
            "unsafe_artifact_path",
            "opened model identity changed",
        ));
    }
    Ok((canonical, file))
}
impl ArtifactReader {
    fn open(root: &Path, declared: &ModelArtifactManifest) -> IdentityResult<Self> {
        if declared.bytes == 0 || declared.bytes > ARTIFACT_LIMIT || declared.external_data_allowed
        {
            return Err(error(
                "artifact_size_mismatch",
                "model artifact exceeds preparation bounds",
            ));
        }
        let relative = validate_relative_path(
            declared
                .relative_path
                .as_deref()
                .ok_or_else(|| error("manifest_invalid", "artifact path missing"))?,
            &declared.role,
        )?;
        let target = root.join(relative);
        let (path, file) = pinned_file(&target)?;
        if !path.starts_with(root) || path.parent() != target.parent() {
            return Err(error("unsafe_artifact_path", "artifact escaped model root"));
        }
        if file
            .metadata()
            .map_err(|e| error("model_read_failed", e))?
            .len()
            != declared.bytes
        {
            return Err(error(
                "artifact_size_mismatch",
                "model size differs from manifest",
            ));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(declared.bytes as usize)
            .map_err(|e| error("model_read_failed", e))?;
        Ok(Self {
            file,
            path,
            declared: declared.clone(),
            bytes,
            hash: Sha256::new(),
        })
    }
    fn step(&mut self) -> IdentityResult<bool> {
        let remaining = self.declared.bytes as usize - self.bytes.len();
        if remaining > 0 {
            let start = self.bytes.len();
            self.bytes.resize(start + remaining.min(CHUNK), 0);
            self.file
                .read_exact(&mut self.bytes[start..])
                .map_err(|e| error("model_read_failed", e))?;
            self.hash.update(&self.bytes[start..]);
        }
        if self.bytes.len() as u64 != self.declared.bytes {
            return Ok(false);
        }
        let mut extra = [0];
        if self
            .file
            .read(&mut extra)
            .map_err(|e| error("model_read_failed", e))?
            != 0
            || self
                .file
                .metadata()
                .map_err(|e| error("model_read_failed", e))?
                .len()
                != self.declared.bytes
            || opened_file_final_path(&self.file, "model")
                .map_err(|e| error("unsafe_artifact_path", e))?
                != self.path
        {
            return Err(error(
                "artifact_size_mismatch",
                "model identity or size changed",
            ));
        }
        if format!("{:x}", self.hash.clone().finalize()) != self.declared.sha256 {
            return Err(error(
                "artifact_hash_mismatch",
                "model hash differs from manifest",
            ));
        }
        Ok(true)
    }
}

pub(crate) struct PreparationSession {
    manifest: InferenceManifest,
    runtime: String,
    root: PathBuf,
    stage: u8,
    reader: Option<ArtifactReader>,
    embedder_bytes: Vec<u8>,
    detector_bytes: Vec<u8>,
    model_path: PathBuf,
    parsed: Option<Model>,
    detector: Option<Detector>,
    runnable: Option<Runnable>,
}
impl PreparationSession {
    pub(crate) fn begin(path: &Path, runtime_name: &str) -> IdentityResult<Self> {
        let actual = runtime_for_name(runtime_name)
            .and_then(|runtime| runtime.name())
            .map_err(|e| error("runtime_unavailable", e))?;
        if actual != runtime_name {
            return Err(error("runtime_mismatch", "requested runtime unavailable"));
        }
        let (path, mut file) = pinned_file(path)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|e| error("model_read_failed", e))?;
        if bytes.len() > 65536 {
            return Err(error("manifest_invalid", "manifest exceeds 64 KiB"));
        }
        let manifest: InferenceManifest =
            serde_json::from_slice(&bytes).map_err(|e| error("manifest_invalid", e))?;
        validate_manifest_contract(&manifest)?;
        if manifest
            .embedder
            .bytes
            .checked_add(manifest.detector.bytes)
            .is_none_or(|total| total > PREPARATION_BUFFER_BYTES)
        {
            return Err(error(
                "artifact_size_mismatch",
                "combined model bytes exceed preparation buffer budget",
            ));
        }
        if generation_for_manifest(&manifest)? != manifest.generation {
            return Err(error(
                "generation_mismatch",
                "manifest generation differs from contents",
            ));
        }
        let root = path
            .parent()
            .ok_or_else(|| error("manifest_path_invalid", "model root missing"))?
            .to_path_buf();
        Ok(Self {
            manifest,
            runtime: runtime_name.into(),
            root,
            stage: 0,
            reader: None,
            embedder_bytes: Vec::new(),
            detector_bytes: Vec::new(),
            model_path: PathBuf::new(),
            parsed: None,
            detector: None,
            runnable: None,
        })
    }
    pub(crate) fn generation(&self) -> &str {
        &self.manifest.generation
    }
    pub(crate) fn phase(&self) -> PreparationPhase {
        match self.stage {
            0 | 1 => PreparationPhase::EmbedderReadHash,
            2 | 3 => PreparationPhase::DetectorReadHash,
            4 => PreparationPhase::DetectorParse,
            5 => PreparationPhase::DetectorPrepare,
            6 => PreparationPhase::DetectorStartup,
            7 => PreparationPhase::EmbedderParse,
            8 => PreparationPhase::EmbedderPrepare,
            _ => PreparationPhase::EmbedderStartup,
        }
    }
    pub(crate) fn step(&mut self) -> IdentityResult<Option<IdentityEngine>> {
        let result = self.advance();
        if result.is_err() {
            self.stage = 255;
            self.reader = None;
            self.embedder_bytes = Vec::new();
            self.detector_bytes = Vec::new();
            self.parsed = None;
            self.runnable = None;
            self.detector = None;
        }
        result
    }
    fn advance(&mut self) -> IdentityResult<Option<IdentityEngine>> {
        match self.stage {
            0 => {
                self.reader = Some(ArtifactReader::open(&self.root, &self.manifest.embedder)?);
                self.stage = 1;
            }
            1 => {
                if self.reader.as_mut().unwrap().step()? {
                    let reader = self.reader.take().unwrap();
                    self.model_path = reader.path;
                    self.embedder_bytes = reader.bytes;
                    self.stage = 2;
                }
            }
            2 => {
                if self.manifest.detector.relative_path.is_some() {
                    self.reader = Some(ArtifactReader::open(&self.root, &self.manifest.detector)?);
                    self.stage = 3;
                } else {
                    verify_artifact_bytes(&self.manifest.detector, BUNDLED_YUNET)?;
                    self.detector_bytes = BUNDLED_YUNET.to_vec();
                    self.stage = 4;
                }
            }
            3 => {
                if self.reader.as_mut().unwrap().step()? {
                    self.detector_bytes = self.reader.take().unwrap().bytes;
                    self.stage = 4;
                }
            }
            4 | 7 => {
                let detector = self.stage == 4;
                let bytes = if detector {
                    &self.detector_bytes
                } else {
                    &self.embedder_bytes
                };
                let mut inference = onnx()
                    .map_err(|e| error("runtime_unavailable", e))?
                    .load_buffer(bytes)
                    .map_err(|e| error("model_parse_rejected", e))?;
                inference
                    .set_input_fact(
                        0,
                        if detector {
                            "1,3,640,640,f32"
                        } else {
                            "1,3,112,112,f32"
                        },
                    )
                    .map_err(|e| error("model_shape_invalid", e))?;
                self.parsed = Some(
                    inference
                        .into_model()
                        .map_err(|e| error("model_shape_invalid", e))?,
                );
                if detector {
                    self.detector_bytes = Vec::new();
                } else {
                    self.embedder_bytes = Vec::new();
                }
                self.stage += 1;
            }
            5 | 8 => {
                let runnable = runtime_for_name(&self.runtime)
                    .map_err(|e| error("runtime_unavailable", e))?
                    .prepare(self.parsed.take().unwrap())
                    .map_err(|e| error("model_prepare_failed", e))?;
                if self.stage == 5 {
                    self.detector = Some(Detector { model: runnable });
                } else {
                    self.runnable = Some(runnable);
                }
                self.stage += 1;
            }
            6 => {
                self.detector.as_ref().unwrap().self_check()?;
                self.stage = 7;
            }
            9 => {
                let model = self.runnable.take().unwrap();
                validate_embedder_startup(&model)?;
                self.stage = 10;
                return Ok(Some(IdentityEngine {
                    model,
                    model_path: self.model_path.clone(),
                    model_sha256: self.manifest.embedder.sha256.clone(),
                    detector: self.detector.take().unwrap(),
                    detector_origin: if self.manifest.detector.relative_path.is_some() {
                        "override"
                    } else {
                        "bundled"
                    },
                    detector_sha256: Some(self.manifest.detector.sha256.clone()),
                    manifest: self.manifest.clone(),
                }));
            }
            _ => {
                return Err(error(
                    "preparation_session_finished",
                    "fresh preparation session required",
                ))
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn wp086_preparation_reader_checkpoints_pinned_bytes_and_rejects_wrong_hash() {
        let root = Fixture(
            std::env::temp_dir().join(format!("wp086-model-reader-{}", uuid::Uuid::new_v4())),
        );
        fs::create_dir(&root.0).unwrap();
        let canonical = fs::canonicalize(&root.0).unwrap();
        let path = canonical.join("model.onnx");
        let bytes = vec![37u8; CHUNK + 17];
        fs::write(&path, &bytes).unwrap();
        let mut declared = ModelArtifactManifest {
            role: "embedder".into(),
            relative_path: Some("model.onnx".into()),
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
            provenance: "fixture".into(),
            license: "fixture".into(),
            external_data_allowed: false,
        };
        let mut reader = ArtifactReader::open(&canonical, &declared).unwrap();
        #[cfg(windows)]
        assert!(
            OpenOptions::new().write(true).open(&path).is_err(),
            "held source must deny concurrent writes"
        );
        assert!(!reader.step().unwrap());
        assert_eq!(reader.bytes.len(), CHUNK);
        assert!(reader.step().unwrap());
        assert_eq!(reader.bytes, bytes);
        drop(reader);
        declared.sha256 = "0".repeat(64);
        let mut reader = ArtifactReader::open(&canonical, &declared).unwrap();
        assert!(!reader.step().unwrap());
        assert_eq!(reader.step().unwrap_err().code, "artifact_hash_mismatch");
        drop(reader);
        declared.bytes += 1;
        assert!(
            matches!(ArtifactReader::open(&canonical,&declared),Err(error) if error.code=="artifact_size_mismatch")
        );
        declared.bytes = ARTIFACT_LIMIT + 1;
        assert!(
            matches!(ArtifactReader::open(&canonical,&declared),Err(error) if error.code=="artifact_size_mismatch")
        );
        declared.bytes = bytes.len() as u64;
        declared.relative_path = Some("../model.onnx".into());
        assert!(
            matches!(ArtifactReader::open(&canonical,&declared),Err(error) if error.code=="unsafe_artifact_path")
        );
    }
}
