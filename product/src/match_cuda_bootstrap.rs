//! Candidate-only payload verification. Every advance is one supervised worker unit.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

// Derived from the exact extracted files of independently SHA256-verified NVIDIA
// archives in wp086-cuda-candidate-plan.json; arbitrary operator manifests cannot
// grant runtime authority. Updating the payload requires a new verified digest.
pub(crate) const PAYLOAD_SHA256: &str =
    "dd1ff1d4b4dad7082e66ad66957c21c79d1ee2f49ad9accc1134ae6e39db6437";
pub(crate) const MAX_STEPS: usize = 10000;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedGpuPeaks {
    pub limit_bytes: u64,
    pub reserved_high_bytes: u64,
    pub used_high_bytes: u64,
}
pub(crate) fn managed_gpu_peaks() -> Result<Option<ManagedGpuPeaks>, &'static str> {
    tract_cuda::managed_pool_peaks()
        .map(|value| {
            value.map(
                |(limit_bytes, reserved_high_bytes, used_high_bytes)| ManagedGpuPeaks {
                    limit_bytes,
                    reserved_high_bytes,
                    used_high_bytes,
                },
            )
        })
        .map_err(|_| "candidate_managed_pool_query_failed")
}

const MAX_FILES: usize = 4096;
const MAX_MANIFEST: u64 = 1024 * 1024;
const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    tract_version: String,
    compiled_cuda_api: u32,
    cudnn_abi: String,
    candidate_only: bool,
    official_archives: Vec<OfficialArchive>,
    files: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfficialArchive {
    component: String,
    sha256: String,
    download_bytes: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeProvenance {
    pub payload_sha256: String,
    pub driver_api: i32,
    pub device_uuid: String,
    pub compute_major: i32,
    pub compute_minor: i32,
    pub cudart_api: i32,
    pub nvrtc_major: i32,
    pub nvrtc_minor: i32,
    pub cudnn_version: u64,
    pub cudnn_cuda_build: u64,
    pub cublas_cuda_build: u64,
    pub cache_scope: String,
}

#[cfg(windows)]
pub(crate) struct Bootstrap {
    root: PathBuf,
    pending_dirs: VecDeque<PathBuf>,
    pinned_dirs: BTreeSet<PathBuf>,
    entries: VecDeque<Entry>,
    active: Option<(File, Entry, Sha256, u64)>,
    manifest_loaded: bool,
    files_verified: usize,
    bin_inventory: Option<std::fs::ReadDir>,
    bin_inventory_complete: bool,
    bin_entries: usize,
    expected_dlls: BTreeSet<String>,
    steps: usize,
    buffer: Vec<u8>,
    native: NativeState,
    // Pins outlive loaded DLL handles and protect NVRTC's later path-based reads.
    pins: Vec<File>,
}

#[cfg(windows)]
struct NativeState {
    stage: u8,
    pool_limit_bytes: u64,
    preload_index: usize,
    libraries: std::collections::BTreeMap<String, libloading::Library>,
    cache_pins: Vec<File>,
    info: NativeProvenance,
}

#[cfg(windows)]
fn pin(path: &Path, directory: bool) -> Result<File, &'static str> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        )
        .open(path)
        .map_err(|_| "candidate_pin_failed")?;
    let meta = file.metadata().map_err(|_| "candidate_stat_failed")?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || meta.is_dir() != directory {
        return Err("candidate_reparse_or_type_rejected");
    }
    Ok(file)
}

