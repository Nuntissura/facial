use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::{
    debug::DebugBus,
    plugins::common::{
        analyze_images, emit_empty_warning, ofiq_dimensions, quality_score, write_feature_artifact,
        FeatureArtifactResult, ImageAnalysis,
    },
};

#[derive(Clone, Copy)]
struct ModelProfile {
    score_weight: f64,
    face_weight: f64,
    sharpness_weight: f64,
    exposure_weight: f64,
    noise_weight: f64,
    color_weight: f64,
    composition_weight: f64,
    resolution_weight: f64,
}

const MODEL_PROFILES: &[(&str, ModelProfile)] = &[
    (
        "model_t",
        ModelProfile {
            score_weight: 0.34,
            face_weight: 0.18,
            sharpness_weight: 0.14,
            exposure_weight: 0.09,
            noise_weight: 0.11,
            color_weight: 0.09,
            composition_weight: 0.07,
            resolution_weight: 0.08,
        },
    ),
    (
        "model_m",
        ModelProfile {
            score_weight: 0.26,
            face_weight: 0.22,
            sharpness_weight: 0.16,
            exposure_weight: 0.09,
            noise_weight: 0.13,
            color_weight: 0.11,
            composition_weight: 0.07,
            resolution_weight: 0.06,
        },
    ),
    (
        "model_s",
        ModelProfile {
            score_weight: 0.22,
            face_weight: 0.22,
            sharpness_weight: 0.19,
            exposure_weight: 0.12,
            noise_weight: 0.16,
            color_weight: 0.11,
            composition_weight: 0.05,
            resolution_weight: 0.03,
        },
    ),
    (
        "model_l",
        ModelProfile {
            score_weight: 0.18,
            face_weight: 0.24,
            sharpness_weight: 0.22,
            exposure_weight: 0.12,
            noise_weight: 0.18,
            color_weight: 0.09,
            composition_weight: 0.04,
            resolution_weight: 0.03,
        },
    ),
];

fn model_profile(feature_id: &str) -> Option<ModelProfile> {
    MODEL_PROFILES
        .iter()
        .find(|(id, _)| *id == feature_id)
        .map(|(_, profile)| *profile)
}

fn profile_signal(item: &ImageAnalysis) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    let base = quality_score(item);
    let face = (item.face_confidence * 100.0).clamp(0.0, 100.0);
    let sharpness = item.sharpness.clamp(0.0, 100.0);
    let exposure = item.exposure.clamp(0.0, 100.0);
    let noise = (100.0 - item.noise_estimate).clamp(0.0, 100.0);
    let color = item.colorfulness.clamp(0.0, 100.0);
    let composition = item.composition.clamp(0.0, 100.0);
    let resolution = (((item.width.max(1) as f64).ln() + (item.height.max(1) as f64).ln()) / 16.0
        * 100.0)
        .clamp(0.0, 100.0);
    (
        base,
        face,
        sharpness,
        exposure,
        noise,
        color,
        composition,
        resolution,
    )
}

fn score_item(item: &ImageAnalysis, profile: ModelProfile) -> f64 {
    let (base, face, sharpness, exposure, noise, color, composition, resolution) =
        profile_signal(item);
    let base_signal = base * profile.score_weight;
    let face_signal = face * profile.face_weight;
    let sharp_signal = sharpness * profile.sharpness_weight;
    let exposure_signal = exposure * profile.exposure_weight;
    let noise_signal = noise * profile.noise_weight;
    let color_signal = color * profile.color_weight;
    let composition_signal = composition * profile.composition_weight;
    let resolution_signal = resolution * profile.resolution_weight;

    // Optional face-specific boost: preserve the behavior of "face-first" eDiFFIQA variants while
    // still remaining deterministic and lightweight.
    //
    // NOTE: `item.eye_open` here is consumed as a coarse face-clarity proxy (amplified ~18x), not a
    // true eye-open / eyes-open detection measure. It biases the bonus toward clearer face regions;
    // do not interpret the resulting `face_bonus` as an eye-state signal.
    let face_bonus = if item.face_count > 0 {
        (item.eye_open * 18.0).clamp(0.0, 18.0)
    } else {
        0.0
    };

    (base_signal
        + face_signal
        + sharp_signal
        + exposure_signal
        + noise_signal
        + color_signal
        + composition_signal
        + resolution_signal
        + face_bonus)
        .clamp(0.0, 100.0)
}

