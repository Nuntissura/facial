//! PIPNet 98-point facial landmarks (WP-021, wave-2 curation signals).
//!
//! Pure-Rust ONNX inference via tract — same graph-external decode pattern as
//! the YuNet detector: the network emits raw maps, all decoding happens here.
//!
//! Model: `pipnet_r18_wflw_98.onnx` (yakhyo/pipnet-onnx, MIT; upstream PIPNet
//! MIT, IJCV 2021), operator-provisioned like ArcFace (47 MB, never bundled).
//! Input: a 1.2x-expanded face crop, resized 256x256, ImageNet-normalized.
//! Outputs (5): `cls`(1,98,8,8), `x`(1,98,8,8), `y`(1,98,8,8),
//! `nb_x`(1,980,8,8), `nb_y`(1,980,8,8) — stride 32, 10 neighbors/landmark.
//! Decode: per-landmark argmax + offset, merged with the reverse-neighbor
//! predictions derived from the vendored WFLW meanface (the reference PIPNet
//! merge, ported).
//!
//! Signals (research basis: governance/research_landmark_models.md):
//! - eyes-open via the published simplified WFLW EAR — `source: real`;
//! - occlusion via per-region landmark-confidence means — `source: proxy`,
//!   triage-only, never a gate.

use std::path::{Path, PathBuf};

use image::RgbImage;
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

/// WFLW meanface: 98 x,y pairs in normalized [0,1] coords (vendored, MIT;
/// see product/assets/landmarks/wflw_meanface-SOURCE.txt).
const WFLW_MEANFACE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/landmarks/wflw_meanface.txt"
));

pub const NUM_LMS: usize = 98;
const NUM_NB: usize = 10;
const INPUT: usize = 256;
const GRID: usize = 8; // INPUT / net stride 32

/// Face crop expansion around the detector box (reference PIPNet demo uses 1.2x).
const CROP_SCALE: f32 = 1.2;

/// Eyes-open threshold on the simplified WFLW EAR (single mid-lid vertical /
/// eye width). Open eyes measure ~0.25-0.40 on this scale; closed collapse
/// toward 0. Raw EARs are always emitted so consumers can re-bucket.
pub const EAR_OPEN_MIN: f32 = 0.15;
pub const EAR_METHOD: &str = "wflw_simplified_v1";

// NOTE (WP-021 validation, 2026-06-11): a per-region cls-confidence OCCLUSION
// proxy was implemented and validated per the spike's gate 4 — it FAILED to
// separate (conf_min 0.565-0.580 across a clean photo, a black eye band, and
// a black mouth band, while EAR separated cleanly). Per the spike contract
// ("if separation is weak, ship eyes-open alone"), the occlusion flag is
// WITHHELD; honest occlusion needs a segmentation model (future packet). The
// raw per-landmark confidences remain exposed as real measurements.

const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

pub struct LandmarkAnalysis {
    /// 98 landmarks in ORIGINAL image pixel coordinates.
    pub points: Vec<[f32; 2]>,
    /// Per-landmark confidence: sigmoid of the cls-map max (localization
    /// peakiness, 0..1). NOT an occlusion signal — see module note.
    pub confidence: Vec<f32>,
    pub ear_left: f32,
    pub ear_right: f32,
    /// "open" | "closed" from min(ear_left, ear_right) vs EAR_OPEN_MIN.
    pub eyes_open: &'static str,
    pub confidence_min: f32,
}

pub struct LandmarkEngine {
    model: TypedRunnableModel<TypedModel>,
    model_path: PathBuf,
    model_sha256: String,
    /// nb_index[i] = the 10 meanface-nearest landmarks of i (excluding i).
    nb_index: Vec<[usize; NUM_NB]>,
    /// reverse[t] = every (source landmark s, neighbor slot k) with
    /// nb_index[s][k] == t; the reference PIPNet merge gathers these.
    reverse: Vec<Vec<(usize, usize)>>,
}

impl LandmarkEngine {
    pub fn load(model_path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(model_path).map_err(|e| format!("read landmark model: {e}"))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let model_sha256 = format!("{:x}", hasher.finalize());
        let model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(&bytes))
            .map_err(|e| format!("parse landmark onnx: {e}"))?
            .with_input_fact(0, f32::fact([1, 3, INPUT as i32, INPUT as i32]).into())
            .map_err(|e| format!("landmark input fact: {e}"))?
            .into_optimized()
            .map_err(|e| format!("optimize landmark onnx: {e}"))?
            .into_runnable()
            .map_err(|e| format!("landmark runnable: {e}"))?;

        let meanface = parse_meanface(WFLW_MEANFACE)?;
        let nb_index = neighbor_index(&meanface);
        let reverse = reverse_index(&nb_index);

