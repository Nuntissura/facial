//! Optional, deterministic face-identity engine (Phase 2, Option A).
//!
//! Pure-Rust ONNX inference via `tract` — no Python, no native runtime. Models
//! are provisioned at runtime (never bundled). When no embedder is configured,
//! or loading fails, the engine stays disabled and the app reports
//! `identity: unavailable` instead of faking a verdict.
//!
//! Alignment: when a YuNet detector model is also provisioned, faces are
//! detected and aligned via a 5-point similarity transform to the canonical
//! ArcFace template (`align="yunet_112"`). Otherwise it falls back to a
//! deterministic whole-image resize (`align="resize_112"`). The method used is
//! reported in every result for audit.

use std::path::{Path, PathBuf};

use image::RgbImage;
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

/// Canonical ArcFace 5-point destination template for a 112x112 crop
/// (left eye, right eye, nose, left mouth, right mouth).
const ARCFACE_DST: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

const DET_INPUT: usize = 640;
/// Floor score for a face to be usable for alignment (kept low so a single
/// imperfect face still aligns). Face *counting* uses the separate, higher
/// `identity_count_threshold` from config, applied by the caller.
const DET_THRESHOLD: f32 = 0.6;
/// IoU cutoff for greedy non-max suppression (OpenCV FaceDetectorYN default).
const NMS_THRESHOLD: f32 = 0.3;

/// A single detected face in ORIGINAL image pixel coordinates.
#[derive(Clone)]
pub struct Face {
    /// Bounding box: x, y (top-left), w, h.
    pub bbox: [f32; 4],
    /// Detection confidence `sqrt(cls*obj)` in [0,1].
    pub score: f32,
    /// 5 landmarks (left eye, right eye, nose, left mouth, right mouth).
    pub landmarks: [[f32; 2]; 5],
}

/// Result of embedding one image together with its face detections, so the
/// identity gate can report face box/count/scale without a second pass.
pub struct GateDetect {
    pub embedding: Vec<f32>,
    /// True when YuNet alignment was applied; false when the resize fallback ran.
    pub aligned: bool,
    pub image_w: u32,
    pub image_h: u32,
    /// All faces >= `DET_THRESHOLD`, post-NMS, sorted by score descending.
    pub faces: Vec<Face>,
    /// The decoded image, kept so curation metadata (face-crop sharpness,
    /// hair heuristic) computes without a second decode (WP-019).
    pub image: RgbImage,
}

/// The YuNet 2023mar float model (OpenCV Zoo, MIT — license vendored beside
/// the asset) compiled into the binary as the DEFAULT detector (WP-020).
/// `identity_detector_path` / `FACIAL_IDENTITY_DETECTOR` override it.
pub const BUNDLED_YUNET: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/models/face_detection_yunet_2023mar.onnx"
));

pub struct IdentityEngine {
    model: TypedRunnableModel<TypedModel>,
    model_path: PathBuf,
    model_sha256: String,
    detector: Option<Detector>,
    /// Where the active detector came from: "override" (configured path),
    /// "bundled" (compiled-in YuNet), "bundled_fallback" (configured path
    /// failed to load, bundled took over), or "none" (resize alignment).
    detector_origin: &'static str,
    detector_sha256: Option<String>,
}

impl IdentityEngine {
    /// Load an ArcFace-style embedder ONNX plus the face detector. Detector
    /// resolution order (WP-020): configured path -> bundled YuNet -> none
    /// (resize fallback). An embedder that fails to load returns Err (engine
    /// stays disabled); detector failures degrade with an explicit origin.
    pub fn load(model_path: &Path, detector_path: Option<&Path>) -> Result<Self, String> {
        let bytes = std::fs::read(model_path).map_err(|e| format!("read model: {e}"))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let model_sha256 = format!("{:x}", hasher.finalize());
        let model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(&bytes))
            .map_err(|e| format!("parse onnx: {e}"))?
            .into_optimized()
            .map_err(|e| format!("optimize onnx: {e}"))?
            .into_runnable()
            .map_err(|e| format!("make runnable: {e}"))?;

