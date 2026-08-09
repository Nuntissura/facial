---
file_id: research_face_identity
file_kind: research-basis
updated_at: 2026-06-10-b
---

<topic id="scope" summary="Why this research exists">

## Scope

Research basis for adding real, deterministic face identity to `facial` (Option A:
pluggable ONNX, model provisioned at runtime, never bundled or downloaded by the
app). The app is a testbed for a Handshake harness tool that models must use
mechanically and deterministically, so the deciding criterion is reproducibility
+ pure-Rust self-containment, not raw speed. Lightweight inline pass (operator
requested token economy), confirmed against current sources.

</topic>

<topic id="sources" summary="Sources checked">

## Sources checked

- Rust ML/edge framework comparison (tract vs ort vs candle), 2025.
- InsightFace model guides + repo (buffalo_l, SCRFD, ArcFace w600k).
- yakhyo/face-reidentification (SCRFD + ArcFace + ONNX reference pipeline).
- ArcFace preprocessing/cosine references (112x112, (x-127.5)/128, L2-norm + cosine).
- HF garavv/arcface-onnx, OpenVINO open_model_zoo arcface README.

Links: calmops Rust ML frameworks 2025; ort.pyke.io/backends; github.com/pykeio/ort;
insightface.ai guides + research/scrfd + research/arcface; github.com/yakhyo/face-reidentification;
huggingface.co/garavv/arcface-onnx.

</topic>

<topic id="decision" summary="Selected engine, models, and method">

## Selected approach

**Inference engine: `tract`** (pure-Rust ONNX, no C/C++ runtime). Chosen for
deterministic CPU inference and zero native/system dependencies — matches
"completely in Rust, no external runtime deps".

**Models (pluggable, provisioned by harness, not bundled):**
- Detector/aligner: **SCRFD** ONNX — `det_2.5g.onnx` (~3 MB) default; `det_500m`
  (~2.4 MB) for speed, `det_10g` (~16 MB) for accuracy. Provides bbox + 5 landmarks.
- Embedder: **ArcFace w600k** — `w600k_r50.onnx` (ResNet50, ~166 MB) default for
  accuracy; `w600k_mbf.onnx` (MobileFace, ~13 MB) as the light option. Output: 512-d.

**Method (fixed pipeline):** detect face -> 5-point similarity-transform align to
112x112 -> normalize `(x-127.5)/128.0` -> embed -> L2-normalize -> cosine similarity
(dot of normalized vectors). Identity gate = cosine vs a labelled **reference set**
and a **negative set**, with a fixed threshold + required margin -> deterministic
verdict (`match` / `no_match` / `unsure`) + scores.

</topic>

<topic id="rejected" summary="Options considered and rejected">

## Rejected options

- **ort (ONNX Runtime bindings):** fastest, but wraps Microsoft's native C++
  runtime (system dependency, execution providers) — violates pure-Rust /
  self-contained constraint. Rejected.
- **candle:** pure-Rust and capable, but oriented to the HF/LLM ecosystem and GPU;
  heavier than needed for a small fixed face model where determinism is the goal.
  Rejected in favor of tract (kept as a fallback if tract lacks an op).
- **Bundling weights in-repo (Option B):** rejected to keep the testbed light;
  harness provisions the ONNX as a versioned asset.
- **VLM / LLM vision judge:** non-deterministic, slow, unauditable — disqualified by
  the harness's mechanical-determinism requirement.

</topic>

<topic id="risks" summary="Risks and mitigations">

## Risks + mitigations

- **tract op coverage:** some SCRFD/ArcFace exports use ops tract may not support.
  Mitigation: test-load the exact ONNX at integration; if an op is missing, switch
  variant or fall back to candle for that model. Validate before claiming support.
- **Alignment determinism:** the 5-point affine warp + resize filter must be pinned
  (fixed landmarks, fixed interpolation) or embeddings drift. Mitigation: implement
  one fixed alignment path; document it; no configurable resize filter on this path.
- **Cross-machine float reproducibility:** pin the tract version and record it; CPU
  float ops are stable for a fixed version. Mitigation: stamp model id + sha256 +
  tract version + thresholds into every identity artifact for audit.
