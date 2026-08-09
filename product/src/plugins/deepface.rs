use serde_json::json;

use crate::{
    debug::DebugBus,
    plugins::common::{
        analyze_images, cosine_similarity, emit_empty_warning, ofiq_dimensions, quality_score,
        write_feature_artifact, FeatureArtifactResult, ImageAnalysis,
    },
};

const FIND_TOP_K: usize = 5;
const FIND_THRESHOLD: f64 = 0.84;
const VERIFY_THRESHOLD: f64 = 0.86;
const VERIFY_SOFT_THRESHOLD: f64 = 0.76;

fn identity_decision(similarity: f64) -> &'static str {
    if similarity >= VERIFY_THRESHOLD {
        "match"
    } else if similarity >= VERIFY_SOFT_THRESHOLD {
        "possible_match"
    } else {
        "nomatch"
    }
}

fn quality_band(score: f64) -> &'static str {
    if score >= 82.0 {
        "excellent"
    } else if score >= 65.0 {
        "good"
    } else if score >= 50.0 {
        "usable"
    } else if score >= 35.0 {
        "weak"
    } else {
        "reject"
    }
}

fn detect_rows(items: &[ImageAnalysis]) -> Vec<serde_json::Value> {
    // NOTE: "detection" here is a skin-color heuristic (common.rs face_probe):
    // any warm/skin-toned region yields face_count = 1, while greyscale images or
    // atypical skin tones yield 0. This is NOT a real face detector. It produces
    // false positives on warm non-face regions (e.g. wood, sunsets, hands) and
    // false negatives on faces whose tone falls outside the heuristic's warm-color
    // band. The emitted `skin_region_proxy` field names this honestly.
    items
        .iter()
        .map(|item| {
            let has_region = item.face_region.is_some();
            let det_score = (item.face_confidence * 100.0).clamp(0.0, 100.0);
            let passed = det_score >= 25.0;
            let face_quality = quality_score(item);
            json!({
                "path": item.path,
                "source": "proxy",
                "skin_region_proxy": item.face_count,
                "face_confidence": item.face_confidence,
                "detection_score": det_score,
                "passed": passed,
                "quality_band": quality_band(face_quality),
                "face_quality": face_quality,
                "regions": item.face_region.as_ref().map_or_else(Vec::<serde_json::Value>::new, |region| vec![json!({
                    "x": region.x,
                    "y": region.y,
                    "width": region.width,
                    "height": region.height,
                    "score": region.score,
                    "area": (region.width as f64) * (region.height as f64),
                    "has_region": has_region,
                })]),
            })
        })
        .collect()
}

fn analyze_rows(items: &[ImageAnalysis]) -> Vec<serde_json::Value> {
    // NOTE: This feature does NOT predict age, gender, or emotion. Those were
    // fabricated from arbitrary arithmetic over pixel statistics (encoding only
    // lighting/skin-tone bias) and have been removed. Instead we report the
    // honest, measured pixel statistics actually computed for each image/region,
    // each clearly named as a measured proxy.
    let mut rows = Vec::new();
    for item in items {
        let dims = ofiq_dimensions(item);
        let quality = quality_score(item);
        rows.push(json!({
            "path": item.path,
            "source": "proxy",
            "mean_luma_proxy": item.mean_luma,
            "luma_std_proxy": item.luma_std,
            "contrast_proxy": item.contrast,
            "exposure_proxy": item.exposure,
            "noise_proxy": item.noise_estimate,
            "skin_ratio_proxy": item.skin_ratio,
            "eye_open_proxy": item.eye_open,
            "face_confidence": item.face_confidence,
            "skin_region_proxy": item.face_count,
            "face_quality": quality,
            "quality_band": quality_band(quality),
            "region": item.face_region,
            "vector_hint": {
                "face_quality": dims.get("face_confidence").copied().unwrap_or(0.0),
                "eye_open": item.eye_open,
                "center_bias": dims.get("center_bias").copied().unwrap_or(0.0),
            },
        }));
    }
    rows
}

fn represent_rows(items: &[ImageAnalysis]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|item| {
            let embedding = &item.embedding;
            let norm = (embedding.iter().map(|value| value * value).sum::<f64>()).sqrt();
            let unit_embedding: Vec<f64> = embedding
                .iter()
                .map(|value| value / norm.max(1e-12))
                .collect();
            let head: Vec<f64> = unit_embedding.iter().copied().take(12).collect();
            let max_component = unit_embedding
                .iter()
                .copied()
                .reduce(f64::max)
                .unwrap_or(0.0);
            let min_component = unit_embedding
                .iter()
                .copied()
                .reduce(f64::min)
                .unwrap_or(0.0);
            json!({
                "id": item.sha256,
                "path": item.path,
                "embedding_dim": embedding.len(),
                "embedding_sum": embedding.iter().copied().sum::<f64>(),
                "embedding_norm": norm,
                "embedding_unit": {
                    "head": head,
                    "max_component": max_component,
                    "min_component": min_component,
                },
            })
        })
        .collect()
}

fn register_rows(items: &[ImageAnalysis]) -> serde_json::Value {
    let mut rows = Vec::new();
    for item in items {
        rows.push(json!({
            "id": item.sha256,
            "path": item.path,
            "quality_score": quality_score(item),
            "face_count": item.face_count,
            "face_confidence": item.face_confidence,
        }));
    }
    json!({
        "count": rows.len(),
        "index": rows,
        "index_size": rows.len(),
        "index_quality": {
            "avg_quality": if rows.is_empty() {
                0.0
            } else {
                rows.iter()
                    .filter_map(|entry| entry.get("quality_score").and_then(|value| value.as_f64()))
                    .sum::<f64>() / rows.len() as f64
            }
        },
    })
}