        let bundled = || -> Option<(Detector, String)> {
            let det = Detector::load_from_bytes(BUNDLED_YUNET).ok()?;
            let mut h = Sha256::new();
            h.update(BUNDLED_YUNET);
            Some((det, format!("{:x}", h.finalize())))
        };
        let (detector, detector_origin, detector_sha256) = match detector_path {
            Some(p) => match std::fs::read(p) {
                Ok(det_bytes) => match Detector::load_from_bytes(&det_bytes) {
                    Ok(det) => {
                        let mut h = Sha256::new();
                        h.update(&det_bytes);
                        (Some(det), "override", Some(format!("{:x}", h.finalize())))
                    }
                    Err(_) => match bundled() {
                        Some((det, sha)) => (Some(det), "bundled_fallback", Some(sha)),
                        None => (None, "none", None),
                    },
                },
                Err(_) => match bundled() {
                    Some((det, sha)) => (Some(det), "bundled_fallback", Some(sha)),
                    None => (None, "none", None),
                },
            },
            None => match bundled() {
                Some((det, sha)) => (Some(det), "bundled", Some(sha)),
                None => (None, "none", None),
            },
        };

        Ok(Self {
            model,
            model_path: model_path.to_path_buf(),
            model_sha256,
            detector,
            detector_origin,
            detector_sha256,
        })
    }

    pub fn detector_origin(&self) -> &'static str {
        self.detector_origin
    }

    pub fn detector_sha256(&self) -> Option<&str> {
        self.detector_sha256.as_deref()
    }

    pub fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn has_detector(&self) -> bool {
        self.detector.is_some()
    }

    pub fn align_method(&self) -> &'static str {
        if self.detector.is_some() {
            "yunet_112"
        } else {
            "resize_112"
        }
    }

    /// Deterministic embedding: align (YuNet or resize) -> 112x112 ->
    /// normalize (x-127.5)/128 -> tract -> L2-normalize.
    pub fn embed_file(&self, image_path: &Path) -> Result<Vec<f32>, String> {
        Ok(self.embed_with_detection(image_path)?.embedding)
    }

    /// Embed, reporting whether YuNet alignment was actually applied (true) or
    /// the resize fallback was used (false) for this specific image.
    pub fn embed_file_detail(&self, image_path: &Path) -> Result<(Vec<f32>, bool), String> {
        let d = self.embed_with_detection(image_path)?;
        Ok((d.embedding, d.aligned))
    }

    /// Embed and return the face detections (box/count/landmarks) in one pass.
    /// The detector runs once; alignment uses the top face, and every face
    /// >= `DET_THRESHOLD` (post-NMS) is returned for the caller to gate on.
    pub fn embed_with_detection(&self, image_path: &Path) -> Result<GateDetect, String> {
        let img = image::open(image_path)
            .map_err(|e| format!("decode {}: {e}", image_path.display()))?
            .to_rgb8();
        let (image_w, image_h) = img.dimensions();
        let (aligned, scrfd, faces) = self.detect_and_align(&img);
        let embedding = self.embed_aligned(&aligned)?;
        Ok(GateDetect {
            embedding,
            aligned: scrfd,
            image_w,
            image_h,
            faces,
            image: img,
        })
    }

    /// Detect all faces once, align from the highest-scoring one (YuNet path),
    /// or fall back to a whole-image resize. Returns the aligned 112x112 crop,
    /// whether YuNet alignment was used, and every detected face.
    fn detect_and_align(&self, img: &RgbImage) -> (RgbImage, bool, Vec<Face>) {
        if let Some(detector) = &self.detector {
            let faces = detector.detect_all(img, DET_THRESHOLD);
            if let Some(top) = faces.first() {
                if let Some(aligned) = align_112(img, &top.landmarks) {
                    return (aligned, true, faces);
                }
            }
            return (
                image::imageops::resize(img, 112, 112, image::imageops::FilterType::Triangle),
                false,
                faces,
            );
        }
        (
            image::imageops::resize(img, 112, 112, image::imageops::FilterType::Triangle),
            false,
            Vec::new(),
        )
    }

    fn embed_aligned(&self, img: &RgbImage) -> Result<Vec<f32>, String> {
        let mut data = vec![0f32; 3 * 112 * 112];
        for y in 0..112usize {
            for x in 0..112usize {
                let px = img.get_pixel(x as u32, y as u32);
                for c in 0..3usize {
                    data[c * 112 * 112 + y * 112 + x] = (px[c] as f32 - 127.5) / 128.0;
                }
            }
        }
        let input = Tensor::from_shape(&[1, 3, 112, 112], &data)
            .map_err(|e| format!("build tensor: {e}"))?;
        let result = self
            .model
            .run(tvec!(input.into()))
            .map_err(|e| format!("inference: {e}"))?;
        let view = result[0]
            .to_array_view::<f32>()
            .map_err(|e| format!("read output: {e}"))?;
        let mut emb: Vec<f32> = view.iter().copied().collect();
        l2_normalize(&mut emb);
        Ok(emb)
    }
}