- **Threshold calibration:** a wrong cutoff misclassifies. Mitigation: thresholds in
  config, reported in receipts; default from InsightFace guidance, operator-tunable.
- **No model provisioned:** never fake a verdict. Mitigation: emit
  `identity: unavailable` and keep cull+quality fully functional.

</topic>

<topic id="validation" summary="How identity will be verified">

## Validation plan

1. Load detector + embedder ONNX via tract; confirm op support (build proof).
2. Same-person pair -> high cosine; different-person pair -> low cosine (correctness).
3. Run the same image twice -> byte-identical embedding (determinism proof).
4. No model present -> `identity: unavailable`, cull+quality unaffected (graceful).
5. Reference vs negative set -> deterministic verdict + margin in JSON (gate proof).

Full ArcFace accuracy is verifiable only once a real model is provisioned; plumbing
+ determinism are provable now with a tiny test ONNX.

</topic>

<topic id="batch-gate-scope" wp="WP-007" summary="Why the batch-gate + face-box/count research exists">

## Scope (WP-007: batch identity gate + YuNet box/count exposure)

Live-environment model feedback: the single-image `identity_gate` forces the model
to call once per image and to bolt `cv2` alongside facial to get a face box and
face count for **scale-bucketing** (crop size relative to image) and
**collage-rejection** (drop multi-face grids). Two asks: (1) a batch/folder gate
that gates a directory in one call and emits a `runs/<run_id>/` CSV + manifest +
receipt; (2) expose the YuNet face bounding box + face count on gate output so the
model can do the whole sort in one Rust tool with no `cv2` side-car. Research basis
recorded before build per the project research-first gate.

</topic>

<topic id="batch-gate-sources" wp="WP-007" summary="Sources checked for decode + curation patterns">

## Sources checked

- **`_source_checks/deepface/.../YuNet.py`** (local) — field implementation delegates
  to `cv2.FaceDetectorYN_create`; raw ONNX decode + NMS are hidden inside OpenCV C++,
  so the Python field code is NOT the decode authority. Confirms output row format:
  `x1,y1,w,h, x_re,y_re, x_le,y_le, x_nt,y_nt, x_rcm,y_rcm, x_lcm,y_lcm, score`.
- **OpenCV C++ `modules/objdetect/src/face_detect.cpp`** (`FaceDetectorYN` internals) —
  authoritative raw decode + NMS for the 2023mar ONNX. Quoted formulas below.
- OpenCV Zoo issue #192 (raw I/O processing — an open doc-gap, not a spec), `demo.py`,
  HF `opencv/face_detection_yunet` model card — confirm 12 outputs
  (`cls/obj/bbox/kps` × strides 8/16/32), defaults `score=0.9`, `nms=0.3`, `topK=5000`.
- Curation-filter field basis: arXiv 2401.12225 (object-detection + **object-count**
  filter ensembles for dataset curation), arXiv 1705.02402 (bbox aggregation; example
  filter criteria: drop confidence < 0.85, height < 1/5 image height, box not covering
  center), WIDER FACE (tiny-face regime — small boxes are the hard/low-value tail).

Links: github.com/opencv/opencv `4.x/modules/objdetect/src/face_detect.cpp`;
github.com/opencv/opencv_zoo issues/192 + models/face_detection_yunet/demo.py;
huggingface.co/opencv/face_detection_yunet; arxiv.org/pdf/2401.12225;
arxiv.org/pdf/1705.02402.

</topic>

<topic id="batch-gate-decode" wp="WP-007" summary="Authoritative YuNet decode + NMS vs current Rust">

## Decode findings (from OpenCV `face_detect.cpp`, verbatim semantics)

Per stride `i` ∈ {8,16,32}, grid cell `(c=col, r=row)`, anchor index `idx`:

