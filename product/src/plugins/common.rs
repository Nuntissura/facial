use std::{
    collections::HashMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use image::{imageops::FilterType, DynamicImage, GenericImage, GenericImageView};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::debug::DebugBus;

pub const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff", "gif"];

#[derive(Clone, Serialize)]
pub struct FaceRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub score: f64,
}

#[derive(Clone, Serialize)]
pub struct ImageAnalysis {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub mean_luma: f64,
    pub luma_std: f64,
    pub median_luma: u8,
    pub contrast: f64,
    pub entropy: f64,
    pub sharpness: f64,
    pub colorfulness: f64,
    pub dynamic_range: f64,
    pub noise_estimate: f64,
    pub exposure: f64,
    pub composition: f64,
    pub skin_ratio: f64,
    pub center_bias: f64,
    pub face_count: u32,
    pub face_confidence: f64,
    /// Coarse face-clarity proxy, NOT an eye-aspect-ratio (EAR) / eye-openness measure.
    /// It is the average of a whole-image bright-pixel fraction (1 - dark-pixel fraction)
    /// and ROI edge energy. It misfires on dark or low-detail images (reads them as
    /// "closed"/low even when eyes are open) and reads bright/high-detail images as high
    /// regardless of actual eye state. The Rust field name is kept as `eye_open` for
    /// cross-file API stability; the emitted JSON key conveys the proxy semantics.
    pub eye_open: f64,
    pub face_region: Option<FaceRegion>,
    pub ahash: u64,
    pub dhash: u64,
    pub phash: u64,
    pub sha256: String,
    pub capture_unix_ms: Option<i64>,
    pub burst_key: String,
    pub embedding: Vec<f64>,
}

#[derive(Debug)]
pub struct FeatureArtifactResult {
    pub status: String,
    pub message: String,
    pub payload: Value,
    pub artifacts: Vec<String>,
}

impl FeatureArtifactResult {
    pub fn ok(payload: Value, artifacts: Vec<String>) -> Self {
        Self {
            status: "ok".to_string(),
            message: "completed".to_string(),
            payload,
            artifacts,
        }
    }

    pub fn empty(feature_id: &str) -> Self {
        Self {
            status: "empty".to_string(),
            message: format!("no images for feature {feature_id}"),
            payload: json!({"feature": feature_id, "count": 0, "items": Vec::<String>::new()}),
            artifacts: Vec::new(),
        }
    }

    pub fn unsupported(feature_id: &str) -> Self {
        Self {
            status: "unsupported".to_string(),
            message: format!("unsupported feature {feature_id}"),
            payload: json!({"feature": feature_id, "count": 0}),
            artifacts: Vec::new(),
        }
    }

    pub fn failed(feature_id: &str, message: &str) -> Self {
        Self {
            status: "failed".to_string(),
            message: message.to_string(),
            payload: json!({"feature": feature_id, "status": "failed"}),
            artifacts: Vec::new(),
        }
    }
}