#[cfg(windows)]
impl Bootstrap {
    pub(crate) fn begin(root: PathBuf, pool_limit_bytes: u64) -> Result<Self, &'static str> {
        if pool_limit_bytes == 0
            || pool_limit_bytes > crate::match_store::ResourceBudget::default().gpu_vram_bytes
        {
            return Err("candidate_gpu_limit_invalid");
        }
        if !root.is_absolute()
            || root.as_os_str().len() > 16000
            || root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err("candidate_root_invalid");
        }
        // Root ancestry is pinned from the drive down before any child lookup.
        let mut ancestors: Vec<_> = root.ancestors().map(Path::to_path_buf).collect();
        if ancestors.len() > 128 {
            return Err("candidate_root_too_deep");
        }
        ancestors.reverse();
        Ok(Self {
            root,
            pending_dirs: ancestors.into(),
            pinned_dirs: BTreeSet::new(),
            entries: VecDeque::new(),
            active: None,
            manifest_loaded: false,
            files_verified: 0,
            bin_inventory: None,
            bin_inventory_complete: false,
            bin_entries: 0,
            expected_dlls: BTreeSet::new(),
            steps: 0,
            buffer: vec![0; CHUNK],
            pins: Vec::new(),
            native: NativeState {
                stage: 0,
                pool_limit_bytes,
                preload_index: 0,
                libraries: std::collections::BTreeMap::new(),
                cache_pins: Vec::new(),
                info: NativeProvenance {
                    payload_sha256: PAYLOAD_SHA256.into(),
                    driver_api: 0,
                    device_uuid: String::new(),
                    compute_major: 0,
                    compute_minor: 0,
                    cudart_api: 0,
                    nvrtc_major: 0,
                    nvrtc_minor: 0,
                    cudnn_version: 0,
                    cudnn_cuda_build: 0,
                    cublas_cuda_build: 0,
                    cache_scope: String::new(),
                },
            },
        })
    }
    pub(crate) fn advance(&mut self) -> Result<Option<NativeProvenance>, &'static str> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err("candidate_bootstrap_step_limit");
        }
        if let Some(path) = self.pending_dirs.pop_front() {
            if self.pinned_dirs.insert(path.clone()) {
                self.pins.push(pin(&path, true)?);
            }
            return Ok(None);
        }
        if !self.manifest_loaded {
            let mut file = pin(&self.root.join("candidate-manifest.json"), false)?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_MANIFEST + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "candidate_manifest_read_failed")?;
            if bytes.len() as u64 > MAX_MANIFEST
                || format!("{:x}", Sha256::digest(&bytes)) != PAYLOAD_SHA256
            {
                return Err("candidate_manifest_untrusted");
            }
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|_| "candidate_manifest_invalid")?;
            if manifest.schema != "facial-cuda-candidate-payload-v1"
                || manifest.tract_version != "0.23.5"
                || manifest.compiled_cuda_api != 13000
                || manifest.cudnn_abi != "9.21.1"
                || !manifest.candidate_only
                || manifest.official_archives.len() != 5
                || manifest.official_archives.iter().any(|a| {
                    a.component.is_empty() || a.sha256.len() != 64 || a.download_bytes == 0
                })
                || manifest.files.is_empty()
                || manifest.files.len() > MAX_FILES
            {
                return Err("candidate_manifest_contract");
            }
            let mut seen = BTreeSet::new();
            let mut total = 0u64;
            let mut dirs = BTreeSet::new();
            for entry in &manifest.files {
                let path = Path::new(&entry.path);
                if entry.path.len() > 1024
                    || !entry.path.is_ascii()
                    || entry.path.contains(['\\', ':'])
                    || path.is_absolute()
                    || path
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                    || !seen.insert(entry.path.clone())
                    || entry.sha256.len() != 64
                    || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("candidate_manifest_path");
                }
                if let Some(name) = entry.path.strip_prefix("bin/") {
                    if name.contains('/') || !name.ends_with(".dll") {
                        return Err("candidate_bin_manifest_invalid");
                    }
                    self.expected_dlls.insert(name.to_ascii_lowercase());
                }
                total = total
                    .checked_add(entry.bytes)
                    .ok_or("candidate_size_overflow")?;
                if total > 4 * 1024 * 1024 * 1024 {
                    return Err("candidate_payload_limit");
                }
                for parent in path
                    .ancestors()
                    .skip(1)
                    .filter(|p| !p.as_os_str().is_empty())
                {
                    dirs.insert(self.root.join(parent));
                }
            }
            if dirs.len() > MAX_FILES {
                return Err("candidate_directory_limit");
            }
            let mut dirs: Vec<_> = dirs.into_iter().collect();
            dirs.sort_by_key(|p| p.components().count());
            self.pending_dirs = dirs.into();
            self.entries = manifest.files.into();
            self.pins.push(file);
            self.manifest_loaded = true;
            return Ok(None);
        }
        if !self.bin_inventory_complete {
            if self.bin_inventory.is_none() {
                self.bin_inventory = Some(
                    std::fs::read_dir(self.root.join("bin"))
                        .map_err(|_| "candidate_bin_inventory_failed")?,
                );
                return Ok(None);
            }
            match self.bin_inventory.as_mut().unwrap().next() {
                Some(Ok(entry)) => {
                    self.bin_entries += 1;
                    if self.bin_entries > 64 {
                        return Err("candidate_bin_inventory_limit");
                    }
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| "candidate_bin_name_invalid")?;
                    if !self.expected_dlls.contains(&name.to_ascii_lowercase()) {
                        return Err("candidate_unlisted_loadable_file");
                    }
                    return Ok(None);
                }
                Some(Err(_)) => return Err("candidate_bin_inventory_failed"),
                None => {
                    self.bin_inventory.take();
                    self.bin_inventory_complete = true;
                    return Ok(None);
                }
            }
        }
        if let Some((mut file, entry, mut hash, mut consumed)) = self.active.take() {
            let count = file
                .read(&mut self.buffer)
                .map_err(|_| "candidate_payload_read_failed")?;
            consumed += count as u64;
            if consumed > entry.bytes {
                return Err("candidate_payload_changed");
            }
            hash.update(&self.buffer[..count]);
            if count == 0 {
                if consumed != entry.bytes || format!("{:x}", hash.finalize()) != entry.sha256 {
                    return Err("candidate_payload_hash_mismatch");
                }
                self.pins.push(file);
                self.files_verified += 1;
            } else {
                self.active = Some((file, entry, hash, consumed));
            }
            return Ok(None);
        }
        if let Some(entry) = self.entries.pop_front() {
            let file = pin(&self.root.join(&entry.path), false)?;
            if file.metadata().map_err(|_| "candidate_stat_failed")?.len() != entry.bytes {
                return Err("candidate_payload_changed");
            }
            self.active = Some((file, entry, Sha256::new(), 0));
            return Ok(None);
        }
        self.buffer = Vec::new();
        self.native.advance(&self.root)
    }
    pub(crate) fn ready(&self) -> bool {
        self.native.stage == 8
    }
}