```
score = sqrt( clamp01(cls) * clamp01(obj) )          # geometric mean
cx = (c + bbox[idx*4+0]) * stride                     # linear
cy = (r + bbox[idx*4+1]) * stride                     # linear
w  = exp(bbox[idx*4+2]) * stride                      # EXPONENTIAL  <-- gotcha
h  = exp(bbox[idx*4+3]) * stride                      # EXPONENTIAL  <-- gotcha
x1 = cx - w/2 ;  y1 = cy - h/2
kps_n = ( kps[idx*10+2n] + c ) * stride , ( kps[idx*10+2n+1] + r ) * stride
if score < scoreThreshold: continue
NMS: cv::dnn::NMSBoxes(boxes, scores, scoreThreshold, nmsThreshold=0.3, eta=1.0, topK=5000)
```

**Cross-check vs current `product/src/identity.rs` `Detector::detect`:**
- `score = sqrt(cls.max(0)*obj.max(0))` — **already matches** OpenCV. ✓
- kps decode `(col + kps)*stride`, `(row + kps)*stride` — **already matches**. ✓ (so
  current alignment is correct; this work does not change embeddings).
- bbox branch (`out[i+6]`) is **never read today** — must be added; w/h need `exp()`,
  NOT the linear form used for kps. This is the single most error-prone line.
- **No NMS today** — `detect` keeps only the argmax face, which sidesteps NMS. An
  honest `face_count` REQUIRES greedy IoU NMS (`nmsThreshold=0.3`); without it one
  face fires across overlapping anchors and the count is inflated garbage.
- Grid uses `grid=round(sqrt(n))` assuming a square grid — valid only because facial
  letterboxes to a square `DET_INPUT=640` (80²/40²/20²). Invariant to guard if input
  ever becomes non-square.
- **Threshold mismatch:** current `DET_THRESHOLD=0.6` vs OpenCV/deepface default
  `0.9`. 0.6 is fine for picking one face to align, but it inflates `face_count` with
  marginal detections — count semantics depend on this value.

</topic>

<topic id="batch-gate-decision" wp="WP-007" summary="Selected approach for box/count + batch">

## Selected approach

**Detector (`identity.rs`):** add the bbox decode branch (read `out[i+6]`, `exp()` on
w/h, derive `x1,y1,w,h` in letterboxed space then divide by `det_scale` back to
original px like kps already do) + a deterministic greedy IoU NMS (`nms=0.3`, sort by
score desc with stable index tiebreak for reproducibility). Change `detect` to return
a list of `{box, landmarks, score}` for all faces ≥ count-threshold; keep the
highest-score post-NMS face for the existing alignment path (embeddings unchanged).

**Gate output (single + batch share one shape):** add `face_count`, `face_box`
(top face, original px, empty if none), `face_frac = box_area/image_area` (the
scale-bucket hint, computed once in Rust). Extend verdict vocab: `no_face`
(count==0 → collage's inverse / garbage), `error` (decode/inference failed; batch
continues). `align` already reports yunet_112/resize_112.

**Count threshold:** make it an explicit config key (default field value **0.9**, not
the 0.6 alignment threshold) and **stamp the value into manifest + receipt** so the
count is auditable and tunable, satisfying the determinism mandate.

