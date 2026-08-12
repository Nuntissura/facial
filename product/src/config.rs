use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub repo_root: PathBuf,
    pub workspace_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub model_registry_path: PathBuf,
    pub debug_log_path: PathBuf,
    pub plugins_root: PathBuf,
    pub api_root: PathBuf,
    pub ingest_in_place_default: bool,
    pub max_debug_events: usize,
    pub font_size_pt: f32,
    /// Required output destination; no run/sort/task may start until this is set.
    pub copy_location: Option<PathBuf>,
    /// Optional, runtime-provisioned face-identity models + gate config (Phase 2).
    pub identity_model_path: Option<PathBuf>,
    pub identity_detector_path: Option<PathBuf>,
    pub identity_reference_dir: Option<PathBuf>,
    pub identity_negative_dir: Option<PathBuf>,
    pub identity_threshold: f32,
    pub identity_margin: f32,
    /// Min YuNet score for a face to be *counted* in gate output (collage /
    /// no-face signals). Separate from the lower alignment floor in identity.rs.
    pub identity_count_threshold: f32,
    /// face_frac (face area / image area) thresholds for the `framing` label.
    /// Calibrated on the leeseo buckets; configurable per subject/lens.
    pub framing_closeup_min: f32,
    pub framing_threequarter_min: f32,
    /// GUI palette: "paper" (light) or "ink" (dark). FACIAL_THEME overrides.
    pub theme_mode: String,
    /// Optional PIPNet 98-pt landmark model (WP-021 wave-2 signals). Resolution:
    /// this path / FACIAL_LANDMARK_MODEL / `<repo>/product/models/
    /// pipnet_r18_wflw_98.onnx` when that conventional file exists.
    pub landmark_model_path: Option<PathBuf>,
    /// Thumbnail disk-cache budget in MB (WP-043). Config `media_thumb_cache_mb`
    /// / FACIAL_MEDIA_THUMB_CACHE_MB; default 2048.
    pub media_thumb_cache_mb: u64,
}

#[derive(Deserialize)]
struct FileConfig {
    workspace_root: Option<String>,
    ingest_in_place_default: Option<bool>,
    max_debug_events: Option<usize>,
    font_size_pt: Option<f32>,
    copy_location: Option<String>,
    identity_model_path: Option<String>,
    identity_detector_path: Option<String>,
    identity_reference_dir: Option<String>,
    identity_negative_dir: Option<String>,
    identity_threshold: Option<f32>,
    identity_margin: Option<f32>,
    identity_count_threshold: Option<f32>,
    framing_closeup_min: Option<f32>,
    framing_threequarter_min: Option<f32>,
    theme_mode: Option<String>,
    landmark_model_path: Option<String>,
    media_thumb_cache_mb: Option<u64>,
}

