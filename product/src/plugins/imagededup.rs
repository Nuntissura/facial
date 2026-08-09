use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::json;

use crate::{
    debug::DebugBus,
    plugins::common::{
        analyze_images, emit_empty_warning, hash_similarity, quality_score, write_feature_artifact,
        FeatureArtifactResult, ImageAnalysis,
    },
};

fn image_keep_score(item: &ImageAnalysis) -> f64 {
    let q = quality_score(item);
    let face = (item.face_count as f64) * 8.0;
    let eyes = item.eye_open;
    let size = ((item.file_size as f64).max(1.0)).ln().max(1.0) * 1.7;
    let confidence = item.face_confidence * 10.0;
    (q * 0.72 + face + eyes + confidence + size).clamp(0.0, 250.0)
}

fn pair_candidates(items: &[ImageAnalysis], threshold: f64) -> Vec<(usize, usize, f64)> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let sim = hash_similarity(&items[i], &items[j]);
            if sim >= threshold {
                out.push((i, j, sim));
            }
        }
    }
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn pair_rows(items: &[ImageAnalysis], threshold: f64) -> Vec<serde_json::Value> {
    pair_candidates(items, threshold)
        .into_iter()
        .map(|(a_idx, b_idx, score)| {
            json!({
                "a": items[a_idx].path,
                "b": items[b_idx].path,
                "similarity": score,
                "a_quality": image_keep_score(&items[a_idx]),
                "b_quality": image_keep_score(&items[b_idx]),
                "method": "ahash+dhash+phash",
                "units": "percent",
            })
        })
        .collect()
}

fn hash_duplicate_rows(items: &[ImageAnalysis]) -> Vec<serde_json::Value> {
    let mut exact = BTreeMap::<String, Vec<&ImageAnalysis>>::new();
    for item in items {
        let key = format!("{}_{}", item.sha256, item.file_size);
        exact.entry(key).or_default().push(item);
    }

    let mut out = Vec::new();
    for (hash_key, paths) in exact {
        if paths.len() > 1 {
            let sorted: Vec<String> = {
                let mut values = paths
                    .iter()
                    .map(|item| item.path.clone())
                    .collect::<Vec<_>>();
                values.sort_unstable();
                values
            };
            let total_size = paths.iter().map(|item| item.file_size).sum::<u64>();
            let best_keep = paths.iter().max_by(|a, b| {
                image_keep_score(a)
                    .total_cmp(&image_keep_score(b))
                    .then_with(|| a.path.cmp(&b.path))
            });
            out.push(json!({
                "group_key": hash_key,
                "type": "sha256+size",
                "paths": sorted,
                "count": paths.len(),
                "type_size_total": total_size,
                "best_keep": best_keep.map(|value| value.path.clone()).unwrap_or_default(),
                "method": "exact_hash_group",
            }));
        }
    }
    out
}

fn pair_score_map(pairs: &[(usize, usize, f64)]) -> HashMap<(usize, usize), f64> {
    let mut map = HashMap::with_capacity(pairs.len());
    for (a, b, score) in pairs {
        map.insert((*a, *b), *score);
        map.insert((*b, *a), *score);
    }
    map
}