fn dimension_payload(item: &ImageAnalysis) -> Vec<Value> {
    ofiq_dimensions(item)
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect()
}

fn model_row(item: &ImageAnalysis, feature_id: &str) -> Value {
    let profile = model_profile(feature_id).expect("model exists");
    let score = score_item(item, profile);
    let (base, face, sharpness, exposure, noise, color, composition, resolution) =
        profile_signal(item);
    let (quality_delta, best_variant) = best_variant(item);
    let fail_fast = if item.face_count == 0 {
        "no_face_detected"
    } else {
        "ok"
    };
    json!({
        "path": item.path,
        "model": feature_id,
        "score": score,
        "score_components": {
            "base_quality": base,
            "face": face,
            "sharpness": sharpness,
            "exposure": exposure,
            "noise_guard": noise,
            "colorfulness": color,
            "composition": composition,
            "resolution": resolution,
            "face_bonus": if item.face_count > 0 { (item.eye_open * 18.0).clamp(0.0, 18.0)} else {0.0},
        },
        "dims": dimension_payload(item),
        "pass_quality": quality_score(item).clamp(0.0, 100.0),
        "face_count": item.face_count,
        "face_confidence": item.face_confidence,
        "eye_open": item.eye_open,
        "quality_delta": quality_delta,
        "best_model_for_image": best_variant,
        "status": fail_fast,
    })
}

fn best_variant(item: &ImageAnalysis) -> (f64, String) {
    let mut best_score = -1.0f64;
    let mut best_model = String::from("model_t");
    for (model_id, profile) in MODEL_PROFILES {
        let score = score_item(item, *profile);
        if score > best_score {
            best_score = score;
            best_model = model_id.to_string();
        }
    }
    (best_score, best_model)
}

fn batch_rows(items: &[ImageAnalysis]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut scores = serde_json::Map::new();
            for (model_id, profile) in MODEL_PROFILES {
                scores.insert((*model_id).to_string(), json!(score_item(item, *profile)));
            }
            let row = model_row(item, "model_s");
            let row_obj = row.as_object().cloned().unwrap_or_default();
            let mut output = serde_json::Map::new();
            for (key, value) in row_obj {
                output.insert(key, value);
            }
            for (key, value) in scores.iter() {
                output.insert(format!("score_{key}"), value.clone());
            }
            output.insert("variant_scores".to_string(), Value::Object(scores));
            serde_json::Value::Object(output)
        })
        .collect()
}

fn batch_summary(items: &[ImageAnalysis], by_model: &HashMap<String, Vec<f64>>) -> Value {
    let mut model_summary = HashMap::new();
    for (model_id, scores) in by_model {
        if scores.is_empty() {
            continue;
        }
        let sum: f64 = scores.iter().sum();
        let avg = sum / scores.len() as f64;
        let min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        model_summary.insert(
            model_id.clone(),
            json!({
                "count": scores.len(),
                "avg": avg,
                "min": min,
                "max": max,
                "median": 0.0,
            }),
        );
    }

    let mut model_scores = Vec::new();
    for (model_id, scores) in by_model {
        if scores.is_empty() {
            continue;
        }
        let sorted = {
            let mut sorted = scores.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sorted
        };
        let median = sorted[sorted.len() / 2];
        if let Some(summary) = model_summary.get_mut(model_id) {
            if let Some(obj) = summary.as_object_mut() {
                if let Some(median_obj) = obj.get_mut("median") {
                    *median_obj = json!(median);
                }
            }
        }
        model_scores.push(json!({
            "model": model_id,
            "summary": model_summary.get(model_id).cloned(),
            "median": median,
        }));
    }
    json!({
        "images": items.len(),
        "model_count": by_model.len(),
        "models": model_scores,
    })
}