pub fn is_image_path(path: &Path) -> bool {
    match path.extension().and_then(|value| value.to_str()) {
        Some(ext) => SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

pub fn normalize_image_paths(raw_paths: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in raw_paths {
        let path = Path::new(raw);
        if path.is_file() && is_image_path(path) {
            out.push(path.to_string_lossy().to_string());
        } else if path.is_dir() {
            for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
                if entry.path().is_file() && is_image_path(entry.path()) {
                    out.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn analyze_images(
    raw_paths: &[String],
    mut debug: Option<&mut DebugBus>,
) -> Vec<ImageAnalysis> {
    let mut out = Vec::new();
    for raw in normalize_image_paths(raw_paths) {
        match analyze_image(Path::new(&raw)) {
            Ok(analysis) => {
                out.push(analysis);
            }
            Err(err) => {
                if let Some(bus) = debug.as_deref_mut() {
                    bus.emit(
                        "WARN",
                        "analysis",
                        &format!("image analysis failed for {raw}: {err}"),
                        None,
                    );
                }
            }
        }
    }
    out
}

pub fn analyze_image(path: &Path) -> Result<ImageAnalysis, String> {
    let mut img = image::open(path).map_err(|err| err.to_string())?;
    let (width, height) = img.dimensions();
    let metadata = fs::metadata(path).map_err(|err| err.to_string())?;

    if width == 0 || height == 0 {
        return Err("empty image".to_string());
    }

    if width > 12_000 || height > 12_000 {
        img = img.resize(1024, 1024 * 4, FilterType::Triangle);
    } else if width > 1_500 || height > 1_500 {
        let target_w = if width > height { 1024 } else { 768 };
        let target_h = if height > width { 1024 } else { 768 };
        img = img.resize(target_w, target_h, FilterType::Triangle);
    }

    let (resized_w, resized_h) = img.dimensions();
    let rgb = img.to_rgb8();
    let gray = img.to_luma8();
    let file_data = fs::read(path).map_err(|err| err.to_string())?;
    let sha256 = hex_digest_sha256(&file_data);

    let luma: Vec<f64> = gray.pixels().map(|p| f64::from(p.0[0])).collect();
    let mean_luma = luma.iter().sum::<f64>() / luma.len() as f64;
    let mut sorted_luma = luma.clone();
    sorted_luma.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_luma = sorted_luma[sorted_luma.len() / 2].round() as u8;
    let luma_std = {
        let var = luma
            .iter()
            .map(|value| {
                let d = *value - mean_luma;
                d * d
            })
            .sum::<f64>()
            / (luma.len().max(1) as f64);
        var.sqrt()
    };

    let dynamic_range = luma
        .iter()
        .fold((f64::MAX, f64::MIN), |(min_v, max_v), value| {
            (min_v.min(*value), max_v.max(*value))
        });
    let dynamic_range = (dynamic_range.1 - dynamic_range.0).clamp(0.0, 255.0) / 2.55;

    let entropy = histogram_entropy(&luma);
    let contrast = luma_std / 2.55;

    let sharpness = tenengrad_sharpness(&gray);
    let noise_estimate = estimate_noise(&gray, &luma);

    let (colorfulness, skin_ratio, eye_open_proxy) = color_skin_eye_metrics(&rgb, &gray);
    let center_bias = center_composition_score(&luma, resized_w, resized_h);
    let exposure = ((127.0 - (mean_luma - 127.0).abs()) / 127.0 * 100.0).clamp(0.0, 100.0);
    let composition = center_bias * 0.5 + (100.0 - (dynamic_range.abs())) * 0.5;
    let (face_count, face_confidence, face_region, eye_open) =
        face_probe(&rgb, &gray, skin_ratio, mean_luma, resized_w, resized_h);

    let ahash = average_hash(&gray, 8);
    let dhash = difference_hash(&gray, 8, 9);
    let phash = dct_hash(&gray);

    let capture_unix_ms = capture_epoch_millis(path);
    let burst_key = compute_burst_key(path, capture_unix_ms, &median_luma);
    let embedding = embedding_from_signal(
        &gray,
        &rgb,
        mean_luma,
        luma_std,
        contrast,
        exposure,
        colorfulness,
        skin_ratio,
    );

    Ok(ImageAnalysis {
        path: path.to_string_lossy().to_string(),
        width,
        height,
        file_size: metadata.len(),
        mean_luma,
        luma_std,
        median_luma,
        contrast,
        entropy,
        sharpness,
        colorfulness,
        dynamic_range,
        noise_estimate,
        exposure,
        composition,
        skin_ratio,
        center_bias,
        face_count,
        face_confidence,
        // Coarse face-clarity proxy (NOT eye-aspect-ratio): average of the ROI edge-energy
        // term (`eye_open` from `face_probe`) and the whole-image bright-pixel fraction
        // (`eye_open_proxy`). Misfires on dark / low-detail images.
        eye_open: (eye_open + eye_open_proxy) / 2.0,
        face_region,
        ahash,
        dhash,
        phash,
        sha256,
        capture_unix_ms,
        burst_key,
        embedding,
    })
}

pub fn quality_score(a: &ImageAnalysis) -> f64 {
    let sharp_term = (a.sharpness * 1.6).clamp(0.0, 100.0);
    let contrast_term = (a.contrast * 0.8).clamp(0.0, 100.0);
    let exposure_term = (a.exposure).clamp(0.0, 100.0);
    let color_term = (a.colorfulness / 2.0).clamp(0.0, 100.0);
    let noise_term = (100.0 - a.noise_estimate).clamp(0.0, 100.0);
    let composition_term = a.composition.clamp(0.0, 100.0);
    (sharp_term * 0.28
        + contrast_term * 0.2
        + exposure_term * 0.18
        + color_term * 0.12
        + noise_term * 0.12
        + composition_term * 0.2)
        .clamp(0.0, 100.0)
}

pub fn ofiq_dimensions(a: &ImageAnalysis) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    out.insert("sharpness".to_string(), a.sharpness);
    out.insert("exposure".to_string(), a.exposure);
    out.insert("contrast".to_string(), a.contrast);
    out.insert("colorfulness".to_string(), a.colorfulness / 2.0);
    out.insert("entropy".to_string(), a.entropy / 8.0 * 100.0);
    out.insert("noise_estimate".to_string(), 100.0 - a.noise_estimate);
    out.insert("dynamic_range".to_string(), a.dynamic_range);
    out.insert(
        "skin_ratio".to_string(),
        (a.skin_ratio * 100.0).clamp(0.0, 100.0),
    );
    out.insert("center_bias".to_string(), a.center_bias);
    out.insert("face_confidence".to_string(), a.face_confidence * 100.0);
    out.insert("face_count".to_string(), (a.face_count as f64).min(4.0));
    // `a.eye_open` is a coarse face-clarity proxy (bright-pixel fraction + ROI edge
    // energy), NOT an eye-aspect-ratio / eye-openness measure; see the doc comment on
    // `ImageAnalysis::eye_open`. The emitted JSON key reflects the true proxy semantics
    // and misfires on dark / low-detail images.
    out.insert(
        "face_clarity_proxy".to_string(),
        (a.eye_open * 100.0).clamp(0.0, 100.0),
    );
    out.insert("sharpness_focus".to_string(), a.sharpness);
    out.insert(
        "noise_guard".to_string(),
        (100.0 - a.noise_estimate).clamp(0.0, 100.0),
    );
    out.insert("composition".to_string(), a.composition);
    out.insert("luma_std".to_string(), a.luma_std);
    out.insert("median_luma".to_string(), f64::from(a.median_luma));
    out
}

// Helper for the shallow hash-only `score_from_signature`; no scorer calls this chain.
#[allow(dead_code)]
pub fn stable_signature(path: &Path) -> u64 {
    sha256_u64(path).unwrap_or_else(|_| fallback_signature(path))
}

/// Shallow hash-only "score": derives a pseudo-score purely from the file's content/path
/// signature (see `stable_signature` / `fallback_signature`). It does NOT analyze image
/// quality and is intentionally NOT called by any scorer, so it cannot contaminate real
/// scoring. Kept as a stable, deterministic placeholder/utility. Behavior is unchanged.
#[allow(dead_code)]
pub fn score_from_signature(path: &Path) -> f64 {
    ((stable_signature(path) % 10_000) as f64) / 100.0
}

pub fn similarity_score(a: &Path, b: &Path) -> f64 {
    let da = match analyze_image(a) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    let db = match analyze_image(b) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    let d = 1.0 - ((da.ahash ^ db.ahash).count_ones() as f64 / 64.0);
    d * 100.0
}

pub fn burst_key(path: &Path) -> String {
    let capture = capture_epoch_millis(path);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    compute_burst_key(path, capture, &find_median_byte(stem.as_bytes()))
}

pub fn now_stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}_{:06}", now.as_secs(), now.subsec_micros())
}

pub fn emit_empty_warning(debug: &mut DebugBus, plugin: &str, feature: &str) {
    debug.emit(
        "WARN",
        plugin,
        &format!("no valid images found for {feature}"),
        Some(json!({"feature": feature})),
    );
}

pub fn write_feature_artifact<T: Serialize>(
    run_root: &Path,
    file_name: &str,
    payload: &T,
) -> String {
    let _ = fs::create_dir_all(run_root);
    let target: PathBuf = run_root.join(file_name);
    let path = target.to_string_lossy().to_string();
    if let Ok(serialized) = serde_json::to_string_pretty(payload) {
        let _ = fs::write(&target, serialized);
    } else {
        let _ = fs::write(&target, "{}");
    }
    path
}

pub fn artifact_paths_from_dir(run_root: &Path, suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !run_root.exists() {
        return out;
    }
    if let Ok(entries) = fs::read_dir(run_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path.extension().and_then(|ext| ext.to_str()).unwrap_or("") == "json"
            {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(suffix) {
                    out.push(path.to_string_lossy().to_string());
                }
            }
        }
    }
    out
}

pub fn now_hashed_name(prefix: &str, ext: &str) -> String {
    format!("{prefix}_{}.{}", now_stamp(), ext)
}

pub fn file_err_context(path: &Path, err: io::Error) -> String {
    format!("failed processing {}: {err}", path.to_string_lossy())
}

pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (va, vb) in a.iter().zip(b.iter()) {
        dot += va * vb;
        norm_a += va * va;
        norm_b += vb * vb;
    }
    if norm_a <= f64::EPSILON || norm_b <= f64::EPSILON {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0)
}

pub fn hash_similarity(a: &ImageAnalysis, b: &ImageAnalysis) -> f64 {
    let hamming = (a.ahash ^ b.ahash).count_ones() as f64 / 64.0;
    let dhash = (a.dhash ^ b.dhash).count_ones() as f64 / 64.0;
    let phash = (a.phash ^ b.phash).count_ones() as f64 / 64.0;
    let sim = 1.0 - (hamming + dhash + phash) / 3.0;
    (sim * 100.0).clamp(0.0, 100.0)
}

fn sha256_u64(path: &Path) -> Result<u64, String> {
    let data = fs::read(path).map_err(|err| err.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    let mut hash = [0_u8; 8];
    hash.copy_from_slice(&digest[..8]);
    Ok(u64::from_le_bytes(hash))
}

/// FNV-1a hash over the path string plus file length / mtime, used only as a fallback for
/// `stable_signature` when the SHA-256 read fails. Feeds the shallow `score_from_signature`
/// utility, which no scorer calls; it never participates in real image scoring.
#[allow(dead_code)]
fn fallback_signature(path: &Path) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    const FNV_PRIME: u64 = 0x100000001b3;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    if let Ok(metadata) = fs::metadata(path) {
        hash ^= metadata.len();
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = modified.duration_since(UNIX_EPOCH) {
                hash ^= age.as_nanos() as u64;
            }
        }
    }
    hash
}

