use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::{
    debug::DebugBus,
    plugins::common::{
        analyze_images, cosine_similarity, emit_empty_warning, hash_similarity, quality_score,
        write_feature_artifact, FeatureArtifactResult, ImageAnalysis,
    },
};

fn quality_item(item: &ImageAnalysis) -> serde_json::Value {
    let score = quality_score(item);
    json!({
        "path": item.path,
        "file_size": item.file_size,
        "width": item.width,
        "height": item.height,
        "quality": score,
        "technical_sharpness": item.sharpness,
        "eyes_sharpness": (item.eye_open * 100.0).clamp(0.0, 100.0),
        "exposure": item.exposure,
        "color_balance": item.colorfulness,
        "dynamic_range": item.dynamic_range,
        "noise_estimate": item.noise_estimate,
        "quality_band": if score >= 82.0 {
            "excellent"
        } else if score >= 68.0 {
            "good"
        } else if score >= 55.0 {
            "usable"
        } else if score >= 40.0 {
            "weak"
        } else {
            "reject"
        },
        "headshot_candidate": score >= 68.0,
        "source": "facet_native",
    })
}

fn composition_item(item: &ImageAnalysis) -> serde_json::Value {
    json!({
        "path": item.path,
        "composition_score": item.composition,
        "center_bias": item.center_bias,
        "entropy": item.entropy,
        "dynamic_range": item.dynamic_range,
        "noise": item.noise_estimate,
    })
}

fn face_item(item: &ImageAnalysis) -> serde_json::Value {
    json!({
        "path": item.path,
        "face_count": item.face_count,
        "face_confidence": item.face_confidence,
        "face_region": item.face_region,
        "eye_open": item.eye_open,
        "region_score": item.face_region.as_ref().map(|value| value.score).unwrap_or(0.0),
    })
}