pub fn run_feature(
    feature_id: &str,
    image_paths: &[String],
    run_root: &Path,
    _run_id: &str,
    debug: &mut DebugBus,
    _manifest_id: &str,
) -> FeatureArtifactResult {
    let items = analyze_images(image_paths, Some(debug));
    if items.is_empty() {
        emit_empty_warning(debug, "ediffiqa", feature_id);
        return FeatureArtifactResult::empty(feature_id);
    }

    if let Some(profile) = model_profile(feature_id) {
        let rows = items
            .iter()
            .map(|item| model_row(item, feature_id))
            .collect::<Vec<_>>();
        let payload = json!({
            "feature": feature_id,
                "source": "proxy",
            "count": rows.len(),
            "model": feature_id,
            "model_profile": {
                "score_weight": profile.score_weight,
                "face_weight": profile.face_weight,
                "sharpness_weight": profile.sharpness_weight,
                "exposure_weight": profile.exposure_weight,
                "noise_weight": profile.noise_weight,
                "color_weight": profile.color_weight,
                "composition_weight": profile.composition_weight,
                "resolution_weight": profile.resolution_weight,
            },
            "items": rows,
        });
        let artifact = write_feature_artifact(run_root, &format!("{feature_id}.json"), &payload);
        return FeatureArtifactResult {
            status: "ok".to_string(),
            message: format!("{feature_id} complete"),
            payload,
            artifacts: vec![artifact],
        };
    }

    match feature_id {
        "batch_inference" => {
            let mut by_model: HashMap<String, Vec<f64>> = HashMap::new();
            for (model_id, profile) in MODEL_PROFILES {
                let mut scores = Vec::new();
                for item in &items {
                    scores.push(score_item(item, *profile));
                }
                by_model.insert((*model_id).to_string(), scores);
            }
            let matrix = batch_rows(&items);
            let mut best_rows = Vec::new();
            let mut model_winner_counts: HashMap<String, usize> = HashMap::new();
            let mut best_scores = Vec::new();
            for row in &matrix {
                let maybe_scores = row["variant_scores"].as_object();
                let mut winner = String::from("model_t");
                let mut winner_score = -1.0f64;
                if let Some(map) = maybe_scores {
                    for (model_id, score) in map {
                        let value = score.as_f64().unwrap_or(0.0);
                        if value > winner_score {
                            winner_score = value;
                            winner = model_id.clone();
                        }
                    }
                    *model_winner_counts.entry(winner.clone()).or_insert(0) += 1;
                    best_scores.push(winner_score);
                }
                if let Some(obj) = row.as_object() {
                    let mut enriched = obj.clone();
                    enriched.insert("winning_model".to_string(), json!(winner));
                    enriched.insert("winning_score".to_string(), json!(winner_score));
                    best_rows.push(serde_json::Value::Object(enriched));
                }
            }
            let best_stats = if best_scores.is_empty() {
                json!({"count": 0})
            } else {
                let sorted = {
                    let mut scores = best_scores.clone();
                    scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    scores
                };
                let min = *sorted.first().unwrap_or(&0.0);
                let max = *sorted.last().unwrap_or(&0.0);
                let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
                json!({
                    "count": sorted.len(),
                    "min": min,
                    "max": max,
                    "avg": avg,
                    "median": sorted[sorted.len() / 2],
                })
            };

            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": matrix.len(),
                "matrix": best_rows,
                "model_summary": batch_summary(&items, &by_model),
                "winner_counts": model_winner_counts,
                "winner_score_stats": best_stats,
            });
            let artifact = write_feature_artifact(run_root, "batch_inference.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("batch inference complete for {} images", matrix.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        _ => FeatureArtifactResult::unsupported(feature_id),
    }
}