        let engine = Self {
            model,
            model_path: model_path.to_path_buf(),
            model_sha256,
            nb_index,
            reverse,
        };
        engine.self_check()?;
        Ok(engine)
    }

    pub fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Blank-frame execution + output-layout assertion (3x [1,98,8,8] then
    /// 2x [1,980,8,8]); a wrong export can never silently emit wrong geometry.
    fn self_check(&self) -> Result<(), String> {
        let out = self.run_raw(&vec![0f32; 3 * INPUT * INPUT])?;
        if out.len() < 5 {
            return Err(format!(
                "landmark model emitted {} outputs, expected 5 (PIPNet cls/x/y/nb_x/nb_y)",
                out.len()
            ));
        }
        let dims: Vec<Vec<usize>> = out.iter().take(5).map(|t| t.shape().to_vec()).collect();
        let plain = [1, NUM_LMS, GRID, GRID];
        let nb = [1, NUM_LMS * NUM_NB, GRID, GRID];
        for (i, expected) in [plain, plain, plain, nb, nb].iter().enumerate() {
            if dims[i] != *expected {
                return Err(format!(
                    "landmark output {i} has shape {:?}, expected {:?} (PIPNet WFLW 2023 export)",
                    dims[i], expected
                ));
            }
        }
        Ok(())
    }

    fn run_raw(&self, blob: &[f32]) -> Result<TVec<TValue>, String> {
        let input = Tensor::from_shape(&[1, 3, INPUT, INPUT], blob)
            .map_err(|e| format!("landmark tensor: {e}"))?;
        self.model
            .run(tvec!(input.into()))
            .map_err(|e| format!("landmark inference: {e}"))
    }

    /// Detect 98 landmarks for the face in `bbox` (original pixel coords) and
    /// derive the wave-2 signals.
    pub fn analyze(&self, img: &RgbImage, bbox: [f32; 4]) -> Result<LandmarkAnalysis, String> {
        let (iw, ih) = img.dimensions();
        if iw == 0 || ih == 0 {
            return Err("empty image".to_string());
        }
        // Square 1.2x crop around the box center, clamped to the image.
        let [bx, by, bw, bh] = bbox;
        let cx = bx + bw / 2.0;
        let cy = by + bh / 2.0;
        let side = bw.max(bh) * CROP_SCALE;
        let x0 = (cx - side / 2.0).max(0.0);
        let y0 = (cy - side / 2.0).max(0.0);
        let x1 = (cx + side / 2.0).min(iw as f32);
        let y1 = (cy + side / 2.0).min(ih as f32);
        let cw = x1 - x0;
        let ch = y1 - y0;
        if cw < 8.0 || ch < 8.0 {
            return Err("face crop too small for landmarks".to_string());
        }
        let crop =
            image::imageops::crop_imm(img, x0 as u32, y0 as u32, cw as u32, ch as u32).to_image();
        let resized = image::imageops::resize(
            &crop,
            INPUT as u32,
            INPUT as u32,
            image::imageops::FilterType::Triangle,
        );

        let mut blob = vec![0f32; 3 * INPUT * INPUT];
        let plane = INPUT * INPUT;
        for y in 0..INPUT {
            for x in 0..INPUT {
                let px = resized.get_pixel(x as u32, y as u32);
                for c in 0..3usize {
                    blob[c * plane + y * INPUT + x] =
                        (px[c] as f32 / 255.0 - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
                }
            }
        }

        let out = self.run_raw(&blob)?;
        let read = |idx: usize| -> Result<Vec<f32>, String> {
            out[idx]
                .to_array_view::<f32>()
                .map(|v| v.iter().copied().collect())
                .map_err(|e| format!("landmark output {idx}: {e}"))
        };
        let cls = read(0)?;
        let off_x = read(1)?;
        let off_y = read(2)?;
        let nb_x = read(3)?;
        let nb_y = read(4)?;

        let (direct, nb_preds, confidence) = decode_maps(&cls, &off_x, &off_y, &nb_x, &nb_y);
        let merged = merge_predictions(&direct, &nb_preds, &self.reverse);

        // Normalized crop coords -> original image pixels.
        let points: Vec<[f32; 2]> = merged
            .iter()
            .map(|(px, py)| [x0 + px * cw, y0 + py * ch])
            .collect();

        let (ear_left, ear_right) = wflw_ear(&points);
        let eyes_open = if ear_left.min(ear_right) >= EAR_OPEN_MIN {
            "open"
        } else {
            "closed"
        };
        let confidence_min = confidence.iter().copied().fold(f32::INFINITY, f32::min);

        Ok(LandmarkAnalysis {
            points,
            confidence,
            ear_left,
            ear_right,
            eyes_open,
            confidence_min,
        })
    }
}

/// Parse the vendored meanface text (whitespace-separated x0 y0 x1 y1 ...).
fn parse_meanface(raw: &str) -> Result<Vec<[f32; 2]>, String> {
    let values: Vec<f32> = raw
        .split_whitespace()
        .map(|v| v.parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("meanface parse: {e}"))?;
    if values.len() != NUM_LMS * 2 {
        return Err(format!(
            "meanface has {} values, expected {}",
            values.len(),
            NUM_LMS * 2
        ));
    }
    Ok((0..NUM_LMS)
        .map(|i| [values[2 * i], values[2 * i + 1]])
        .collect())
}

/// For each landmark: its NUM_NB nearest meanface landmarks (self excluded).
fn neighbor_index(meanface: &[[f32; 2]]) -> Vec<[usize; NUM_NB]> {
    let mut out = Vec::with_capacity(meanface.len());
    for (i, a) in meanface.iter().enumerate() {
        let mut dists: Vec<(usize, f32)> = meanface
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(j, b)| {
                let dx = a[0] - b[0];
                let dy = a[1] - b[1];
                (j, dx * dx + dy * dy)
            })
            .collect();
        dists.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut nb = [0usize; NUM_NB];
        for (k, slot) in nb.iter_mut().enumerate() {
            *slot = dists[k].0;
        }
        out.push(nb);
    }
    out
}