/// YuNet face detector (OpenCV Zoo 2023mar: 12 outputs, 3 strides [8,16,32],
/// 1 anchor/cell; cls[0..3], obj[3..6], bbox[6..9], kps[9..12]; raw-0-255 BGR).
struct Detector {
    model: TypedRunnableModel<TypedModel>,
}

impl Detector {
    /// Load a YuNet detector from raw ONNX bytes (file or bundled), with a
    /// startup self-check: the model must execute on a blank frame and expose
    /// the 12-output 2023mar layout our decode expects — a wrong export can
    /// never silently produce wrong geometry.
    fn load_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(bytes))
            .map_err(|e| format!("parse detector onnx: {e}"))?
            .with_input_fact(
                0,
                f32::fact([1, 3, DET_INPUT as i32, DET_INPUT as i32]).into(),
            )
            .map_err(|e| format!("detector input fact: {e}"))?
            .into_optimized()
            .map_err(|e| format!("optimize detector: {e}"))?
            .into_runnable()
            .map_err(|e| format!("detector runnable: {e}"))?;
        let detector = Self { model };
        detector.self_check()?;
        Ok(detector)
    }

    /// Run a blank frame through the model and verify the output layout.
    fn self_check(&self) -> Result<(), String> {
        let blob = vec![0f32; 3 * DET_INPUT * DET_INPUT];
        let input = Tensor::from_shape(&[1, 3, DET_INPUT, DET_INPUT], &blob)
            .map_err(|e| format!("self-check tensor: {e}"))?;
        let out = self
            .model
            .run(tvec!(input.into()))
            .map_err(|e| format!("detector self-check inference: {e}"))?;
        if out.len() < 12 {
            return Err(format!(
                "detector output layout mismatch: {} outputs, expected >= 12 (YuNet 2023mar)",
                out.len()
            ));
        }
        Ok(())
    }

    /// Detect every face with score >= `min_score`, decoded into the ORIGINAL
    /// image's pixel space, de-duplicated by greedy IoU NMS, sorted by score
    /// descending. Empty when no face clears the threshold.
    fn detect_all(&self, img: &RgbImage, min_score: f32) -> Vec<Face> {
        let (ow, oh) = img.dimensions();
        if ow == 0 || oh == 0 {
            return Vec::new();
        }
        // Letterbox into DET_INPUT x DET_INPUT, preserving aspect ratio.
        let im_ratio = oh as f32 / ow as f32;
        let (new_w, new_h) = if im_ratio > 1.0 {
            let nh = DET_INPUT as f32;
            (((nh / im_ratio).round() as u32).max(1), nh as u32)
        } else {
            let nw = DET_INPUT as f32;
            (nw as u32, ((nw * im_ratio).round() as u32).max(1))
        };
        let det_scale = new_h as f32 / oh as f32;
        let resized =
            image::imageops::resize(img, new_w, new_h, image::imageops::FilterType::Triangle);

        let mut blob = vec![0f32; 3 * DET_INPUT * DET_INPUT];
        for y in 0..new_h.min(DET_INPUT as u32) {
            for x in 0..new_w.min(DET_INPUT as u32) {
                let px = resized.get_pixel(x, y);
                // YuNet expects raw 0-255 pixels in BGR channel order (OpenCV).
                let plane = DET_INPUT * DET_INPUT;
                let off = (y as usize) * DET_INPUT + (x as usize);
                blob[off] = px[2] as f32; // B
                blob[plane + off] = px[1] as f32; // G
                blob[2 * plane + off] = px[0] as f32; // R
            }
        }
        let input = match Tensor::from_shape(&[1, 3, DET_INPUT, DET_INPUT], &blob) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out = match self.model.run(tvec!(input.into())) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        if out.len() < 12 {
            return Vec::new();
        }

        // YuNet 2023mar layout (confirmed via output shapes): per stride i in 0..3
        //   cls=out[i], obj=out[i+3], bbox=out[i+6], kps=out[i+9].
        // 1 anchor/cell; grid = sqrt(rows); stride = DET_INPUT / grid.
        // Decode (matches OpenCV FaceDetectorYN postprocess):
        //   score = sqrt(clamp01(cls)*clamp01(obj));
        //   cx = (col+dx)*stride; cy = (row+dy)*stride;       (linear)
        //   w  = exp(dw)*stride;  h  = exp(dh)*stride;         (EXPONENTIAL)
        //   landmark = (col/row + delta)*stride;
        // then divide by det_scale to return to original image pixels.
        let plane = |idx: usize| -> Vec<f32> {
            out[idx]
                .to_array_view::<f32>()
                .map(|v| v.iter().copied().collect())
                .unwrap_or_default()
        };
        let floor = min_score.max(0.0);
        let mut faces: Vec<Face> = Vec::new();
        for i in 0..3 {
            let cls = plane(i);
            let obj = plane(i + 3);
            let bbox = plane(i + 6);
            let kps = plane(i + 9);
            let n = cls.len();
            if n == 0 || obj.len() < n || bbox.len() < n * 4 || kps.len() < n * 10 {
                continue;
            }
            let grid = (n as f64).sqrt().round() as usize;
            // 1 anchor/cell on a square grid; skip if the layout isn't square.
            if grid == 0 || grid * grid != n {
                continue;
            }
            let stride = (DET_INPUT / grid) as f32;
            for r in 0..n {
                let score = (cls[r].max(0.0).min(1.0) * obj[r].max(0.0).min(1.0)).sqrt();
                if score < floor {
                    continue;
                }
                let row = (r / grid) as f32;
                let col = (r % grid) as f32;
                let cx = (col + bbox[r * 4]) * stride;
                let cy = (row + bbox[r * 4 + 1]) * stride;
                let w = bbox[r * 4 + 2].exp() * stride;
                let h = bbox[r * 4 + 3].exp() * stride;
                if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
                    continue;
                }
                let mut pts = [[0f32; 2]; 5];
                for k in 0..5 {
                    pts[k][0] = (col + kps[r * 10 + 2 * k]) * stride / det_scale;
                    pts[k][1] = (row + kps[r * 10 + 2 * k + 1]) * stride / det_scale;
                }
                // box in original pixels, clamped to image bounds.
                let mut x = (cx - w / 2.0) / det_scale;
                let mut y = (cy - h / 2.0) / det_scale;
                let mut bw = w / det_scale;
                let mut bh = h / det_scale;
                if x < 0.0 {
                    bw += x;
                    x = 0.0;
                }
                if y < 0.0 {
                    bh += y;
                    y = 0.0;
                }
                bw = bw.min(ow as f32 - x).max(0.0);
                bh = bh.min(oh as f32 - y).max(0.0);
                if bw <= 0.0 || bh <= 0.0 {
                    continue;
                }
                faces.push(Face {
                    bbox: [x, y, bw, bh],
                    score,
                    landmarks: pts,
                });
            }
        }
        nms(faces, NMS_THRESHOLD)
    }
}