fn hex_digest_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn capture_epoch_millis(path: &Path) -> Option<i64> {
    match fs::metadata(path).and_then(|value| value.modified()) {
        Ok(modified) => modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| Some(duration.as_millis() as i64)),
        Err(_) => None,
    }
}

fn compute_burst_key(path: &Path, capture_unix_ms: Option<i64>, fallback: &u8) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if let Some(ts) = capture_unix_ms {
        return format!("ts-{}", ts / 1000);
    }
    let lower = stem.to_lowercase();
    if let Some(index) = lower.rfind('_') {
        if index > 0 && index + 1 < lower.len() {
            return lower[..index].to_string();
        }
    }
    let trimmed = lower.trim_end_matches(|ch: char| ch.is_ascii_digit());
    if trimmed.len() > 2 {
        trimmed.to_string()
    } else {
        let mut fallback_key = format!("img_{fallback}");
        if fallback_key.len() > 6 {
            fallback_key.truncate(6);
        }
        fallback_key
    }
}

fn find_median_byte(bytes: &[u8]) -> u8 {
    if bytes.is_empty() {
        return b'0';
    }
    let mut sorted = bytes.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn tenengrad_sharpness(gray: &image::GrayImage) -> f64 {
    let (w, h) = gray.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let p = |dx: i32, dy: i32| -> f64 {
                f64::from(
                    gray.get_pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32)
                        .0[0],
                )
            };
            let gx = (-1.0 * p(-1, -1))
                + (1.0 * p(1, -1))
                + (-2.0 * p(-1, 0))
                + (2.0 * p(1, 0))
                + (-1.0 * p(-1, 1))
                + (1.0 * p(1, 1));
            let gy = (-1.0 * p(-1, -1))
                + (-2.0 * p(0, -1))
                + (-1.0 * p(1, -1))
                + (1.0 * p(-1, 1))
                + (2.0 * p(0, 1))
                + (1.0 * p(1, 1));
            let mag = (gx * gx + gy * gy).sqrt();
            sum += mag;
        }
    }
    let norm = ((w - 2) * (h - 2)) as f64;
    let score = sum / norm;
    (score / 6.5).clamp(0.0, 100.0)
}