**Batch (`identity_gate_dir`):** new command; reuse the already-cached reference/
negative embeddings (loaded once); iterate the directory (top-level default, future
`recursive` flag); stream rows to `runs/<run_id>/identity_gate.csv` incrementally;
write `manifest.json` (superset + summary); receipt carries `run_id`, artifact paths,
and summary counts. Per-image error isolation: a bad file becomes an `error` row, not
a batch abort (unlike today's `embed_file_detail`, which bails).

</topic>

<topic id="batch-gate-rejected" wp="WP-007" summary="Options rejected">

## Rejected options

- **Link OpenCV / `cv2` (the model's current side-car):** violates the Rust-native
  runtime mandate (CODEX §4) and re-introduces a native dependency. The entire goal is
  to delete the cv2 side-car, not vendor it. Rejected.
- **Derive the box from the 5 landmarks only (skip the bbox head):** cheaper, no new
  decode, but less accurate than the trained bbox output and gives no basis for an
  honest count. Rejected.
- **Count raw anchors above threshold (skip NMS):** inflated counts break collage
  rejection — the one use case `face_count` exists for. Rejected; NMS is required.
- **Diverge single-gate vs batch output shapes:** rejected — one shared row schema so
  the model parses identically in both modes.

</topic>

<topic id="batch-gate-risks" wp="WP-007" summary="Risks and mitigations">

## Risks + mitigations

- **`exp()` blow-up / degenerate boxes** on bad anchors → clamp box to image bounds,
  reject NaN/inf and non-positive w/h before NMS.
- **Count-threshold semantics** (0.6 vs 0.9) → explicit config key, default 0.9,
  stamped into manifest + receipt; document that count depends on it.
- **NMS non-determinism** from unstable sort/tie order → sort by score desc with a
  stable secondary key (anchor index) so two runs are byte-identical (determinism
  mandate, mirrors existing identity determinism risk).
- **Square-grid assumption** → keep `DET_INPUT` square; assert `grid*grid==n` and skip
  the stride if not, rather than mis-indexing.
- **Large directory memory** → stream CSV rows; never hold all decoded images at once.
- **CSV correctness** → quote fields (paths may contain commas/quotes); write a header
  row; fixed column order.
- **Run-root resolution** → batch needs a `runs/<run_id>/` home; reuse the existing
  `run_root_for` worktree/copy-root logic, or root under the gated dir's `.facial/` if
  no project copy location is set (decision to confirm in the work packet).

</topic>

<topic id="batch-gate-validation" wp="WP-007" summary="How box/count + batch will be verified">

## Validation plan

1. Single clear face → `face_count==1`, box inside image, `face_frac` plausible (build
   + runtime proof).
2. Known collage / grid of N faces → `face_count≈N` (post-NMS), multi-face surfaced
   (collage-rejection proof).
3. No-face image → `face_count==0`, `verdict=no_face`, `align=resize_112` (fallback
   honesty proof).
4. Directory with one corrupt file → corrupt row `verdict=error`, others succeed,
   summary counts correct, batch does not abort (isolation proof).
5. Run the batch twice → byte-identical CSV (determinism proof).
6. Spot-check box coords vs `cv2.FaceDetectorYN` on 2-3 images (cross-impl parity,
   optional but strongest box-correctness evidence).

</topic>

<topic id="framing-calibration" wp="WP-011" summary="face_frac -> shot-type thresholds, calibrated on leeseo">

## Framing classification (STUB-A / WP-011)

Emit a `framing = close-up | three-quarter | full-body` label from `face_frac`
(face area / image area, already produced by WP-007). Field model's #3, "trivial
once face box exists."

**Field basis:** shot-scale classification keys off the largest face/person box area
ratio (arXiv 2008.03548 unified shot-type framework; multiple imaging patents). Those
cite *person-box* ratios (close-up >=35%, medium 10-35%, long <10%). Our signal is
*face*-area, which is much smaller, so thresholds must be calibrated to face area, not
copied from person-box numbers.

**Calibration (the operator's own i5 buckets, 10 imgs each, gated with this build):**

```
closeup       face_frac min=0.103 p50=0.147 max=0.249 mean=0.162
threequarter  face_frac min=0.036 p50=0.060 max=0.082 mean=0.060
fullbody      face_frac < threequarter by definition (face smaller); expect < ~0.03
```

Clean gap between three-quarter (max 0.082) and close-up (min 0.103).

**Selected defaults (configurable):**
- `framing_closeup_min = 0.09`   -> face_frac >= 0.09 => close-up
- `framing_threequarter_min = 0.03` -> 0.03 <= face_frac < 0.09 => three-quarter
- else => full-body
- `framing = none` when no face detected (face_frac null).

Exposed as config keys (`FACIAL_FRAMING_CLOSEUP_MIN`, `FACIAL_FRAMING_THREEQUARTER_MIN`)
and stamped into the gate manifest so the bucketing is auditable and tunable per subject.

Links: arxiv.org/pdf/2008.03548 (shot-type classification); calibration is local to the
leeseo buffer and should be re-checked per new subject/lens.

</topic>