#[cfg(windows)]
impl NativeState {
    fn load_system_search(path: &Path) -> Result<libloading::Library, &'static str> {
        unsafe {
            libloading::os::windows::Library::load_with_flags(
                path,
                windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
            .map(Into::into)
            .map_err(|_| "candidate_library_load_failed")
        }
    }
    fn advance(&mut self, root: &Path) -> Result<Option<NativeProvenance>, &'static str> {
        use windows_sys::Win32::System::LibraryLoader::{
            SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        unsafe {
            match self.stage {
                0 => {
                    tract_cuda::configure_managed_pool(
                        usize::try_from(self.pool_limit_bytes)
                            .map_err(|_| "candidate_gpu_quota_overflow")?,
                    )
                    .map_err(|_| "candidate_gpu_quota_configuration")?;
                    // Resolve system ownership through the OS, never inherited environment.
                    let mut system = [0u16; 32768];
                    let length = windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
                        system.as_mut_ptr(),
                        system.len() as u32,
                    ) as usize;
                    if length == 0 || length >= system.len() {
                        return Err("candidate_system_directory_failed");
                    }
                    use std::os::windows::ffi::OsStringExt;
                    let driver = PathBuf::from(std::ffi::OsString::from_wide(&system[..length]))
                        .join("nvcuda.dll");
                    self.cache_pins.push(pin(&driver, false)?);
                    // System driver is loaded first, before any candidate CUDA DLL can resolve it.
                    self.libraries
                        .insert("nvcuda.dll".into(), Self::load_system_search(&driver)?);
                    if SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) == 0 {
                        return Err("candidate_dll_search_failed");
                    }
                }
                1 => {
                    // Dependency order verified from pinned PE import/delay-import tables.
                    // No user/application directory search: later inserted files cannot satisfy DLL imports.
                    const DLLS: &[&str] = &[
                        "cudart64_13.dll",
                        "nvrtc-builtins64_130.dll",
                        "nvrtc64_130_0.dll",
                        "cublasLt64_13.dll",
                        "cublas64_13.dll",
                        "cudnn_ext64_9.dll",
                        "cudnn_graph64_9.dll",
                        "cudnn_ops64_9.dll",
                        "cudnn_adv64_9.dll",
                        "cudnn_cnn64_9.dll",
                        "cudnn_engines_precompiled64_9.dll",
                        "cudnn_engines_runtime_compiled64_9.dll",
                        "cudnn_engines_tensor_ir64_9.dll",
                        "cudnn_heuristic64_9.dll",
                        "cudnn64_9.dll",
                    ];
                    if let Some(name) = DLLS.get(self.preload_index) {
                        self.libraries.insert(
                            (*name).into(),
                            Self::load_system_search(&root.join("bin").join(name))?,
                        );
                        self.preload_index += 1;
                        return Ok(None);
                    }
                }
                2 => {
                    let library = self
                        .libraries
                        .get("cudart64_13.dll")
                        .ok_or("candidate_library_absent")?;
                    let get: libloading::Symbol<unsafe extern "C" fn(*mut i32) -> i32> = library
                        .get(b"cudaRuntimeGetVersion\0")
                        .map_err(|_| "candidate_cudart_symbol")?;
                    if get(&mut self.info.cudart_api) != 0 || self.info.cudart_api / 1000 != 13 {
                        return Err("candidate_cudart_version");
                    }
                }
                3 => {
                    let library = self
                        .libraries
                        .get("nvrtc64_130_0.dll")
                        .ok_or("candidate_library_absent")?;
                    let get: libloading::Symbol<unsafe extern "C" fn(*mut i32, *mut i32) -> i32> =
                        library
                            .get(b"nvrtcVersion\0")
                            .map_err(|_| "candidate_nvrtc_symbol")?;
                    if get(&mut self.info.nvrtc_major, &mut self.info.nvrtc_minor) != 0
                        || self.info.nvrtc_major != 13
                        || self.info.nvrtc_minor != 0
                    {
                        return Err("candidate_nvrtc_version");
                    }
                }
                4 => {
                    let library = self
                        .libraries
                        .get("cudnn64_9.dll")
                        .ok_or("candidate_library_absent")?;
                    self.info.cudnn_version = library
                        .get::<unsafe extern "C" fn() -> usize>(b"cudnnGetVersion\0")
                        .map_err(|_| "candidate_cudnn_symbol")?(
                    ) as u64;
                    self.info.cudnn_cuda_build = library
                        .get::<unsafe extern "C" fn() -> usize>(b"cudnnGetCudartVersion\0")
                        .map_err(|_| "candidate_cudnn_symbol")?(
                    ) as u64;
                    if self.info.cudnn_version != 92501 {
                        return Err("candidate_cudnn_version");
                    }
                }
                5 => {
                    let library = self
                        .libraries
                        .get("cublas64_13.dll")
                        .ok_or("candidate_library_absent")?;
                    self.info.cublas_cuda_build = library
                        .get::<unsafe extern "C" fn() -> usize>(b"cublasGetCudartVersion\0")
                        .map_err(|_| "candidate_cublas_symbol")?(
                    ) as u64;
                }
                6 => {
                    let library = self
                        .libraries
                        .get("nvcuda.dll")
                        .ok_or("candidate_library_absent")?;
                    let init = library
                        .get::<unsafe extern "C" fn(u32) -> i32>(b"cuInit\0")
                        .map_err(|_| "candidate_driver_symbol")?;
                    let get = library
                        .get::<unsafe extern "C" fn(*mut i32) -> i32>(b"cuDriverGetVersion\0")
                        .map_err(|_| "candidate_driver_symbol")?;
                    if get(&mut self.info.driver_api) != 0
                        || self.info.driver_api < 13000
                        || init(0) != 0
                    {
                        return Err("candidate_driver_unavailable");
                    }
                    let device = library
                        .get::<unsafe extern "C" fn(*mut i32, i32) -> i32>(b"cuDeviceGet\0")
                        .map_err(|_| "candidate_driver_symbol")?;
                    let attribute = library
                        .get::<unsafe extern "C" fn(*mut i32, i32, i32) -> i32>(
                            b"cuDeviceGetAttribute\0",
                        )
                        .map_err(|_| "candidate_driver_symbol")?;
                    let uuid = library
                        .get::<unsafe extern "C" fn(*mut [u8; 16], i32) -> i32>(
                            b"cuDeviceGetUuid_v2\0",
                        )
                        .map_err(|_| "candidate_driver_symbol")?;
                    let mut dev = 0;
                    let mut id = [0u8; 16];
                    if device(&mut dev, 0) != 0
                        || attribute(&mut self.info.compute_major, 75, dev) != 0
                        || attribute(&mut self.info.compute_minor, 76, dev) != 0
                        || uuid(&mut id, dev) != 0
                    {
                        return Err("candidate_device_identity_failed");
                    }
                    self.info.device_uuid = id.iter().map(|b| format!("{b:02x}")).collect();
                }
                7 => {
                    self.info.cache_scope = format!(
                        "{}-{}-sm{}{}-driver{}-cold-{}",
                        PAYLOAD_SHA256,
                        self.info.device_uuid,
                        self.info.compute_major,
                        self.info.compute_minor,
                        self.info.driver_api,
                        uuid::Uuid::new_v4().simple()
                    );
                    let parent = root.join("candidate-cache");
                    match std::fs::create_dir(&parent) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(_) => return Err("candidate_cache_create_failed"),
                    }
                    self.cache_pins.push(pin(&parent, true)?);
                    let cache = parent.join(&self.info.cache_scope);
                    std::fs::create_dir(&cache).map_err(|_| "candidate_cache_create_failed")?;
                    self.cache_pins.push(pin(&cache, true)?);
                    tract_cuda::kernels::set_cubin_dir(cache)
                        .map_err(|_| "candidate_cache_already_initialized")?;
                }
                8 => return Ok(Some(self.info.clone())),
                _ => return Err("candidate_bootstrap_state"),
            }
        }
        self.stage += 1;
        Ok(if self.stage == 8 {
            Some(self.info.clone())
        } else {
            None
        })
    }
}