fn histogram_entropy(luma: &[f64]) -> f64 {
    let mut bins = [0_u64; 256];
    for value in luma {
        let idx = value.round().clamp(0.0, 255.0) as usize;
        bins[idx] += 1;
    }
    let total = luma.len() as f64;
    let mut h = 0.0;
    for count in bins {
        if count == 0 {
            continue;
        }
        let p = (count as f64) / total;
        h += -p * (p.ln() / 2.0f64.ln());
    }
    h * 100.0 / 8.0
}

fn color_skin_eye_metrics(rgb: &image::RgbImage, _gray: &image::GrayImage) -> (f64, f64, f64) {
    let mut skin = 0_u32;
    let mut saturation_sum = 0.0;
    let (w, h) = rgb.dimensions();
    let mut dark_pixels = 0_u32;
    for p in rgb.pixels() {
        let r = f64::from(p.0[0]);
        let g = f64::from(p.0[1]);
        let b = f64::from(p.0[2]);

        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let saturation = if max > 0.0 { delta / max } else { 0.0 };
        saturation_sum += saturation;

        let is_skin =
            r > g && g > b && (r - g).abs() > 15.0 && (r - b).abs() > 15.0 && (r + g + b) > 110.0;
        if is_skin {
            skin += 1;
        }
        if r + g + b < 100.0 {
            dark_pixels += 1;
        }
    }
    let pixels = (w * h).max(1) as f64;
    let skin_ratio = skin as f64 / pixels;
    let colorfulness = (saturation_sum / pixels * 100.0).clamp(0.0, 100.0);
    // Face-clarity proxy, NOT eye-openness: just the bright-pixel fraction of the whole
    // image (1 - dark-pixel fraction). It is averaged with the ROI edge-energy term from
    // `face_probe` to form `ImageAnalysis::eye_open`. Misfires on dark / low-detail
    // images (reads low even when eyes are open).
    let eye_open_proxy = 100.0 * (1.0 - dark_pixels as f64 / pixels);
    (colorfulness, skin_ratio, eye_open_proxy)
}