fn duplicate_pass_payload(items: &[ImageAnalysis]) -> serde_json::Value {
    const MIN_GROUP_SIZE: usize = 2;
    const MIN_AVG_HASH_SIMILARITY: f64 = 98.0;

    let mut groups = HashMap::<String, Vec<&ImageAnalysis>>::new();
    for item in items {
        let key = format!(
            "{:016x}-{:016x}-{:016x}",
            item.ahash, item.dhash, item.phash
        );
        groups.entry(key).or_default().push(item);
    }
    let mut grouped = std::collections::BTreeMap::<String, serde_json::Value>::new();
    let mut total_members = 0_usize;

    for (key, members) in groups {
        if members.len() < MIN_GROUP_SIZE {
            continue;
        }
        let mut paths = Vec::new();
        let mut quality_sum = 0.0;
        let mut file_size_sum = 0_u64;
        for member in &members {
            paths.push(member.path.clone());
            quality_sum += quality_score(member);
            file_size_sum += member.file_size;
        }

        let avg_quality = if members.is_empty() {
            0.0
        } else {
            quality_sum / members.len() as f64
        };

        let mut sim_total = 0.0;
        let mut sim_count = 0.0;
        let mut min_similarity = 0.0;
        let mut max_similarity = 0.0;
        let mut first_pair = true;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let sim = hash_similarity(members[i], members[j]);
                sim_total += sim;
                sim_count += 1.0;
                if first_pair {
                    min_similarity = sim;
                    max_similarity = sim;
                    first_pair = false;
                } else {
                    min_similarity = min_similarity.min(sim);
                    max_similarity = max_similarity.max(sim);
                }
            }
        }
        let avg_similarity = if sim_count > 0.0 {
            sim_total / sim_count
        } else {
            0.0
        };
        let representative = members
            .iter()
            .max_by(|a, b| {
                quality_score(a)
                    .partial_cmp(&quality_score(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        b.file_size
                            .partial_cmp(&a.file_size)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .map(|value| value.path.clone())
            .unwrap_or_default();

        total_members += paths.len();

        let signature = &members[0];
        grouped.insert(
            key.clone(),
            json!({
                "group_key": key,
                "member_count": paths.len(),
                "member_files": paths,
                "avg_similarity": avg_similarity,
                "min_similarity": min_similarity,
                "max_similarity": max_similarity,
                "avg_quality": avg_quality,
                "total_file_size": file_size_sum,
                "representative": representative,
                "signature": {
                    "sha256": signature.sha256,
                    "ahash": signature.ahash,
                    "dhash": signature.dhash,
                    "phash": signature.phash,
                },
                "matches_threshold": avg_similarity >= MIN_AVG_HASH_SIMILARITY,
            }),
        );
    }

    json!({
        "count": grouped.len(),
        "total_members_in_duplicate_groups": total_members,
        "groups": grouped,
        "policy": {
            "min_group_size": MIN_GROUP_SIZE,
            "min_avg_hash_similarity": MIN_AVG_HASH_SIMILARITY,
            "similarity_units": "percent",
            "scoring_unit": "quality_score",
        },
    })
}

fn burst_blink_payload(items: &[ImageAnalysis]) -> serde_json::Value {
    const BLINK_CLOSED_THRESHOLD: f64 = 35.0;
    const MIN_BURST_BLOCK_SIZE: usize = 2;

    let mut by_burst: HashMap<String, Vec<&ImageAnalysis>> = HashMap::new();
    for item in items {
        by_burst
            .entry(item.burst_key.clone())
            .or_default()
            .push(item);
    }

    let mut output = Vec::new();
    for (key, candidates) in by_burst {
        if candidates.len() < MIN_BURST_BLOCK_SIZE {
            continue;
        }
        let mut sorted = candidates.clone();
        sorted.sort_by(|a, b| {
            b.capture_unix_ms
                .unwrap_or(0)
                .cmp(&a.capture_unix_ms.unwrap_or(0))
        });

        let closed_eyes = sorted
            .iter()
            .filter(|entry| entry.eye_open < BLINK_CLOSED_THRESHOLD)
            .count();
        let mut blink_ratio = 0.0;
        let mut sim_sum = 0.0;
        let mut sim_count = 0.0;

        for i in 0..sorted.len() {
            for j in (i + 1)..sorted.len() {
                let sim = cosine_similarity(&sorted[i].embedding, &sorted[j].embedding);
                sim_sum += sim;
                sim_count += 1.0;
            }
        }

        let average_burst_similarity = if sim_count > 0.0 {
            sim_sum / sim_count
        } else {
            0.0
        };
        if sim_count > 0.0 {
            blink_ratio = closed_eyes as f64 / sorted.len() as f64;
        }
        let keep = sorted
            .iter()
            .max_by(|a, b| {
                let a_score =
                    quality_score(a).clamp(0.0, 100.0) + (a.eye_open).clamp(0.0, 100.0) * 0.35;
                let b_score =
                    quality_score(b).clamp(0.0, 100.0) + (b.eye_open).clamp(0.0, 100.0) * 0.35;
                a_score
                    .partial_cmp(&b_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.path.cmp(&b.path))
            })
            .map(|value| value.path.clone())
            .unwrap_or_default();
        let mut items_payload = Vec::new();
        for item in sorted {
            items_payload.push(json!({
                "path": item.path,
                "face_count": item.face_count,
                "capture_unix_ms": item.capture_unix_ms,
                "eye_open": item.eye_open,
                "quality": quality_score(item),
                "blink_like": item.eye_open < BLINK_CLOSED_THRESHOLD,
                "headshot_ready": item.eye_open >= BLINK_CLOSED_THRESHOLD && quality_score(item) >= 55.0,
            }));
        }

        output.push(json!({
            "burst_key": key,
            "count": items_payload.len(),
            "blink_frames": closed_eyes,
            "blink_ratio": blink_ratio,
            "mean_embedding_similarity": average_burst_similarity,
            "recommended_keep": keep,
            "items": items_payload,
            "burst_contains_blink": closed_eyes > 0,
        }));
    }
    json!({
        "count": output.len(),
        "blocks": output,
        "policy": {
            "min_burst_size": MIN_BURST_BLOCK_SIZE,
            "blink_closed_threshold": BLINK_CLOSED_THRESHOLD,
            "sort_order": "capture_unix_ms_desc",
        },
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
        emit_empty_warning(debug, "facet", feature_id);
        return FeatureArtifactResult::empty(feature_id);
    }

    match feature_id {
        "quality_pass" => {
            let rows = items.iter().map(quality_item).collect::<Vec<_>>();
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
            });
            let artifact = write_feature_artifact(run_root, "quality_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("quality_pass computed on {} images", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "composition_pass" => {
            let rows = items.iter().map(composition_item).collect::<Vec<_>>();
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "items": rows,
            });
            let artifact = write_feature_artifact(run_root, "composition_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("composition_pass computed on {} images", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "faces_pass" => {
            let rows = items.iter().map(face_item).collect::<Vec<_>>();
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "total_faces": items.iter().map(|item| item.face_count).sum::<u32>(),
                "items": rows,
            });
            let artifact = write_feature_artifact(run_root, "faces_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("faces_pass completed on {} images", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "duplicate_pass" => {
            let dup = duplicate_pass_payload(&items);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "method": "sha256+ahash+dhash+phash+pair-sim",
                "count": dup["count"].as_u64().unwrap_or(0),
                "policy": dup["policy"].clone(),
                "groups": dup["groups"].clone(),
                "total_members_in_duplicate_groups": dup["total_members_in_duplicate_groups"].as_u64().unwrap_or(0),
                "coverage_percent": if items.is_empty() {
                    0.0
                } else {
                    (dup["total_members_in_duplicate_groups"].as_u64().unwrap_or(0) as f64) * 100.0 / items.len() as f64
                },
            });
            let artifact = write_feature_artifact(run_root, "duplicate_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!(
                    "duplicate_pass found {} groups",
                    dup["count"].as_u64().unwrap_or(0)
                ),
                payload,
                artifacts: vec![artifact],
            }
        }
        "burst_blink_pass" => {
            let blocks = burst_blink_payload(&items);
            let avg_sim = {
                let mut sim = 0.0;
                let mut count = 0.0;
                for i in 0..items.len() {
                    for j in (i + 1)..items.len() {
                        sim += cosine_similarity(&items[i].embedding, &items[j].embedding);
                        count += 1.0;
                    }
                }
                if count > 0.0 {
                    sim / count
                } else {
                    0.0
                }
            };
            let mut payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": blocks["count"].as_u64().unwrap_or(0),
                "blocks": blocks["blocks"].clone(),
                "mean_embedding_similarity": avg_sim,
                "policy": blocks["policy"].clone(),
            });
            if let Some(root) = payload.as_object_mut() {
                root.insert(
                    "images_scanned".to_string(),
                    serde_json::Value::from(items.len()),
                );
                root.insert(
                    "blink_blocks".to_string(),
                    serde_json::Value::from(blocks["count"].as_u64().unwrap_or(0)),
                );
            }
            let artifact = write_feature_artifact(run_root, "burst_blink_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: "burst_blink pass completed".to_string(),
                payload,
                artifacts: vec![artifact],
            }
        }
        "diagnostics_pass" => {
            let mut rows = Vec::new();
            for item in items {
                rows.push(json!({
                    "path": item.path,
                    "width": item.width,
                    "height": item.height,
                    "file_size": item.file_size,
                    "sha256": item.sha256,
                    "ahash": item.ahash,
                    "dhash": item.dhash,
                    "phash": item.phash,
                    "quality": quality_score(&item),
                    "capture_unix_ms": item.capture_unix_ms,
                }));
            }
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "run_root": run_root.to_string_lossy().to_string(),
                "items": rows,
            });
            let artifact = write_feature_artifact(run_root, "diagnostics_pass.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: "diagnostics collected".to_string(),
                payload,
                artifacts: vec![artifact],
            }
        }
        _ => FeatureArtifactResult::unsupported(feature_id),
    }
}
