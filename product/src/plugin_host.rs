use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use walkdir::WalkDir;

use crate::plugins::{
    common::FeatureArtifactResult, deepface, ediffiqa, facet, imagededup, python_ofiq,
};
use crate::{config::AppConfig, debug::DebugBus, models::PluginRunResult};

#[derive(Clone, Serialize, Deserialize)]
pub struct PluginFeature {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub output_kind: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub package: String,
    pub adapter: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub features: Vec<PluginFeature>,
}

pub struct PluginHost {
    config: AppConfig,
    manifests: Vec<PluginManifest>,
}

impl PluginHost {
    pub fn new(config: &AppConfig) -> Self {
        let mut host = Self {
            config: config.clone(),
            manifests: Vec::new(),
        };
        host.refresh();
        host
    }

    pub fn refresh(&mut self) {
        self.manifests = self.load_manifests();
    }

    pub fn list_plugins(&self) -> Vec<PluginManifest> {
        self.manifests.clone()
    }

    pub fn run_feature(
        &self,
        plugin_id: &str,
        feature_id: &str,
        image_paths: &[String],
        run_root: &Path,
        run_id: &str,
        debug: &mut DebugBus,
    ) -> PluginRunResult {
        let manifest = match self.manifests.iter().find(|item| item.id == plugin_id) {
            Some(item) => item.clone(),
            None => {
                let plugins_root = self.config.plugins_root.display().to_string();
                let loaded_ids: Vec<String> =
                    self.manifests.iter().map(|item| item.id.clone()).collect();
                return PluginRunResult {
                    plugin_id: plugin_id.to_string(),
                    feature_id: feature_id.to_string(),
                    status: "failed".to_string(),
                    message: format!(
                        "unknown plugin: {plugin_id} (plugins_root: {plugins_root}; loaded plugins: [{}])",
                        loaded_ids.join(", ")
                    ),
                    payload: json!({
                        "status": "failed",
                        "plugin_id": plugin_id,
                        "plugins_root": plugins_root,
                        "loaded_plugins": loaded_ids,
                    }),
                    artifacts: Vec::new(),
                };
            }
        };

        if !manifest
            .features
            .iter()
            .any(|feature| feature.id == feature_id)
        {
            return PluginRunResult {
                plugin_id: plugin_id.to_string(),
                feature_id: feature_id.to_string(),
                status: "failed".to_string(),
                message: format!("feature not found for {plugin_id}: {feature_id}"),
                payload: json!({"status":"failed"}),
                artifacts: Vec::new(),
            };
        }

        let _ = std::fs::create_dir_all(run_root);

        let result: FeatureArtifactResult = match manifest.id.as_str() {
            "facet" => facet::run_feature(
                feature_id,
                image_paths,
                run_root,
                run_id,
                debug,
                &manifest.id,
            ),
            "python-ofiq" => python_ofiq::run_feature(
                feature_id,
                image_paths,
                run_root,
                run_id,
                debug,
                &manifest.id,
            ),
            "deepface" => deepface::run_feature(
                feature_id,
                image_paths,
                run_root,
                run_id,
                debug,
                &manifest.id,
            ),
            "imagededup" => imagededup::run_feature(
                feature_id,
                image_paths,
                run_root,
                run_id,
                debug,
                &manifest.id,
            ),
            "ediffiqa" => ediffiqa::run_feature(
                feature_id,
                image_paths,
                run_root,
                run_id,
                debug,
                &manifest.id,
            ),
            other => {
                debug.emit(
                    "WARN",
                    "plugin_host",
                    &format!("plugin executor not implemented: {other}"),
                    None,
                );
                FeatureArtifactResult {
                    status: "failed".to_string(),
                    message: format!("no native executor for {other}"),
                    payload: json!({"status":"failed","feature":feature_id,"plugin":other}),
                    artifacts: Vec::new(),
                }
            }
        };

        let mut payload = result.payload;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("plugin_id".to_string(), json!(manifest.id.clone()));
            obj.insert("run_id".to_string(), json!(run_id));
            obj.insert("source".to_string(), json!(manifest.id.clone()));
        }

        PluginRunResult {
            plugin_id: manifest.id,
            feature_id: feature_id.to_string(),
            status: result.status,
            message: result.message,
            payload,
            artifacts: result.artifacts,
        }
    }

    fn load_manifests(&self) -> Vec<PluginManifest> {
        if !self.config.plugins_root.exists() {
            eprintln!(
                "WARN plugin_host: plugins_root does not exist: {}",
                self.config.plugins_root.display()
            );
            return Vec::new();
        }
        let mut out = Vec::new();
        for entry in WalkDir::new(&self.config.plugins_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_name().to_string_lossy() != "metadata.json" {
                continue;
            }
            match std::fs::read_to_string(entry.path()) {
                Ok(raw) => match serde_json::from_str::<PluginManifest>(&raw) {
                    Ok(manifest) => out.push(manifest),
                    Err(err) => {
                        eprintln!(
                            "WARN plugin_host: malformed plugin manifest {}: {err}",
                            entry.path().display()
                        );
                    }
                },
                Err(err) => {
                    eprintln!(
                        "WARN plugin_host: unable to read plugin manifest {}: {err}",
                        entry.path().display()
                    );
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}