fn center_composition_score(luma: &[f64], w: u32, h: u32) -> f64 {
    let cols = 9_u32;
    let rows = 9_u32;
    let cell_w = std::cmp::max(1, w / cols);
    let cell_h = std::cmp::max(1, h / rows);
    let mut grid = vec![0.0_f64; (cols * rows) as usize];
    for y in 0..h {
        for x in 0..w {
            let cx = (x / cell_w).min(cols - 1);
            let cy = (y / cell_h).min(rows - 1);
            let index = (cy * cols + cx) as usize;
            let px = luma[(y * w + x) as usize];
            grid[index] += px;
        }
    }
    let center = (rows / 2 * cols + cols / 2) as usize;
    let down = center.saturating_add(cols as usize);
    let center_neighbors = vec![
        center,
        center.saturating_sub(cols as usize),
        down,
        if center > 0 { center - 1 } else { center },
        (center + 1).min(grid.len() - 1),
        if center >= cols as usize {
            center - cols as usize
        } else {
            center
        },
        if down < grid.len() { down } else { center },
    ];
    let center_mass = center_neighbors
        .iter()
        .map(|index| grid[*index])
        .sum::<f64>();
    let total = grid.iter().sum::<f64>();
    if total <= 0.0 {
        return 50.0;
    }
    let normalized = center_mass / total;
    (normalized * 1000.0).clamp(0.0, 100.0)
}