/// Greedy IoU non-max suppression. Deterministic: sort by score descending with
/// a stable index tiebreak, then keep boxes that don't overlap a kept box by
/// more than `iou_threshold`.
fn nms(mut faces: Vec<Face>, iou_threshold: f32) -> Vec<Face> {
    // Stable sort by score desc; equal scores keep original (anchor) order.
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<Face> = Vec::new();
    for f in faces {
        if kept.iter().all(|k| iou(&k.bbox, &f.bbox) <= iou_threshold) {
            kept.push(f);
        }
    }
    kept
}

/// Intersection-over-union of two [x, y, w, h] boxes.
fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let (ax2, ay2) = (a[0] + a[2], a[1] + a[3]);
    let (bx2, by2) = (b[0] + b[2], b[1] + b[3]);
    let ix1 = a[0].max(b[0]);
    let iy1 = a[1].max(b[1]);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Align a face to a 112x112 ArcFace crop using a least-squares similarity
/// transform from the 5 landmarks to the canonical template.
fn align_112(img: &RgbImage, landmarks: &[[f32; 2]; 5]) -> Option<RgbImage> {
    let [a, b, tx, ty] = solve_similarity(landmarks, &ARCFACE_DST)?;
    let det = a * a + b * b;
    if det.abs() < 1e-9 {
        return None;
    }
    let (w, h) = img.dimensions();
    let mut out = RgbImage::new(112, 112);
    for oy in 0..112u32 {
        for ox in 0..112u32 {
            // invert the similarity: src = L^-1 * (dst - t)
            let ddx = ox as f32 - tx;
            let ddy = oy as f32 - ty;
            let sx = (a * ddx + b * ddy) / det;
            let sy = (-b * ddx + a * ddy) / det;
            let px = bilinear(img, sx, sy, w, h);
            out.put_pixel(ox, oy, image::Rgb(px));
        }
    }
    Some(out)
}