#[cfg(not(windows))]
pub(crate) struct Bootstrap;
#[cfg(not(windows))]
impl Bootstrap {
    pub(crate) fn begin(_: PathBuf, _: u64) -> Result<Self, &'static str> {
        Err("candidate_platform_unsupported")
    }
    pub(crate) fn advance(&mut self) -> Result<Option<NativeProvenance>, &'static str> {
        Err("candidate_platform_unsupported")
    }
    pub(crate) fn ready(&self) -> bool {
        false
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn wp086_cuda_bootstrap_rejects_untrusted_manifest_before_library_loading() {
        let root = std::env::temp_dir().join(format!(
            "candidate-untrusted-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("candidate-manifest.json"), b"{}").unwrap();
        let mut session = Bootstrap::begin(root.clone(), 16 * 1024 * 1024).unwrap();
        let mut rejected = false;
        for _ in 0..130 {
            match session.advance() {
                Err(code) => {
                    assert_eq!(code, "candidate_manifest_untrusted");
                    rejected = true;
                    break;
                }
                Ok(None) => {}
                Ok(Some(_)) => panic!("untrusted bootstrap became ready"),
            }
        }
        assert!(rejected);
        assert_eq!(session.native.stage, 0);
        assert!(!root.join("candidate-cache").exists());
        drop(session);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn wp086_cuda_payload_pins_prevent_leaf_and_parent_retarget() {
        let root =
            std::env::temp_dir().join(format!("candidate-pins-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("header.h");
        std::fs::write(&path, b"verified").unwrap();
        let directory = pin(&root, true).unwrap();
        let file = pin(&path, false).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&root, root.with_extension("moved")).is_err());
        drop(file);
        drop(directory);
        std::fs::write(&path, b"released").unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
