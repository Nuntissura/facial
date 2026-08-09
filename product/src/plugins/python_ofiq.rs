use std::path::Path;

use serde_json::json;

use crate::{
    debug::DebugBus,
    plugins::common::{
        analyze_images, emit_empty_warning, ofiq_dimensions, quality_score, write_feature_artifact,
        FaceRegion, FeatureArtifactResult, ImageAnalysis,
    },
};

const QUALITY_BANDS: &[(f64, &str)] = &[
    (80.0, "excellent"),
    (65.0, "good"),
    (50.0, "usable"),
    (35.0, "weak"),
    (0.0, "reject"),
];

fn quality_band(score: f64) -> &'static str {
    for &(min, label) in QUALITY_BANDS {
        if score >= min {
            return label;
        }
    }
    "reject"
}

fn stable_dimensions(item: &ImageAnalysis) -> (Vec<String>, Vec<f64>) {
    let mut pairs = ofiq_dimensions(item)
        .into_iter()
        .collect::<Vec<(String, f64)>>();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let names = pairs.iter().map(|entry| entry.0.clone()).collect();
    let values = pairs.iter().map(|entry| entry.1).collect();
    (names, values)
}

fn vector_items(items: &[ImageAnalysis]) -> serde_json::Value {
    let mut out = Vec::new();
    let mut max_dimension_count: usize = 0;
    let mut schema_dimensions: Vec<String> = Vec::new();

    for item in items {
        let scalar_quality = quality_score(item);
        let (dimension_names, dimension_values) = stable_dimensions(item);
        let mut vector = Vec::new();
        let mut min_value = f64::INFINITY;
        let mut max_value = f64::NEG_INFINITY;
        let mut total = 0.0;

        for (idx, value) in dimension_values.iter().enumerate() {
            vector.push(json!({
                "index": idx,
                "name": dimension_names[idx].clone(),
                "value": value,
                "unit": "score_0_100",
            }));
            min_value = min_value.min(*value);
            max_value = max_value.max(*value);
            total += *value;
        }

        let count = dimension_values.len() as f64;
        let mean = if count > 0.0 { total / count } else { 0.0 };
        let quality_gap = (scalar_quality - mean).abs();

        max_dimension_count = max_dimension_count.max(dimension_names.len());
        if schema_dimensions.is_empty() {
            schema_dimensions = dimension_names.clone();
        }

        out.push(json!({
            "path": item.path,
            "scalar_quality": scalar_quality,
            "quality_band": quality_band(scalar_quality),
            "quality_vector": dimension_values,
            "dimensions": dimension_names,
            "quality_gap_vs_dimension_mean": quality_gap,
            "quality_band_summary": {
                "min": min_value,
                "max": max_value,
                "mean": mean,
                "count": dimension_values.len(),
            },
            "metadata": {
                "face_count": item.face_count,
                "file_size": item.file_size,
                "width": item.width,
                "height": item.height,
                "vector_norm": (dimension_values.iter().map(|value| value * value).sum::<f64>()).sqrt(),
            },
            "vector_cells": vector,
        }));
    }

    json!({
        "status": "ok",
        "count": out.len(),
        "items": out,
        "schema": {
            "version": "0.2-native",
            "dimension_count": max_dimension_count,
            "dimensions": schema_dimensions,
        }
    })
}

/// Builds a zeroed `ImageAnalysis` so the advertised dimension list can be
/// sourced from the same `ofiq_dimensions()` path the real `vector_quality`
/// feature uses. This keeps the advertised schema in lockstep with the
/// emitted vector instead of duplicating a hand-maintained name list.
fn probe_analysis() -> ImageAnalysis {
    ImageAnalysis {
        path: String::new(),
        width: 0,
        height: 0,
        file_size: 0,
        mean_luma: 0.0,
        luma_std: 0.0,
        median_luma: 0,
        contrast: 0.0,
        entropy: 0.0,
        sharpness: 0.0,
        colorfulness: 0.0,
        dynamic_range: 0.0,
        noise_estimate: 0.0,
        exposure: 0.0,
        composition: 0.0,
        skin_ratio: 0.0,
        center_bias: 0.0,
        face_count: 0,
        face_confidence: 0.0,
        eye_open: 0.0,
        face_region: None::<FaceRegion>,
        ahash: 0,
        dhash: 0,
        phash: 0,
        sha256: String::new(),
        capture_unix_ms: None,
        burst_key: String::new(),
        embedding: Vec::new(),
    }
}

/// Advertised quality-dimension names, sourced from `ofiq_dimensions()` so the
/// setup report cannot drift from the actual emitted vector schema.
fn advertised_dimensions() -> Vec<String> {
    let mut names: Vec<String> = ofiq_dimensions(&probe_analysis()).into_keys().collect();
    names.sort();
    names
}