/// Bilinear sample; out-of-bounds returns black.
fn bilinear(img: &RgbImage, sx: f32, sy: f32, w: u32, h: u32) -> [u8; 3] {
    if sx < 0.0 || sy < 0.0 || sx > (w - 1) as f32 || sy > (h - 1) as f32 {
        return [0, 0, 0];
    }
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = sx - x0 as f32;
    let fy = sy - y0 as f32;
    let p00 = img.get_pixel(x0, y0);
    let p10 = img.get_pixel(x1, y0);
    let p01 = img.get_pixel(x0, y1);
    let p11 = img.get_pixel(x1, y1);
    let mut out = [0u8; 3];
    for c in 0..3usize {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// Least-squares similarity transform mapping src -> dst as
/// (x,y) -> (a*x - b*y + tx, b*x + a*y + ty). Returns [a, b, tx, ty].
fn solve_similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> Option<[f32; 4]> {
    // Normal equations A^T A p = A^T b, where each point gives two rows:
    //   [ x, -y, 1, 0 ] . p = X
    //   [ y,  x, 0, 1 ] . p = Y
    let mut ata = [[0f64; 4]; 4];
    let mut atb = [0f64; 4];
    for i in 0..5 {
        let (x, y) = (src[i][0] as f64, src[i][1] as f64);
        let (xx, yy) = (dst[i][0] as f64, dst[i][1] as f64);
        let rows = [([x, -y, 1.0, 0.0], xx), ([y, x, 0.0, 1.0], yy)];
        for (coeff, rhs) in rows {
            for r in 0..4 {
                for c in 0..4 {
                    ata[r][c] += coeff[r] * coeff[c];
                }
                atb[r] += coeff[r] * rhs;
            }
        }
    }
    let p = solve4(ata, atb)?;
    Some([p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32])
}

/// Solve a 4x4 linear system by Gaussian elimination with partial pivoting.
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        // pivot
        let mut piv = col;
        for r in (col + 1)..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        // eliminate
        for r in 0..4 {
            if r == col {
                continue;
            }
            let f = a[r][col] / a[col][col];
            for c in col..4 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0f64; 4];
    for i in 0..4 {
        x[i] = b[i] / a[i][i];
    }
    Some(x)
}

// ---------------------------------------------------------------------------
// Curation metadata, wave 1 (WP-019): face-crop sharpness, coarse yaw bucket,
// hair-color heuristic. Sharpness/yaw derive from real detector geometry
// (`source: real`); the hair flag is an HSV heuristic (`source: proxy`).
// ---------------------------------------------------------------------------

/// Laplacian variance over a region (the standard focus measure): higher =
/// sharper. `bbox` (x, y, w, h in pixels) restricts to the face crop; `None`
/// measures the whole image.
pub fn laplacian_variance(img: &RgbImage, bbox: Option<[f32; 4]>) -> f32 {
    let (iw, ih) = img.dimensions();
    if iw < 3 || ih < 3 {
        return 0.0;
    }
    let (x0, y0, x1, y1) = match bbox {
        Some([bx, by, bw, bh]) => {
            let x0 = bx.max(0.0) as u32;
            let y0 = by.max(0.0) as u32;
            let x1 = ((bx + bw).min(iw as f32)) as u32;
            let y1 = ((by + bh).min(ih as f32)) as u32;
            (x0, y0, x1, y1)
        }
        None => (0, 0, iw, ih),
    };
    if x1.saturating_sub(x0) < 3 || y1.saturating_sub(y0) < 3 {
        return 0.0;
    }
    let gray = |x: u32, y: u32| -> f32 {
        let p = img.get_pixel(x, y);
        0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
    };
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for y in (y0 + 1)..(y1 - 1) {
        for x in (x0 + 1)..(x1 - 1) {
            let lap = 4.0 * gray(x, y)
                - gray(x - 1, y)
                - gray(x + 1, y)
                - gray(x, y - 1)
                - gray(x, y + 1);
            sum += lap as f64;
            sum_sq += (lap as f64) * (lap as f64);
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    let mean = sum / n as f64;
    ((sum_sq / n as f64) - mean * mean).max(0.0) as f32
}

/// Coarse yaw bucket from the 5-point landmarks: compares the nose's
/// horizontal position between the eyes. Returns `(bucket, eye_nose_ratio)`
/// where ratio = min(d_left, d_right)/max(..) in [0,1] (1 = perfectly
/// frontal). Buckets only — 5 points cannot support precise pose angles.
pub fn yaw_bucket(landmarks: &[[f32; 2]; 5]) -> (&'static str, f32) {
    let left_eye = landmarks[0];
    let right_eye = landmarks[1];
    let nose = landmarks[2];
    let d_left = nose[0] - left_eye[0];
    let d_right = right_eye[0] - nose[0];
    // Nose outside the eye span = strong profile regardless of ratio.
    if d_left <= 0.0 || d_right <= 0.0 {
        return ("profile", 0.0);
    }
    let ratio = d_left.min(d_right) / d_left.max(d_right);
    if ratio >= 0.55 {
        ("frontal", ratio)
    } else if ratio >= 0.25 {
        ("quarter", ratio)
    } else {
        ("profile", ratio)
    }
}

/// Dominant hair-color flag from the strip above the face box (HSV heuristic,
/// `source: proxy`). Returns `(label, confidence)` where confidence is the
/// winning bucket's pixel share in [0,1]. Targets triage (e.g. surfacing pink
/// wigs), never gating.
pub fn hair_color_flag(img: &RgbImage, bbox: [f32; 4]) -> (&'static str, f32) {
    let (iw, ih) = img.dimensions();
    let [bx, by, bw, bh] = bbox;
    // Strip: face width +15% each side, from half a face-height above the box
    // down to the box top.
    let x0 = (bx - bw * 0.15).max(0.0) as u32;
    let x1 = ((bx + bw * 1.15).min(iw as f32)) as u32;
    let y0 = (by - bh * 0.5).max(0.0) as u32;
    let y1 = by.max(0.0).min(ih as f32) as u32;
    if x1 <= x0 || y1 <= y0 {
        return ("unknown", 0.0);
    }
    let mut counts: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut total = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = img.get_pixel(x, y);
            let (h, s, v) = rgb_to_hsv(p[0], p[1], p[2]);
            let label = classify_hair_pixel(h, s, v);
            *counts.entry(label).or_insert(0) += 1;
            total += 1;
        }
    }
    if total == 0 {
        return ("unknown", 0.0);
    }
    let (label, count) = counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .unwrap_or(("unknown", 0));
    (label, count as f32 / total as f32)
}

/// HSV: h in degrees [0,360), s and v in [0,1].
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max < 1e-6 { 0.0 } else { delta / max };
    (h, s, max)
}