/// Invert the neighbor table: reverse[t] = all (s, k) with nb_index[s][k] == t.
fn reverse_index(nb_index: &[[usize; NUM_NB]]) -> Vec<Vec<(usize, usize)>> {
    let mut reverse = vec![Vec::new(); nb_index.len()];
    for (s, nbs) in nb_index.iter().enumerate() {
        for (k, &t) in nbs.iter().enumerate() {
            reverse[t].push((s, k));
        }
    }
    reverse
}

/// Raw-map decode: per landmark the argmax cell + offsets give the direct
/// estimate; the same cell's neighbor maps give NUM_NB predictions of that
/// landmark's meanface neighbors. All coords normalized to [0,1] of the crop.
#[allow(clippy::type_complexity)]
fn decode_maps(
    cls: &[f32],
    off_x: &[f32],
    off_y: &[f32],
    nb_x: &[f32],
    nb_y: &[f32],
) -> (Vec<(f32, f32)>, Vec<Vec<(f32, f32)>>, Vec<f32>) {
    let cells = GRID * GRID;
    let mut direct = Vec::with_capacity(NUM_LMS);
    let mut nb_preds = Vec::with_capacity(NUM_LMS);
    let mut confidence = Vec::with_capacity(NUM_LMS);
    for i in 0..NUM_LMS {
        let map = &cls[i * cells..(i + 1) * cells];
        let (mut best_idx, mut best) = (0usize, f32::NEG_INFINITY);
        for (idx, &v) in map.iter().enumerate() {
            if v > best {
                best = v;
                best_idx = idx;
            }
        }
        let r = (best_idx / GRID) as f32;
        let c = (best_idx % GRID) as f32;
        confidence.push(1.0 / (1.0 + (-best).exp())); // sigmoid
        let px = (c + off_x[i * cells + best_idx]) / GRID as f32;
        let py = (r + off_y[i * cells + best_idx]) / GRID as f32;
        direct.push((px, py));
        let mut nbs = Vec::with_capacity(NUM_NB);
        for k in 0..NUM_NB {
            let ch = (i * NUM_NB + k) * cells + best_idx;
            nbs.push(((c + nb_x[ch]) / GRID as f32, (r + nb_y[ch]) / GRID as f32));
        }
        nb_preds.push(nbs);
    }
    (direct, nb_preds, confidence)
}

/// The reference PIPNet merge: each landmark's final position is the mean of
/// its own direct estimate and every neighbor prediction pointing at it.
fn merge_predictions(
    direct: &[(f32, f32)],
    nb_preds: &[Vec<(f32, f32)>],
    reverse: &[Vec<(usize, usize)>],
) -> Vec<(f32, f32)> {
    direct
        .iter()
        .enumerate()
        .map(|(t, &(dx, dy))| {
            let mut sx = dx;
            let mut sy = dy;
            let mut n = 1.0f32;
            for &(s, k) in &reverse[t] {
                let (nx, ny) = nb_preds[s][k];
                sx += nx;
                sy += ny;
                n += 1.0;
            }
            (sx / n, sy / n)
        })
        .collect()
}