/// Genuine, dependency-free capability check for the scoring pipeline.
/// Returns `(image_decode_ok, output_writable, detail)`.
fn probe_capabilities(run_root: &Path) -> (bool, bool, serde_json::Value) {
    // 1x1 PNG (smallest valid PNG) decoded in-memory to verify the image
    // crate can decode the buffers the score pipeline depends on.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let decode = image::load_from_memory(TINY_PNG);
    let (image_decode_ok, decode_detail) = match &decode {
        Ok(img) => {
            let (w, h) = (img.width(), img.height());
            (
                true,
                json!({"ok": true, "decoded_width": w, "decoded_height": h}),
            )
        }
        Err(err) => (false, json!({"ok": false, "error": err.to_string()})),
    };

    // Verify the configured output dir is writable by round-tripping a probe file.
    let _ = std::fs::create_dir_all(run_root);
    let probe_path = run_root.join(".python_ofiq_setup_probe.tmp");
    let write_result = std::fs::write(&probe_path, b"probe");
    let output_writable = write_result.is_ok();
    let write_detail = match &write_result {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe_path);
            json!({"ok": true, "path": probe_path.to_string_lossy()})
        }
        Err(err) => {
            json!({"ok": false, "path": probe_path.to_string_lossy(), "error": err.to_string()})
        }
    };

    let detail = json!({
        "image_decode": decode_detail,
        "output_writable": write_detail,
    });
    (image_decode_ok, output_writable, detail)
}

fn setup_payload(run_root: &Path) -> serde_json::Value {
    let dimensions = advertised_dimensions();
    let (image_decode_ok, output_writable, checks) = probe_capabilities(run_root);
    let status = if image_decode_ok && output_writable {
        "ready"
    } else {
        "degraded"
    };
    json!({
        "feature": "setup_data",
        "status": status,
        "engine": "rust_native_ofiq_rewrite",
        "version": "native-ofiq-v2",
        "dimensions": dimensions,
        "dimension_count": dimensions.len(),
        "score_0_100": true,
        "checks": checks,
        "thresholds": {
            "scalar_quality_headshot_min": 68.0,
            "vector_quality_gap_tolerance": 25.0,
            "quality_score_range": [0.0, 100.0],
        },
        "notes": "Face-oriented scoring derived from native image analysis; status reflects a live image-decode + output-writability probe",
    })
}

pub fn run_feature(
    feature_id: &str,
    image_paths: &[String],
    run_root: &std::path::Path,
    _run_id: &str,
    debug: &mut DebugBus,
    manifest_id: &str,
) -> FeatureArtifactResult {
    let items = analyze_images(image_paths, Some(debug));
    if items.is_empty() {
        emit_empty_warning(debug, manifest_id, feature_id);
        return FeatureArtifactResult::empty(feature_id);
    }

    match feature_id {
        "setup_data" => {
            let payload = setup_payload(run_root);
            let artifact = write_feature_artifact(run_root, "setup_data.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: "setup_data completed".to_string(),
                payload,
                artifacts: vec![artifact],
            }
        }
        "scalar_quality" => {
            let mut rows = Vec::new();
            for item in items {
                let scalar_quality = quality_score(&item);
                let dims = ofiq_dimensions(&item);
                let dimension_sum: f64 = dims.values().sum();
                rows.push(json!({
                    "path": item.path,
                    "scalar_quality": scalar_quality,
                    "quality_band": quality_band(scalar_quality),
                    "dimension_sum": dimension_sum,
                    "dimension_count": dims.len(),
                    "face_count": item.face_count,
                    "face_confidence": item.face_confidence,
                    "face_eye_open": item.eye_open,
                    "face_region": item.face_region,
                    "source": "python_ofiq_native",
                }));
            }
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
                "range": [0.0, 100.0],
                "thresholds": {
                    "headshot_min": 68.0,
                },
            });
            let artifact = write_feature_artifact(run_root, "scalar_quality.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!(
                    "scalar quality on {} images",
                    payload["count"].as_u64().unwrap_or(0)
                ),
                payload,
                artifacts: vec![artifact],
            }
        }
        "vector_quality" => {
            let data = vector_items(&items);
            let mut summary_count = 0.0;
            let mut summary_min = f64::INFINITY;
            let mut summary_max = f64::NEG_INFINITY;
            let mut summary_sum = 0.0;

            if let Some(rows) = data["items"].as_array() {
                for row in rows {
                    if let Some(vector) = row["quality_vector"].as_array() {
                        for value in vector {
                            if let Some(number) = value.as_f64() {
                                summary_min = summary_min.min(number);
                                summary_max = summary_max.max(number);
                                summary_sum += number;
                                summary_count += 1.0;
                            }
                        }
                    }
                }
            }

            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "status": "ok",
                "count": data["count"].as_u64().unwrap_or(0),
                "items": data["items"].clone(),
                "vector_schema": data["schema"].clone(),
                "summary": {
                    "dimension_count": data["schema"]["dimension_count"].as_u64().unwrap_or(0),
                    "global_min": if summary_count > 0.0 { summary_min } else { 0.0 },
                    "global_max": if summary_count > 0.0 { summary_max } else { 0.0 },
                    "global_mean": if summary_count > 0.0 { summary_sum / summary_count } else { 0.0 },
                    "global_values": summary_count as u64,
                },
            });
            let artifact = write_feature_artifact(run_root, "vector_quality.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!(
                    "vector quality on {} images",
                    payload["count"].as_u64().unwrap_or(0)
                ),
                payload,
                artifacts: vec![artifact],
            }
        }
        _ => FeatureArtifactResult::unsupported(feature_id),
    }
}