/// Skin-blob heuristic, NOT a real face detector. It does not detect faces, landmarks,
/// or eyes. It runs a naive per-pixel skin test (`r > g > b` with fixed channel-difference
/// and brightness thresholds), takes the bounding box of all matching pixels in a central
/// crop, and scores that single bbox by skin ratio, area share, aspect ratio, and
/// centeredness. It emits at most one region and the returned face count is capped at 1
/// (0 or 1). It will false-positive on any large warm/skin-toned area (sand, wood, walls,
/// exposed skin that is not a face) and false-negative on small, off-center, dark, or
/// non-skin-tone faces.
fn face_probe(
    rgb: &image::RgbImage,
    gray: &image::GrayImage,
    skin_ratio: f64,
    mean_luma: f64,
    width: u32,
    height: u32,
) -> (u32, f64, Option<FaceRegion>, f64) {
    let region = if skin_ratio > 0.02 {
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        let x0 = (width as f64 * 0.2) as u32;
        let x1 = (width as f64 * 0.8) as u32;
        let y0 = (height as f64 * 0.12) as u32;
        let y1 = (height as f64 * 0.88) as u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = rgb.get_pixel(x, y);
                let r = f64::from(p.0[0]);
                let g = f64::from(p.0[1]);
                let b = f64::from(p.0[2]);
                let is_skin = r > g
                    && g > b
                    && (r - g).abs() > 15.0
                    && (r - b).abs() > 15.0
                    && (r + g + b) > 110.0;
                if is_skin {
                    xs.push(x);
                    ys.push(y);
                }
            }
        }
        if !xs.is_empty() {
            let x_min = *xs.iter().min().unwrap_or(&0);
            let x_max = *xs.iter().max().unwrap_or(&0);
            let y_min = *ys.iter().min().unwrap_or(&0);
            let y_max = *ys.iter().max().unwrap_or(&0);
            let w = x_max.saturating_sub(x_min).saturating_add(1);
            let h = y_max.saturating_sub(y_min).saturating_add(1);
            let area = (w as f64) * (h as f64);
            let ratio = h.max(1) as f64 / w.max(1) as f64;
            let image_area = (width as f64) * (height as f64);
            let area_share = area / image_area;
            let aspect_ok = (0.5..=1.8).contains(&ratio);
            let center_x = (x_min + w / 2) as f64 / width as f64;
            let center_y = (y_min + h / 2) as f64 / height as f64;
            let centered = 1.0 - ((center_x - 0.5).abs() + (center_y - 0.45).abs()) * 0.8;
            let confidence = ((skin_ratio * 220.0).clamp(0.0, 35.0)
                + (area_share * 180.0).clamp(0.0, 35.0)
                + if aspect_ok { 20.0 } else { 8.0 }
                + (centered.max(0.0) * 10.0))
                .clamp(0.0, 100.0);
            let count = if confidence > 25.0 && area_share > 0.04 {
                1
            } else {
                0
            };
            let mut edge_score = 0.0;
            if w > 4 && h > 4 {
                let roi = gray.view(x_min, y_min, w, h).to_image();
                edge_score = tenengrad_sharpness(&roi) / 100.0;
                let mut top = 0.0;
                for y in 0..roi.height() {
                    for x in 0..roi.width() {
                        if y < roi.height() / 3 {
                            top += f64::from(roi.get_pixel(x, y).0[0]);
                        }
                    }
                }
                top /= (w as f64 * (h as f64 / 3.0)).max(1.0);
            }
            let eye_open =
                (edge_score * 60.0).clamp(0.0, 100.0) * if mean_luma > 40.0 { 1.0 } else { 0.8 };
            let region = FaceRegion {
                x: x_min,
                y: y_min,
                width: w,
                height: h,
                score: confidence,
            };
            return (
                count,
                confidence / 100.0,
                if count > 0 { Some(region) } else { None },
                eye_open,
            );
        }
        (0, 0.0, None, 0.0)
    } else {
        (0, 0.0, None, 0.0)
    };
    region
}