/// One pixel's hair bucket. Saturated pixels classify by hue; desaturated by
/// value. Buckets chosen for curation triage (wig/dye detection), not beauty.
fn classify_hair_pixel(h: f32, s: f32, v: f32) -> &'static str {
    if v < 0.15 {
        return "black";
    }
    if s < 0.14 {
        return if v > 0.65 { "gray_white" } else { "brown" };
    }
    if s >= 0.18 {
        match h {
            x if (290.0..345.0).contains(&x) => return "pink_purple",
            x if !(15.0..345.0).contains(&x) => return "red",
            x if (15.0..45.0).contains(&x) && v < 0.55 => return "brown",
            x if (15.0..70.0).contains(&x) => return "blonde",
            x if (70.0..170.0).contains(&x) => return "green",
            x if (170.0..260.0).contains(&x) => return "blue",
            x if (260.0..290.0).contains(&x) => return "pink_purple",
            _ => {}
        }
    }
    if v < 0.45 {
        "brown"
    } else {
        "other"
    }
}

/// Cosine similarity of two L2-normalized vectors (their dot product).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0f32;
    for i in 0..n {
        dot += a[i] * b[i];
    }
    dot
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Greedy threshold clustering over L2-normalized embeddings (WP-018).
/// Deterministic: inputs are visited in order; an item joins the FIRST
/// existing cluster whose representative (first member) it matches at
/// `cosine >= threshold`, else founds a new cluster. Returns the cluster
/// index per input, numbered by order of founding.
pub fn cluster_embeddings(embeddings: &[Vec<f32>], threshold: f32) -> Vec<usize> {
    let mut assignment = Vec::with_capacity(embeddings.len());
    let mut representatives: Vec<usize> = Vec::new();
    for (i, emb) in embeddings.iter().enumerate() {
        let mut joined = None;
        for (cluster_idx, &rep) in representatives.iter().enumerate() {
            if cosine(emb, &embeddings[rep]) >= threshold {
                joined = Some(cluster_idx);
                break;
            }
        }
        match joined {
            Some(cluster_idx) => assignment.push(cluster_idx),
            None => {
                representatives.push(i);
                assignment.push(representatives.len() - 1);
            }
        }
    }
    assignment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_yunet_loads_and_passes_self_check() {
        // Compiled-in YuNet must parse under tract, execute on a blank frame,
        // and expose the 12-output 2023mar layout (WP-020 startup guard).
        let det = Detector::load_from_bytes(BUNDLED_YUNET);
        assert!(det.is_ok(), "bundled YuNet failed: {:?}", det.err());
    }

    #[test]
    fn cluster_embeddings_groups_by_threshold() {
        // Three orthogonal-ish groups in 3D (unit vectors).
        let e = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.999, 0.0447, 0.0], // ~0.999 sim to [0]
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.9988, 0.0499], // ~0.999 sim to [2]
            vec![0.0, 0.0, 1.0],
        ];
        let clusters = cluster_embeddings(&e, 0.95);
        assert_eq!(clusters, vec![0, 0, 1, 1, 2]);
        // Stricter threshold splits everything.
        let clusters = cluster_embeddings(&e, 0.9999);
        assert_eq!(clusters, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn laplacian_variance_orders_sharp_above_blurred() {
        // Checkerboard = maximal high-frequency energy; flat = zero.
        let mut sharp = RgbImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let v = if (x + y) % 2 == 0 { 255u8 } else { 0u8 };
                sharp.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        let flat = RgbImage::from_pixel(32, 32, image::Rgb([128, 128, 128]));
        let s = laplacian_variance(&sharp, None);
        let f = laplacian_variance(&flat, None);
        assert!(s > 10_000.0, "checkerboard variance was {s}");
        assert_eq!(f, 0.0);
        // Restricting to a bbox works and stays finite.
        let cropped = laplacian_variance(&sharp, Some([4.0, 4.0, 16.0, 16.0]));
        assert!(cropped > 10_000.0);
        // Degenerate bbox -> 0, no panic.
        assert_eq!(
            laplacian_variance(&sharp, Some([30.0, 30.0, 1.0, 1.0])),
            0.0
        );
    }

    #[test]
    fn yaw_bucket_classifies_geometry() {
        // Frontal: nose centered between the eyes.
        let frontal = [
            [30.0, 40.0],
            [70.0, 40.0],
            [50.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        assert_eq!(yaw_bucket(&frontal).0, "frontal");
        // Quarter: nose clearly off-center.
        let quarter = [
            [30.0, 40.0],
            [70.0, 40.0],
            [38.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        assert_eq!(yaw_bucket(&quarter).0, "quarter");
        // Profile: nose outside the eye span.
        let profile = [
            [30.0, 40.0],
            [70.0, 40.0],
            [25.0, 55.0],
            [35.0, 70.0],
            [65.0, 70.0],
        ];
        let (bucket, ratio) = yaw_bucket(&profile);
        assert_eq!(bucket, "profile");
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn hair_color_flag_spots_the_pink_wig() {
        // Image: pink strip above the face box, skin-ish inside it.
        let mut img = RgbImage::from_pixel(100, 100, image::Rgb([230, 120, 190])); // pink
        for y in 50..100 {
            for x in 20..80 {
                img.put_pixel(x, y, image::Rgb([210, 170, 140])); // skin-ish
            }
        }
        let (label, confidence) = hair_color_flag(&img, [20.0, 50.0, 60.0, 50.0]);
        assert_eq!(label, "pink_purple");
        assert!(confidence > 0.9, "confidence was {confidence}");

        // Black hair.
        let mut img = RgbImage::from_pixel(100, 100, image::Rgb([18, 16, 15]));
        for y in 50..100 {
            for x in 20..80 {
                img.put_pixel(x, y, image::Rgb([210, 170, 140]));
            }
        }
        let (label, _) = hair_color_flag(&img, [20.0, 50.0, 60.0, 50.0]);
        assert_eq!(label, "black");

        // Degenerate box at the image top -> unknown, no panic.
        let (label, confidence) = hair_color_flag(&img, [0.0, 0.0, 10.0, 0.0]);
        assert_eq!(label, "unknown");
        assert_eq!(confidence, 0.0);
    }

    #[test]
    fn rgb_to_hsv_sanity() {
        let (h, s, v) = rgb_to_hsv(255, 0, 0);
        assert!(h.abs() < 1.0 && (s - 1.0).abs() < 1e-5 && (v - 1.0).abs() < 1e-5);
        let (_, s, v) = rgb_to_hsv(128, 128, 128);
        assert!(s < 1e-5 && (v - 0.50196).abs() < 1e-3);
    }
}