fn remove_candidates(items: &[ImageAnalysis], threshold: f64) -> Vec<serde_json::Value> {
    let pairs = pair_candidates(items, threshold);
    if items.is_empty() || pairs.is_empty() {
        return Vec::new();
    }

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    let mut removed = HashSet::<usize>::new();
    for (a, b, _) in &pairs {
        adjacency[*a].push(*b);
        adjacency[*b].push(*a);
    }
    for links in &mut adjacency {
        links.sort_unstable();
        links.dedup();
    }

    let score_map = pair_score_map(&pairs);
    let mut visited = vec![false; items.len()];
    let mut out = Vec::new();
    let mut component_id = 0_u32;

    for start in 0..items.len() {
        if visited[start] || adjacency[start].is_empty() {
            continue;
        }
        component_id += 1;
        let mut queue = VecDeque::new();
        queue.push_back(start);
        visited[start] = true;
        let mut component = Vec::new();
        while let Some(idx) = queue.pop_front() {
            component.push(idx);
            for &neighbor in &adjacency[idx] {
                if !visited[neighbor] {
                    visited[neighbor] = true;
                    queue.push_back(neighbor);
                }
            }
        }

        if component.len() <= 1 {
            continue;
        }

        component.sort_unstable();
        let mut sorted = component.clone();
        sorted.sort_by(|a, b| {
            let score_a = image_keep_score(&items[*a]);
            let score_b = image_keep_score(&items[*b]);
            score_b
                .total_cmp(&score_a)
                .then_with(|| items[*a].path.cmp(&items[*b].path))
        });

        let keeper = sorted.first().copied().unwrap_or(start);
        for idx in component {
            if idx == keeper {
                continue;
            }
            if removed.contains(&idx) {
                continue;
            }
            let keeper_score = image_keep_score(&items[keeper]);
            let candidate_score = image_keep_score(&items[idx]);
            let similarity = score_map
                .get(&(idx, keeper))
                .copied()
                .unwrap_or_else(|| score_map.get(&(keeper, idx)).copied().unwrap_or(0.0));
            removed.insert(idx);
            out.push(json!({
                "path": items[idx].path,
                "action": "remove",
                "keep": items[keeper].path,
                "component_id": component_id,
                "similarity_to_keep": similarity,
                "similarity_threshold": threshold,
                "decision": {
                    "keep_score": keeper_score,
                    "remove_score": candidate_score,
                    "score_delta": (keeper_score - candidate_score),
                    "reason": "connected_component_quality_max",
                },
            }));
        }
    }

    out.sort_by(|a, b| {
        let a_path = a.get("path").and_then(|value| value.as_str()).unwrap_or("");
        let b_path = b.get("path").and_then(|value| value.as_str()).unwrap_or("");
        a_path.cmp(b_path)
    });
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
        emit_empty_warning(debug, "imagededup", feature_id);
        return FeatureArtifactResult::empty(feature_id);
    }

    match feature_id {
        "hash_duplicates" => {
            let total_pairs = items.len().saturating_sub(1) * items.len() / 2;
            let groups = hash_duplicate_rows(&items);
            let mut duplicates = 0usize;
            for group in &groups {
                duplicates += group["count"].as_u64().unwrap_or(0) as usize;
            }
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": groups.len(),
                "groups": groups,
                "method": "sha256+size",
                "duplicates_found": duplicates,
                "total_candidate_pairs": total_pairs,
                "coverage_percent": if items.is_empty() {
                    0.0
                } else {
                    (duplicates as f64) * 100.0 / items.len() as f64
                },
            });
            let artifact = write_feature_artifact(run_root, "hash_duplicates.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("found {} hash groups", groups.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "cnn_duplicates" => {
            let threshold = 75.0;
            let pairs = pair_candidates(&items, threshold);
            let rows = pair_rows(&items, threshold);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": rows.len(),
                "pairs": rows,
                "method": "perceptual-hash hybrid (ahash+dhash+phash, no neural inference)",
                "threshold": threshold,
                "pairs_selected": pairs.len(),
                "pairs_considered": if items.len() >= 2 {
                    items.len() * (items.len() - 1) / 2
                } else {
                    0
                },
            });
            let artifact = write_feature_artifact(run_root, "cnn_duplicates.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("found {} duplicate candidates", rows.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        "remove_candidates" => {
            let threshold = 78.0;
            let removed = remove_candidates(&items, threshold);
            let payload = json!({
                "feature": feature_id,
                "source": "proxy",
                "count": removed.len(),
                "remove_list": removed,
                "pairs_threshold": threshold,
                "policy": {
                    "component_scoring": "connected_component_max_score",
                    "keep_score_formula": "0.72*quality_score + 8*face_count + eye_open + 10*face_confidence + 1.7*ln(file_size)",
                },
                "images_scanned": items.len(),
            });
            let artifact = write_feature_artifact(run_root, "remove_candidates.json", &payload);
            FeatureArtifactResult {
                status: "ok".to_string(),
                message: format!("candidate removal list size {}", removed.len()),
                payload,
                artifacts: vec![artifact],
            }
        }
        _ => FeatureArtifactResult::unsupported(feature_id),
    }
}