fn average_hash(gray: &image::GrayImage, size: u32) -> u64 {
    let small = DynamicImage::ImageLuma8(gray.clone())
        .resize_exact(size, size, FilterType::Nearest)
        .to_luma8();
    let mut sum = 0_u64;
    for p in small.pixels() {
        sum += u64::from(p.0[0]);
    }
    let mean = (sum as f64) / ((size * size) as f64);
    let mut hash = 0_u64;
    let mut bit = 0_u64;
    for y in 0..size {
        for x in 0..size {
            let idx = u64::from(small.get_pixel(x, y).0[0] > mean as u8);
            hash |= idx << bit;
            bit = (bit + 1) % 64;
        }
    }
    hash
}

fn difference_hash(gray: &image::GrayImage, width: u32, height: u32) -> u64 {
    let img = DynamicImage::ImageLuma8(gray.clone())
        .resize_exact(width, height, FilterType::Nearest)
        .to_luma8();
    let mut hash = 0_u64;
    let mut bit = 0_u64;
    for y in 0..height {
        for x in 0..(width.saturating_sub(1)) {
            let left = img.get_pixel(x, y).0[0];
            let right = img.get_pixel(x + 1, y).0[0];
            let idx = u64::from(left > right);
            hash |= idx << (bit % 64);
            bit = bit.saturating_add(1);
        }
    }
    hash
}

fn dct_hash(gray: &image::GrayImage) -> u64 {
    let size = 32_u32;
    let img = DynamicImage::ImageLuma8(gray.clone())
        .resize_exact(size, size, FilterType::Nearest)
        .to_luma8();
    let mut matrix = vec![vec![0.0_f64; size as usize]; size as usize];
    for y in 0..size {
        for x in 0..size {
            matrix[y as usize][x as usize] = f64::from(img.get_pixel(x, y).0[0]);
        }
    }
    let mut coeffs = Vec::new();
    for v in 0..8 {
        for u in 0..8 {
            if u == 0 && v == 0 {
                continue;
            }
            let mut sum = 0.0;
            for y in 0..size {
                for x in 0..size {
                    let a = std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64
                        / (2.0 * size as f64);
                    let b = std::f64::consts::PI * (2.0 * y as f64 + 1.0) * v as f64
                        / (2.0 * size as f64);
                    sum += matrix[y as usize][x as usize] * a.cos() * b.cos();
                }
            }
            let au = if u == 0 { 1.0 / 2.0_f64.sqrt() } else { 1.0 };
            let bv = if v == 0 { 1.0 / 2.0_f64.sqrt() } else { 1.0 };
            let c = 0.25 * au * bv * sum;
            coeffs.push(c);
        }
    }
    let mut sorted = coeffs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let mut hash = 0_u64;
    for (idx, value) in coeffs.iter().take(64).enumerate() {
        if *value > median {
            hash |= 1_u64 << (idx as u64 % 64);
        }
    }
    hash
}