/// Published simplified WFLW EAR: mid-lid vertical over eye width.
/// Right eye: ||p66-p62|| / ||p64-p60||; left eye: ||p74-p70|| / ||p72-p68||.
fn wflw_ear(points: &[[f32; 2]]) -> (f32, f32) {
    let dist = |a: usize, b: usize| -> f32 {
        let dx = points[a][0] - points[b][0];
        let dy = points[a][1] - points[b][1];
        (dx * dx + dy * dy).sqrt()
    };
    let right = {
        let width = dist(60, 64);
        if width <= f32::EPSILON {
            0.0
        } else {
            dist(62, 66) / width
        }
    };
    let left = {
        let width = dist(68, 72);
        if width <= f32::EPSILON {
            0.0
        } else {
            dist(70, 74) / width
        }
    };
    (left, right)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meanface_parses_and_neighbors_are_sane() {
        let meanface = parse_meanface(WFLW_MEANFACE).unwrap();
        assert_eq!(meanface.len(), NUM_LMS);
        let nb = neighbor_index(&meanface);
        assert_eq!(nb.len(), NUM_LMS);
        for (i, nbs) in nb.iter().enumerate() {
            assert!(!nbs.contains(&i), "landmark {i} lists itself as neighbor");
            let unique: std::collections::BTreeSet<_> = nbs.iter().collect();
            assert_eq!(unique.len(), NUM_NB, "landmark {i} has duplicate neighbors");
        }
        // Every landmark is someone's neighbor in a 98-pt face (sanity).
        let reverse = reverse_index(&nb);
        assert!(reverse.iter().filter(|r| r.is_empty()).count() < NUM_LMS / 4);
    }

    #[test]
    fn decode_maps_reads_argmax_offsets_and_confidence() {
        let cells = GRID * GRID;
        let mut cls = vec![-10.0f32; NUM_LMS * cells];
        let mut off_x = vec![0f32; NUM_LMS * cells];
        let mut off_y = vec![0f32; NUM_LMS * cells];
        let nb_x = vec![0f32; NUM_LMS * NUM_NB * cells];
        let nb_y = vec![0f32; NUM_LMS * NUM_NB * cells];
        // Landmark 5 peaks at cell (row 2, col 3) with offsets (0.5, 0.25).
        let idx = 2 * GRID + 3;
        cls[5 * cells + idx] = 10.0;
        off_x[5 * cells + idx] = 0.5;
        off_y[5 * cells + idx] = 0.25;
        let (direct, nb_preds, conf) = decode_maps(&cls, &off_x, &off_y, &nb_x, &nb_y);
        let (px, py) = direct[5];
        assert!((px - (3.0 + 0.5) / 8.0).abs() < 1e-6);
        assert!((py - (2.0 + 0.25) / 8.0).abs() < 1e-6);
        assert!(conf[5] > 0.99); // sigmoid(10)
        assert!(conf[0] < 0.01); // sigmoid(-10)
        assert_eq!(nb_preds[5].len(), NUM_NB);
    }

    #[test]
    fn merge_predictions_averages_reverse_neighbors() {
        // 2 landmarks; landmark 1's slot-0 neighbor is landmark 0.
        let direct = vec![(0.2, 0.2), (0.8, 0.8)];
        let nb_preds = vec![vec![(0.0, 0.0)], vec![(0.4, 0.4)]];
        let reverse = vec![vec![(1usize, 0usize)], vec![]];
        let merged = merge_predictions(&direct, &nb_preds, &reverse);
        // lm0 = mean(direct 0.2, neighbor-pred 0.4) = 0.3; lm1 untouched.
        assert!((merged[0].0 - 0.3).abs() < 1e-6);
        assert!((merged[1].0 - 0.8).abs() < 1e-6);
    }

    #[test]
    fn wflw_ear_separates_open_from_closed_geometry() {
        let mut points = vec![[0.0f32; 2]; NUM_LMS];
        // Right eye: corners (0,0)-(30,0); mid lids at y -4/+4 (open).
        points[60] = [0.0, 0.0];
        points[64] = [30.0, 0.0];
        points[62] = [15.0, -4.0];
        points[66] = [15.0, 4.0];
        // Left eye: corners (50,0)-(80,0); lids nearly touching (closed).
        points[68] = [50.0, 0.0];
        points[72] = [80.0, 0.0];
        points[70] = [65.0, -0.5];
        points[74] = [65.0, 0.5];
        let (left, right) = wflw_ear(&points);
        assert!(right > EAR_OPEN_MIN, "open eye EAR {right}");
        assert!(left < EAR_OPEN_MIN, "closed eye EAR {left}");
    }

    #[test]
    fn engine_loads_under_tract_when_model_is_provisioned() {
        // Validation gate 1 from the spike: tract 0.21 must load the export and
        // the 5-output layout must self-check. Skips (loudly) only when the
        // 47MB provisioned model is absent on this machine.
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/models/pipnet_r18_wflw_98.onnx"
        ));
        if !path.exists() {
            eprintln!("SKIP engine_loads_under_tract: provisioned model missing at {path:?}");
            return;
        }
        let engine = LandmarkEngine::load(&path);
        assert!(
            engine.is_ok(),
            "PIPNet failed under tract: {:?}",
            engine.err()
        );
        assert_eq!(engine.unwrap().model_sha256().len(), 64);
    }
}