fn discover_repo_root() -> PathBuf {
    if let Ok(value) = env::var("FACIAL_REPO_ROOT") {
        let candidate = PathBuf::from(value);
        if candidate.exists() {
            return candidate;
        }
    }

    if let Ok(exe) = env::current_exe() {
        let mut current = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        for _ in 0..8 {
            // Installed layout: facial.exe and facial-cli.exe live beside the
            // staged `product` assets under Program Files. There is no source
            // Cargo.toml there, so recognize the shipped config directly.
            if is_installed_layout(&current) {
                return current;
            }
            if current.join("product").join("src").join("main.rs").exists()
                || current.join("product").join("Cargo.toml").exists()
            {
                return current;
            }
            if current.join("Cargo.toml").exists()
                && current
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|name| name == "product")
                    .unwrap_or(false)
            {
                if let Some(parent) = current.parent() {
                    return parent.to_path_buf();
                }
            }
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent.to_path_buf();
        }
    }

    let mut current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for _ in 0..6 {
        if current.join("product").join("src").join("main.rs").exists()
            || current.join("product").join("Cargo.toml").exists()
        {
            return current;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    if current.join("product").exists() {
        current
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn is_installed_layout(repo_root: &Path) -> bool {
    repo_root
        .join("product")
        .join("config")
        .join("default.json")
        .is_file()
        && !repo_root.join("product").join("Cargo.toml").is_file()
}

fn installed_user_data_root(repo_root: &Path) -> Option<PathBuf> {
    if !is_installed_layout(repo_root) {
        return None;
    }
    env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join("Facial"))
}

pub fn load_config() -> AppConfig {
    let repo_root = discover_repo_root();
    let installed_data_root = installed_user_data_root(&repo_root);
    let install_default_config = repo_root
        .join("product")
        .join("config")
        .join("default.json");
    let settings_path = settings_path_for(&repo_root);
    // On a fresh install the user settings file may not exist yet; seed initial values from
    // the install default shipped alongside the program, then saves go to the user path.
    let has_user_settings = settings_path.exists();
    let default_config_path = if has_user_settings {
        settings_path
    } else {
        install_default_config
    };
    let mut ingest = false;
    let mut max_events = 800usize;
    // Generous default: the previous egui default read too small for this look.
    let mut font_size = 19.0f32;
    let mut workspace_root_raw: Option<String> = None;
    let mut copy_location_raw: Option<String> = None;
    let mut id_model_raw: Option<String> = None;
    let mut id_det_raw: Option<String> = None;
    let mut id_ref_raw: Option<String> = None;
    let mut id_neg_raw: Option<String> = None;
    let mut id_threshold = 0.5f32;
    let mut id_margin = 0.1f32;
    let mut id_count_threshold = 0.9f32;
    let mut framing_closeup_min = 0.09f32;
    let mut framing_threequarter_min = 0.03f32;
    let mut theme_mode = "paper".to_string();
    let mut lm_raw: Option<String> = None;
    let mut thumb_cache_mb = 2048u64;

    if let Ok(raw) = fs::read_to_string(&default_config_path) {
        if let Ok(file_cfg) = serde_json::from_str::<FileConfig>(&raw) {
            ingest = file_cfg.ingest_in_place_default.unwrap_or(false);
            if let Some(value) = file_cfg.max_debug_events {
                max_events = value;
            }
            if let Some(value) = file_cfg.font_size_pt {
                font_size = value;
            }
            if let Some(value) = file_cfg.workspace_root {
                workspace_root_raw = Some(value);
            }
            if let Some(value) = file_cfg.copy_location {
                copy_location_raw = Some(value);
            }
            id_model_raw = file_cfg.identity_model_path;
            id_det_raw = file_cfg.identity_detector_path;
            id_ref_raw = file_cfg.identity_reference_dir;
            id_neg_raw = file_cfg.identity_negative_dir;
            if let Some(value) = file_cfg.identity_threshold {
                id_threshold = value;
            }
            if let Some(value) = file_cfg.identity_margin {
                id_margin = value;
            }
            if let Some(value) = file_cfg.identity_count_threshold {
                id_count_threshold = value;
            }
            if let Some(value) = file_cfg.framing_closeup_min {
                framing_closeup_min = value;
            }
            if let Some(value) = file_cfg.framing_threequarter_min {
                framing_threequarter_min = value;
            }
            if let Some(value) = file_cfg.theme_mode {
                theme_mode = value;
            }
            lm_raw = file_cfg.landmark_model_path;
            if let Some(value) = file_cfg.media_thumb_cache_mb {
                thumb_cache_mb = value;
            }
        }
    }

    let env_path = |key: &str, fallback: Option<String>| -> Option<PathBuf> {
        env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or(fallback)
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
    };
    let identity_model_path = env_path("FACIAL_IDENTITY_MODEL", id_model_raw);
    let identity_detector_path = env_path("FACIAL_IDENTITY_DETECTOR", id_det_raw);
    let identity_reference_dir = env_path("FACIAL_IDENTITY_REF_DIR", id_ref_raw);
    let identity_negative_dir = env_path("FACIAL_IDENTITY_NEG_DIR", id_neg_raw);
    let identity_threshold = env::var("FACIAL_IDENTITY_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(id_threshold);
    let identity_margin = env::var("FACIAL_IDENTITY_MARGIN")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(id_margin);
    let identity_count_threshold = env::var("FACIAL_IDENTITY_COUNT_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(id_count_threshold);
    let framing_closeup_min = env::var("FACIAL_FRAMING_CLOSEUP_MIN")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(framing_closeup_min);
    let framing_threequarter_min = env::var("FACIAL_FRAMING_THREEQUARTER_MIN")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(framing_threequarter_min);

    let env_font = env::var("FACIAL_FONT_SIZE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(font_size)
        .clamp(10.0, 48.0);
    let theme_mode = env::var("FACIAL_THEME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(theme_mode);
    // Landmark model: env -> config -> conventional provisioned path.
    let landmark_model_path = env_path("FACIAL_LANDMARK_MODEL", lm_raw).or_else(|| {
        let conventional = repo_root
            .join("product")
            .join("models")
            .join("pipnet_r18_wflw_98.onnx");
        conventional.exists().then_some(conventional)
    });
    let copy_location = env::var("FACIAL_COPY_LOCATION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or(copy_location_raw)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);

    let env_ingest = env::var("FACIAL_INGEST_IN_PLACE")
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(ingest);
    let env_events = env::var("FACIAL_MAX_DEBUG_EVENTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(max_events);
    let media_thumb_cache_mb = env::var("FACIAL_MEDIA_THUMB_CACHE_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(thumb_cache_mb)
        .max(64);
    // A packaged install must not inherit the developer machine's workspace
    // from the shipped seed config. Once the per-user settings file exists,
    // its explicitly selected workspace is authoritative.
    let saved_workspace = if installed_data_root.is_some() && !has_user_settings {
        None
    } else {
        workspace_root_raw
    };
    let workspace_root = env::var("FACIAL_WORKSPACE_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or(saved_workspace)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| installed_data_root.clone())
        .unwrap_or_else(|| repo_root.clone());

    // Optional relocation overrides for the worktrees / data output roots.
    // When unset, both fall back to the selected workspace root, not the app install root.
    let data_root = env::var("FACIAL_DATA_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let worktrees_root = env::var("FACIAL_WORKTREES_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| data_root.as_ref().map(|root| root.join("worktrees")))
        .unwrap_or_else(|| workspace_root.join(".facial").join("worktrees"));
    let product_data_root = data_root
        .clone()
        .unwrap_or_else(|| workspace_root.join(".facial").join("data"));

    AppConfig {
        repo_root: repo_root.clone(),
        workspace_root,
        worktrees_root,
        model_registry_path: product_data_root.join("model_registry.json"),
        debug_log_path: product_data_root.join("events.jsonl"),
        plugins_root: repo_root.join("product").join("plugins"),
        api_root: product_data_root.join("api"),
        ingest_in_place_default: env_ingest,
        max_debug_events: env_events,
        font_size_pt: env_font,
        copy_location,
        identity_model_path,
        identity_detector_path,
        identity_reference_dir,
        identity_negative_dir,
        identity_threshold,
        identity_margin,
        media_thumb_cache_mb,
        identity_count_threshold,
        framing_closeup_min,
        framing_threequarter_min,
        theme_mode,
        landmark_model_path,
    }
}

/// Where user settings are read from and written to. Honors FACIAL_CONFIG_PATH so an
/// installed app (read-only Program Files) keeps settings in a user-writable dir; unset,
/// it falls back to the in-repo default so dev behavior is unchanged.
fn config_file_path(config: &AppConfig) -> PathBuf {
    settings_path_for(&config.repo_root)
}

/// Resolve the settings file path: FACIAL_CONFIG_PATH, else the installed
/// `%LOCALAPPDATA%/Facial/config/default.json`, else the in-repo default.
fn settings_path_for(repo_root: &Path) -> PathBuf {
    env::var("FACIAL_CONFIG_PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            installed_user_data_root(repo_root).map(|root| root.join("config").join("default.json"))
        })
        .unwrap_or_else(|| {
            repo_root
                .join("product")
                .join("config")
                .join("default.json")
        })
}

/// Write the persistable settings to default.json, preserving every known field.
fn write_settings_file(
    path: &Path,
    workspace_root: &Path,
    in_place: bool,
    max_events: usize,
    font_size_pt: f32,
    theme_mode: &str,
    copy_location: &Option<PathBuf>,
    identity_model_path: &Option<PathBuf>,
    identity_detector_path: &Option<PathBuf>,
    landmark_model_path: &Option<PathBuf>,
) -> std::io::Result<()> {
    let mut json = serde_json::json!({
        "workspace_root": workspace_root.to_string_lossy().to_string(),
        "ingest_in_place_default": in_place,
        "max_debug_events": max_events,
        "font_size_pt": font_size_pt,
        "theme_mode": theme_mode,
    });
    if let Some(location) = copy_location {
        json["copy_location"] = serde_json::Value::String(location.to_string_lossy().to_string());
    }
    if let Some(p) = identity_model_path {
        json["identity_model_path"] = serde_json::Value::String(p.to_string_lossy().to_string());
    }
    if let Some(p) = identity_detector_path {
        json["identity_detector_path"] = serde_json::Value::String(p.to_string_lossy().to_string());
    }
    if let Some(p) = landmark_model_path {
        json["landmark_model_path"] = serde_json::Value::String(p.to_string_lossy().to_string());
    }
    let body = serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string());
    // The settings path may live in a user dir that does not exist yet (fresh install).
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, format!("{body}\n"))
}

/// Persist the chosen UI font size (Options tab), preserving the other fields.
pub fn save_font_size(config: &AppConfig, font_size_pt: f32) -> std::io::Result<()> {
    write_settings_file(
        &config_file_path(config),
        &config.workspace_root,
        config.ingest_in_place_default,
        config.max_debug_events,
        font_size_pt,
        &config.theme_mode,
        &config.copy_location,
        &config.identity_model_path,
        &config.identity_detector_path,
        &config.landmark_model_path,
    )
}

/// Persist the copy/output location, preserving the other fields.
pub fn save_copy_location(
    config: &AppConfig,
    copy_location: &Option<PathBuf>,
) -> std::io::Result<()> {
    write_settings_file(
        &config_file_path(config),
        &config.workspace_root,
        config.ingest_in_place_default,
        config.max_debug_events,
        config.font_size_pt,
        &config.theme_mode,
        copy_location,
        &config.identity_model_path,
        &config.identity_detector_path,
        &config.landmark_model_path,
    )
}

/// Persist the chosen GUI theme mode ("paper" | "ink"), preserving the others.
pub fn save_theme_mode(config: &AppConfig, theme_mode: &str) -> std::io::Result<()> {
    write_settings_file(
        &config_file_path(config),
        &config.workspace_root,
        config.ingest_in_place_default,
        config.max_debug_events,
        config.font_size_pt,
        theme_mode,
        &config.copy_location,
        &config.identity_model_path,
        &config.identity_detector_path,
        &config.landmark_model_path,
    )
}

/// Persist the identity model and detector paths, preserving the other fields.
pub fn save_identity_paths(
    config: &AppConfig,
    identity_model_path: &Option<PathBuf>,
    identity_detector_path: &Option<PathBuf>,
) -> std::io::Result<()> {
    write_settings_file(
        &config_file_path(config),
        &config.workspace_root,
        config.ingest_in_place_default,
        config.max_debug_events,
        config.font_size_pt,
        &config.theme_mode,
        &config.copy_location,
        identity_model_path,
        identity_detector_path,
        &config.landmark_model_path,
    )
}

/// Persist the selected runtime workspace root, preserving the other fields.
pub fn save_workspace_root(config: &AppConfig, workspace_root: &Path) -> std::io::Result<()> {
    write_settings_file(
        &config_file_path(config),
        workspace_root,
        config.ingest_in_place_default,
        config.max_debug_events,
        config.font_size_pt,
        &config.theme_mode,
        &config.copy_location,
        &config.identity_model_path,
        &config.identity_detector_path,
        &config.landmark_model_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "facial_config_{label}_{}",
            Uuid::new_v4().to_string().replace('-', "_")
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_repo(root: &Path) {
        fs::create_dir_all(root.join("product").join("config")).unwrap();
        fs::create_dir_all(root.join("product").join("plugins")).unwrap();
        fs::write(
            root.join("product").join("config").join("default.json"),
            "{}\n",
        )
        .unwrap();
    }

    #[test]
    fn workspace_root_env_moves_runtime_state_out_of_repo_root() {
        let _guard = env_lock().lock().unwrap();
        let repo = temp_root("repo");
        let workspace = temp_root("workspace");
        make_repo(&repo);

        env::set_var("FACIAL_REPO_ROOT", &repo);
        env::set_var("FACIAL_WORKSPACE_ROOT", &workspace);
        env::remove_var("FACIAL_DATA_ROOT");
        env::remove_var("FACIAL_WORKTREES_ROOT");

        let config = load_config();

        assert_eq!(config.repo_root, repo);
        assert_eq!(config.workspace_root, workspace);
        assert_eq!(
            config.worktrees_root,
            workspace.join(".facial").join("worktrees")
        );
        assert_eq!(
            config.model_registry_path,
            workspace
                .join(".facial")
                .join("data")
                .join("model_registry.json")
        );
        assert_eq!(
            config.api_root,
            workspace.join(".facial").join("data").join("api")
        );

        env::remove_var("FACIAL_REPO_ROOT");
        env::remove_var("FACIAL_WORKSPACE_ROOT");
    }

    #[test]
    fn config_path_env_redirects_settings_read_and_write() {
        let _guard = env_lock().lock().unwrap();
        let repo = temp_root("repo_cfg");
        make_repo(&repo);
        let user_dir = temp_root("userdata");
        let user_settings = user_dir.join("config").join("default.json"); // parent not pre-created

        env::set_var("FACIAL_REPO_ROOT", &repo);
        env::set_var("FACIAL_CONFIG_PATH", &user_settings);
        env::remove_var("FACIAL_WORKSPACE_ROOT");
        env::remove_var("FACIAL_DATA_ROOT");
        env::remove_var("FACIAL_WORKTREES_ROOT");

        // Fresh install: the user settings file does not exist yet, so the settings path
        // still resolves to FACIAL_CONFIG_PATH (values are seeded from the install default).
        let config = load_config();
        assert_eq!(config_file_path(&config), user_settings);

        // A save must create the missing parent dir and write to the user path...
        save_theme_mode(&config, "ink").unwrap();
        assert!(
            user_settings.exists(),
            "settings should be written to FACIAL_CONFIG_PATH"
        );
        // ...and must NOT touch the read-only install default.
        let repo_default = repo.join("product").join("config").join("default.json");
        assert_eq!(
            fs::read_to_string(&repo_default).unwrap().trim(),
            "{}",
            "the install default must stay untouched"
        );

        env::remove_var("FACIAL_REPO_ROOT");
        env::remove_var("FACIAL_CONFIG_PATH");
    }

    #[test]
    fn installed_layout_uses_local_app_data_without_launcher_environment() {
        let _guard = env_lock().lock().unwrap();
        let install = temp_root("installed");
        let local = temp_root("localappdata");
        let prior_repo_root = env::var_os("FACIAL_REPO_ROOT");
        let prior_local_app_data = env::var_os("LOCALAPPDATA");
        fs::create_dir_all(install.join("product").join("config")).unwrap();
        fs::write(
            install.join("product").join("config").join("default.json"),
            r#"{"workspace_root":"D:\\developer-only"}"#,
        )
        .unwrap();

        env::set_var("FACIAL_REPO_ROOT", &install);
        env::set_var("LOCALAPPDATA", &local);
        env::remove_var("FACIAL_CONFIG_PATH");
        env::remove_var("FACIAL_WORKSPACE_ROOT");
        env::remove_var("FACIAL_DATA_ROOT");
        env::remove_var("FACIAL_WORKTREES_ROOT");

        let config = load_config();
        let expected_data = local.join("Facial");
        assert_eq!(config.repo_root, install);
        assert_eq!(config.workspace_root, expected_data);
        assert_eq!(
            config_file_path(&config),
            expected_data.join("config").join("default.json")
        );

        match prior_repo_root {
            Some(value) => env::set_var("FACIAL_REPO_ROOT", value),
            None => env::remove_var("FACIAL_REPO_ROOT"),
        }
        match prior_local_app_data {
            Some(value) => env::set_var("LOCALAPPDATA", value),
            None => env::remove_var("LOCALAPPDATA"),
        }
    }
}