fn find_rows(items: &[ImageAnalysis], top_k: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for (index, query) in items.iter().enumerate() {
        let query_quality = quality_score(query);
        let mut ranked = Vec::new();
        for (candidate_idx, candidate) in items.iter().enumerate() {
            if index == candidate_idx {
                continue;
            }
            let score = cosine_similarity(&query.embedding, &candidate.embedding);
            ranked.push(json!({
                "path": candidate.path,
                "similarity": score,
                "similarity_percent": (score * 100.0).clamp(0.0, 100.0),
                "distance": (1.0 - score).abs(),
                "verified": score >= FIND_THRESHOLD,
                "quality": quality_score(candidate),
                "face_count": candidate.face_count,
            }));
        }
        ranked.sort_by(|a, b| {
            b["similarity"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["similarity"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut top = Vec::new();
        let mut scores = Vec::new();
        for candidate in ranked.iter().take(top_k) {
            scores.push(candidate["similarity"].as_f64().unwrap_or(0.0));
            top.push(candidate.clone());
        }
        let margin = if scores.len() >= 2 {
            (scores.first().copied().unwrap_or(0.0) - scores.get(1).copied().unwrap_or(0.0)).abs()
        } else {
            0.0
        };
        let best = scores.first().copied().unwrap_or(0.0);
        out.push(json!({
            "query": query.path,
            "query_quality": query_quality,
            "query_face_count": query.face_count,
            "candidates": top,
            "candidates_found": scores.len(),
            "best_similarity": best,
            "top_gap": margin,
            "threshold": FIND_THRESHOLD,
            "decision": if best >= FIND_THRESHOLD { "accepted" } else { "needs_review" },
        }));
    }
    out
}

fn verify_pairs(items: &[ImageAnalysis]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let similarity = cosine_similarity(&items[i].embedding, &items[j].embedding);
            let decision = identity_decision(similarity);
            let distance = (1.0 - similarity).abs();
            let quality_a = quality_score(&items[i]);
            let quality_b = quality_score(&items[j]);
            let face_delta = (items[i].face_count as f64 - items[j].face_count as f64).abs();
            out.push(json!({
                "a": items[i].path,
                "b": items[j].path,
                "similarity": similarity,
                "similarity_percent": (similarity * 100.0).clamp(0.0, 100.0),
                "distance": distance,
                "verified": similarity >= VERIFY_THRESHOLD,
                "decision": decision,
                "decision_confidence": if decision == "match" {
                    100.0
                } else if decision == "possible_match" {
                    70.0
                } else {
                    40.0
                },
                "quality_gap": (quality_a - quality_b).abs(),
                "quality_a": quality_a,
                "quality_b": quality_b,
                "face_count_delta": face_delta,
                "explain": if decision == "match" {
                    "similarity above strict threshold"
                } else if decision == "possible_match" {
                    "close similarity; keep in manual review set"
                } else {
                    "similarity below hard threshold"
                },
            }));
        }
    }
    out
}

pub fn run_feature(
    feature_id: &str,
    image_paths: &[String],
    run_root: &std::path::Path,
    _run_id: &str,
    debug: &mut DebugBus,
    _manifest_id: &str,
) -> FeatureArtifactResult {
    let items = analyze_images(image_paths, Some(debug));
    if items.is_empty() {
        emit_empty_warning(debug, "deepface", feature_id);
        return FeatureArtifactResult::empty(feature_id);
    }

    match feature_id {
        "detect" => {
            let rows = detect_rows(&items);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
                "policy": {
                    "minimum_detection_score": 25.0,
                    "quality_scale": "0-100",
                },
            });
            let artifact = write_feature_artifact(run_root, "detect.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("detect completed on {} images", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "analyze" => {
            let rows = analyze_rows(&items);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
                "policy": {
                    "returns": "measured image/region pixel statistics",
                    "no_identity_prediction": true,
                },
            });
            let artifact = write_feature_artifact(run_root, "analyze.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!(
                    "analyze completed on {} images",
                    payload["count"].as_u64().unwrap_or(0)
                ),
                payload,
                artifacts: vec![artifact],
            }
        }
        "represent" => {
            let rows = represent_rows(&items);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
                "notes": "Native embedding proxy derived from native image channels",
            });
            let artifact = write_feature_artifact(run_root, "represent.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("represent completed on {} images", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "register" => {
            let idx = register_rows(&items);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": idx["count"].as_u64().unwrap_or(0),
                "index": idx["index"].clone(),
                "index_size": idx["index_size"].as_u64().unwrap_or(0),
                "index_quality": idx["index_quality"].clone(),
            });
            let artifact = write_feature_artifact(run_root, "register.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: "register completed".to_string(),
                payload,
                artifacts: vec![artifact],
            }
        }
        "find" => {
            let rows = find_rows(&items, FIND_TOP_K);
            let mut accepted = 0_u64;
            for row in &rows {
                if row["decision"] == "accepted" {
                    accepted += 1;
                }
            }
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "rows": rows,
                "top_k": FIND_TOP_K,
                "accepted_queries": accepted,
                "threshold": FIND_THRESHOLD,
            });
            let artifact = write_feature_artifact(run_root, "find.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("find completed for {} queries", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "verify" => {
            let rows = verify_pairs(&items);
            let mut accepted = 0_u64;
            for row in &rows {
                if row["verified"].as_bool().unwrap_or(false) {
                    accepted += 1;
                }
            }
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "pairs": rows,
                "threshold": VERIFY_THRESHOLD,
                "soft_threshold": VERIFY_SOFT_THRESHOLD,
                "verified_pairs": accepted,
            });
            let artifact = write_feature_artifact(run_root, "verify.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("verify completed with {} pairs", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        _ => FeatureArtifactResult::unsupported(feature_id),
    }
}