fn embedding_from_signal(
    gray: &image::GrayImage,
    rgb: &image::RgbImage,
    mean_luma: f64,
    luma_std: f64,
    contrast: f64,
    exposure: f64,
    colorfulness: f64,
    skin_ratio: f64,
) -> Vec<f64> {
    let small = DynamicImage::ImageLuma8(gray.clone())
        .resize(16, 16, FilterType::Triangle)
        .to_luma8();
    let mut out = Vec::with_capacity(16 * 16 + 8);
    for p in small.pixels() {
        out.push(f64::from(p.0[0]) / 255.0);
    }
    let (mut r_sum, mut g_sum, mut b_sum) = (0.0, 0.0, 0.0);
    let (mut r2, mut g2, mut b2) = (0.0, 0.0, 0.0);
    for p in rgb.pixels() {
        let r = f64::from(p.0[0]) / 255.0;
        let g = f64::from(p.0[1]) / 255.0;
        let b = f64::from(p.0[2]) / 255.0;
        r_sum += r;
        g_sum += g;
        b_sum += b;
        r2 += r * r;
        g2 += g * g;
        b2 += b * b;
    }
    let count = (rgb.width() * rgb.height()).max(1) as f64;
    out.push(mean_luma / 255.0);
    out.push(luma_std / 128.0);
    out.push(contrast / 100.0);
    out.push(exposure / 100.0);
    out.push(colorfulness / 100.0);
    out.push(skin_ratio);
    out.push((r_sum / count).clamp(0.0, 1.0));
    out.push((g_sum / count).clamp(0.0, 1.0));
    out.push((b_sum / count).clamp(0.0, 1.0));
    out.push((r2 / count).clamp(0.0, 1.0));
    out.push((g2 / count).clamp(0.0, 1.0));
    out.push((b2 / count).clamp(0.0, 1.0));
    out
}

fn estimate_noise(gray: &image::GrayImage, luma: &[f64]) -> f64 {
    if gray.width() < 3 || gray.height() < 3 {
        return 0.0;
    }
    let mut diffs = 0.0;
    let (w, h) = gray.dimensions();
    let mut count = 0.0;
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let p = f64::from(gray.get_pixel(x, y).0[0]);
            let neighbors = [
                f64::from(gray.get_pixel(x - 1, y).0[0]),
                f64::from(gray.get_pixel(x + 1, y).0[0]),
                f64::from(gray.get_pixel(x, y - 1).0[0]),
                f64::from(gray.get_pixel(x, y + 1).0[0]),
            ];
            let mean = (neighbors.iter().sum::<f64>() + p) / 5.0;
            diffs += (p - mean).abs();
            count += 1.0;
        }
    }
    let raw = if count > 0.0 {
        (diffs / count).clamp(0.0, 255.0)
    } else {
        0.0
    };
    let base = luma.iter().map(|v| (*v - 128.0).abs()).sum::<f64>() / (luma.len().max(1) as f64);
    let noise = (raw / base.max(0.5) * 100.0).clamp(0.0, 100.0);
    noise
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_grid_hashes_do_not_panic_on_narrow_images() {
        let gray =
            image::GrayImage::from_fn(7, 80, |x, y| image::Luma([((x * 17 + y * 3) % 255) as u8]));

        let _ = average_hash(&gray, 8);
        let _ = difference_hash(&gray, 8, 9);
        let _ = dct_hash(&gray);
    }
}
