---
file_id: research_landmark_models
file_kind: research
updated_at: 2026-06-11
---

<topic id="scope" wp="WP-019" summary="Why this research exists and what it must decide">

## Scope

WP-019 wave-2 research spike. The app (`product/`) runs a pure-Rust inference stack
(`tract-onnx = "0.21"` pinned in `product/Cargo.toml`) with YuNet 2023mar (5-point
landmarks) + ArcFace, models provisioned at runtime by the operator, never bundled
or auto-downloaded. Wave 2 wants two new per-image curation signals: **eyes-open
(EAR)** and **occlusion**. 5-point YuNet landmarks cannot support either honestly,
so this spike evaluates >=68-point (or dense) facial-landmark ONNX models that can
run under tract-onnx 0.21, and ends with a BUILD / NO-BUILD recommendation.
Deliverable is this report only — no runtime integration in WP-019 (per the packet
`non_goals`). Operator context is commercial production, so model-weight licenses
are a first-class selection criterion, not a footnote.

</topic>

<topic id="candidates" wp="WP-019" summary="Comparison of 68+/dense landmark ONNX candidates">

## Candidate models

| Model | Points | Input | ONNX size | License (code / weights) | ONNX artifact source | tract-0.21 op risk |
|---|---|---|---|---|---|---|
| PIPNet R18 WFLW (yakhyo/pipnet-onnx) | 98 | 256x256 | 47.05 MB | MIT (port) / MIT (upstream jhb86253817/PIPNet code); weights trained on research datasets (WFLW) | github.com/yakhyo/pipnet-onnx/releases/download/weights/pipnet_r18_wflw_98.onnx | Low (plain ResNet-18 convs, fixed shapes; decode done app-side) |
| PIPNet R18 300W+CelebA (same repo) | 68 | 256x256 | 45.73 MB | same as above | .../pipnet_r18_300w_celeba_68.onnx | Low (same graph family) |
| PFLD 98 (yakhyo/face-landmark-detection; polarisZhao/PFLD-pytorch) | 98 | 112x112 | ~3 MB class (0.73M-param PFLD variants) | **No license** (GitHub license field null on both repos) | None released — export-it-yourself (`onnx_export.py` / `pytorch2onnx.py`) | Low (MobileNetV2-style convs) |
| PFLD/MobileFaceNet 68 (cunjian/pytorch_face_landmark) | 68 | 112x112 | ~3 MB class (0.73M / 1.01M params) | **No license** (license field null); ONNX via Google Drive links | Google Drive links in README (not stable artifacts) | Low |
| PFLD 68 via FaceONNX.Models (`landmarks_68_pfld.onnx`) | 68 | 112x112 | inside split-RAR packs (~233 MB archives) | MIT (FaceONNX repos), but weight provenance undocumented | github.com/FaceONNX/FaceONNX.Models (RAR parts; no single-file URL) | Low |
| InsightFace 2d106det | 106 | 192x192 (NCHW, 0-255) | ~5 MB (MobileNet-0.5) | Code MIT / **pretrained models non-commercial research only** | insightface buffalo_l pack; HF mirrors (menglaoda/_insightface, KlingTeam/LivePortrait) | Low (MobileNet convs; app already runs insightface-family CNNs) |
| MediaPipe FaceMesh 468 (PINTO zoo 032_FaceMesh ONNX) | 468 (x,y,z) | 192x192 | ~2-3 MB | Apache-2.0 (upstream Google model card AND PINTO 032 folder LICENSE) | github.com/PINTO0309/PINTO_model_zoo/tree/main/032_FaceMesh (download script; ONNX + post-process variants) | Moderate (TFLite->ONNX conversion artifacts; 468 base is custom-op-free, **478 attention variant uses custom/sampling ops — avoid**) |
| 2DFAN4 / FAN heatmap (1adrianb/face-alignment; facefusion `2dfan4.onnx`) | 68 (heatmap) | 256x256 | Large (~90 MB class, exact size unverified) | BSD-3-Clause (upstream face-alignment) | facefusion assets release (used as facefusion default landmarker) | Low-moderate ops, but size/latency heavy |
| 3DDFA_V2 (mb1_120x120) | 68 from 62 3DMM params | 120x120 | ~13 MB (3.27M params) | Code MIT, but 68-pt reconstruction needs BFM (Basel Face Model) basis data — research-restricted | github.com/cleardusk/3DDFA_V2 (`--onnx` path) | Low ops; **post-process needs BFM basis files** (extra licensed data + offline prep) |
| SLPT (Jiahao-UTS/SLPT-master) | 98 | n/s | n/s | **GPL-2.0** | No official ONNX export | **High** (DETR-style transformer + dynamic local-patch resampling => GridSample-class ops, absent in tract 0.21) |
| atksh/onnx-facial-lmk-detector (end-to-end) | 106 | dynamic | n/s | repo MIT but embeds insightface-derived models (non-commercial weights) | github.com/atksh/onnx-facial-lmk-detector | High (end-to-end graph: NMS, dynamic shapes, control flow) |

Eliminations on first pass: SLPT (GPL-2.0 + no ONNX + resampling ops), 3DDFA_V2
(BFM data restriction + post-process complexity), atksh end-to-end (dynamic-shape
graph + insightface weight license), 2d106det (non-commercial weights — see also
`ear-requirements`: its eye indices are not authoritatively documented), all
unlicensed PFLD repos (license field null = no redistribution/use grant at all;
FaceONNX's MIT claim has undocumented weight provenance).

</topic>

<topic id="ear-requirements" wp="WP-019" summary="EAR point requirements per landmark scheme">

## Eyes-open: EAR point requirements

Classic EAR (Soukupova & Cech 2016) needs **6 points per eye**: 2 corners + 2 upper-lid
+ 2 lower-lid. `EAR = (||p2-p6|| + ||p3-p5||) / (2 * ||p1-p4||)`; eyes-closed threshold
commonly ~0.2 (per-subject calibration recommended).

- **68-pt iBUG/300-W scheme** (0-based): right eye = indices **36-41**, left eye =
  **42-47**. p1..p6 map directly (e.g. right eye p1=36 outer corner, p4=39 inner
  corner, p2/p3=37/38 upper lid, p5/p6=40/41 lower lid).
- **98-pt WFLW scheme**: right eye = **60-67** (8 pts), left eye = **68-75** (8 pts),
  plus pupils **96** (right) and **97** (left). Published simplified EAR for WFLW:
  `EAR_R = ||P66-P62|| / ||P64-P60||`, `EAR_L = ||P74-P70|| / ||P72-P68||`
  (corners 60/64 and 68/72; mid-lid verticals 62/66 and 70/74). The 8-pt ring also
  supports the classic two-vertical-pair form, and the pupil points give a free
  cross-check (pupil-between-lids visibility). This scheme is authoritatively
  documented by the WFLW dataset definition.
- **106-pt InsightFace scheme**: eyelid points exist (roughly 8-10 per eye) but the
  index layout is **not authoritatively documented** by deepinsight — only
  community-annotated diagrams circulate (multiple insightface issues asking for the
  map remain unanswered). Any EAR use would require empirically verifying eye indices
  against rendered fixtures first. This is a real adoption cost unique to 2d106det.
- **FaceMesh 468 scheme**: field-standard 6-pt EAR subsets: eye A = **33, 160, 158,
  133, 153, 144**, eye B = **362, 385, 387, 263, 373, 380** (left/right naming varies
  by source between subject-side and image-side conventions; verify side empirically
  once, then fix it in code).

Conclusion: EAR is honestly achievable with any 68/98/468 candidate; WFLW-98 and
iBUG-68 have the cleanest documented index contracts.

</topic>

<topic id="occlusion-signals" wp="WP-019" summary="What occlusion signal each model gives, and field practice">

## Occlusion signals

What each candidate actually emits:

- **Direct-regression models (PFLD, 2d106det)**: coordinates only, **no per-landmark
  confidence**. Under occlusion they hallucinate plausible positions — silent failure,
  no native occlusion signal.
- **FaceMesh 468**: full mesh always regressed + **one scalar face-presence score**.
  No per-landmark confidence; the scalar is a crop-quality signal, not an occlusion
  detector.
- **Heatmap/score-map models (2DFAN4, PIPNet)**: a usable proxy exists. 2DFAN4 emits
  68 heatmaps; max activation per landmark is used in the field as per-landmark
  confidence (facefusion's `face-landmarker-score` threshold, default 0.5). PIPNet
  emits a per-landmark classification score map (`cls_map`, one low-res grid per
  landmark) + offset/neighbor maps; the per-landmark `cls_map` max is the analogous
  confidence proxy (weaker than a trained occlusion head, but real signal).
- **SLPT**: coordinates only.

Field practice for *honest* occlusion detection does **not** rely on regression
landmarks: OFIQ (reference implementation of ISO/IEC 29794-5 face image quality)
computes `EyesOpen` from 98-pt landmarks (ADNet, WFLW scheme) but computes occlusion
measures (`EyesVisible`, face-occlusion prevention) from **face-parsing segmentation
+ a dedicated occlusion-segmentation CNN** (~400 MB model set). Patents and
commercial stacks that do use landmark confidence use heatmap-style confidences and
threshold them. Implication for this app: eyes-open via EAR = `source: real`;
occlusion via landmark confidence = `source: proxy` (labeled, triage-only, never a
gate — same discipline as WP-019 wave-1 hair_color_flag); honest occlusion would be
a separate segmentation model and is out of wave-2 scope.

</topic>

<topic id="tract-compatibility" wp="WP-019" summary="Verified tract-onnx 0.21 operator constraints per candidate">

## tract-onnx 0.21 compatibility

Verified facts (tract source + docs):

- The app pins `tract-onnx = "0.21"`. In the **0.21.13 tag** of
  `onnx/src/ops/mod.rs`, **GridSample is NOT registered**; it appears only on
  current `main` (a post-0.21 addition). `NonMaxSuppression` and `Resize` ARE
  registered in 0.21.13; `PRelu` is in tract's supported-op list.
- tract documents testing against ONNX opset 9 through 18 and passes ~85% of ONNX
  backend tests; tensor-sequence/optional-tensor ops are out of scope by design.

Per-candidate risk:

- **PIPNet ONNX (256x256 fixed)**: ResNet-18 trunk + plain conv heads; five output
  tensors (`cls_map`, `offset_x`, `offset_y`, `nb_x`, `nb_y`); the pixel-in-pixel
  decode (per-landmark argmax + offset + 10-neighbor averaging) lives **outside the
  graph** in app code — exactly the YuNet pattern already proven in
  `product/src/identity.rs` (raw maps in, Rust decode out). Lowest risk. Export
  opset undocumented — must be checked (torch exports are typically 11-17, all
  inside tract's tested range).
- **PFLD**: MobileNetV2-style ops, low risk — but no licensed ready artifact exists.
- **2d106det**: MobileNet-0.5, fixed 192x192 NCHW, raw 0-255 input; low op risk
  (the app already runs insightface-family CNNs under tract) — license kills it.
- **FaceMesh 468 (PINTO ONNX)**: moderate. Converted TFLite->ONNX (transpose/pad
  conversion artifacts; some variants carry baked post-processing). The 468 base
  graph is custom-op-free; the **478 "with-attention" variant uses custom sampling
  ops in the original graph (GridSample-class when ported) — incompatible with
  tract 0.21, avoid**. Also operationally: FaceMesh expects a MediaPipe-style
  rotated/margined face crop to perform to reputation, which the current YuNet crop
  path does not produce.
- **2DFAN4**: standard hourglass convs, but ~90 MB-class and the heaviest CPU cost.
- **SLPT / atksh end-to-end**: dynamic-shape / control-flow / resampling graphs —
  high risk under 0.21; rejected.

</topic>

<topic id="recommendation" wp="WP-019" summary="Selected model and BUILD/NO-BUILD verdict">

## Recommendation

**Selected: PIPNet ResNet-18 WFLW 98-pt ONNX (`pipnet_r18_wflw_98.onnx`, 47 MB) from
yakhyo/pipnet-onnx.** Justification:

1. **License**: MIT on both the ONNX port and the upstream PIPNet code — the only
   candidate with a permissive license AND a ready, stable, single-file ONNX
   artifact URL. (Weights are trained on the WFLW research dataset; the app never
   bundles or redistributes models — operator provisions at runtime, consistent
   with the existing SCRFD/ArcFace/YuNet posture.)
2. **tract risk**: lowest of all candidates — fixed-shape ResNet-18 convolutions,
   graph-external decode in Rust, same integration pattern as the existing YuNet
   raw-output decode.
3. **Signals**: 98-pt WFLW gives authoritative documented eye indices (60-67 /
   68-75 + pupils 96/97) for EAR, and `cls_map` gives a per-landmark confidence
   proxy for occlusion — the runner-ups give one or the other, not both
   (FaceMesh: no per-landmark confidence; PFLD: no licensed artifact; 2d106det:
   non-commercial + undocumented indices).
4. **Accuracy reputation**: PIPNet (IJCV 2021) is a well-cited, field-used detector
   (torchlm ships a reimplementation); ResNet-18 WFLW NME is competitive for
   curation-grade use. 47 MB is acceptable next to the already-provisioned 166 MB
   ArcFace.

**Honesty split**: eyes-open (EAR) is **fully achievable** (`source: real`,
geometry from real landmarks, per-eye EAR + threshold with calibration note).
Occlusion is achievable **only as a labeled proxy** (`source: proxy`,
per-landmark `cls_map` confidence thresholds, triage-only, never a gate); honest
occlusion detection requires a separate parsing/segmentation model (OFIQ-style)
and should be a future packet if field feedback demands it. Runner-up if PIPNet
fails validation: PINTO FaceMesh 468 (Apache-2.0, ~2-3 MB) — eyes-open only,
zero occlusion signal, plus crop-convention work.

**Verdict: BUILD** (wave 2, new work packet after this spike), gated on this
validation order:
1. Load `pipnet_r18_wflw_98.onnx` under tract-onnx 0.21 (`onnx().model_for_path` +
   `into_optimized()`); confirm opset <= 18, all five outputs typed and shaped as
   expected. This is the single biggest unknown — do it before any other work.
2. Port the decode (per-landmark argmax + offset + neighbor averaging, 1.2x bbox
   crop, ImageNet normalization) and parity-check landmarks against the reference
   Python implementation on a handful of fixture crops (tolerance: a few pixels).
3. EAR sanity fixtures: open-eye vs closed-eye crops produce separated EAR
   distributions; pick threshold (~0.2 start) and stamp method + threshold into the
   manifest row like wave-1 yaw buckets.
4. Occlusion proxy calibration: `cls_map` confidence distributions on clean vs
   occluded fixtures; only expose the flag if the separation is real, and always
   as `source: proxy`. If separation is weak, ship eyes-open alone — the BUILD
   verdict does not depend on the occlusion proxy.

</topic>

<topic id="sources" wp="WP-019" summary="Every URL consulted">

## Sources

Candidate repos and artifacts:

- https://github.com/yakhyo/pipnet-onnx
- https://api.github.com/repos/yakhyo/pipnet-onnx/releases (asset names + byte sizes)
- https://github.com/yakhyo/pipnet-onnx/releases/download/weights/pipnet_r18_wflw_98.onnx
- https://github.com/yakhyo/pipnet-onnx/releases/download/weights/pipnet_r18_300w_celeba_68.onnx
- https://github.com/jhb86253817/PIPNet
- https://github.com/DefTruth/torchlm (PIPNet reimplementation with ONNX export)
- https://github.com/yakhyo/face-landmark-detection (+ raw README; api.github.com license field = null)
- https://github.com/polarisZhao/PFLD-pytorch (api.github.com license field = null)
- https://github.com/cunjian/pytorch_face_landmark (+ raw README; api.github.com license field = null)
- https://github.com/FaceONNX/FaceONNX
- https://github.com/FaceONNX/FaceONNX.Models (+ api.github.com contents listing)
- https://github.com/deepinsight/insightface/blob/master/alignment/coordinate_reg/README.md
- https://github.com/deepinsight/insightface/issues/1369
- https://github.com/deepinsight/insightface/issues/2588
- https://github.com/deepinsight/insightface/issues/2532
- https://github.com/deepinsight/insightface/issues/2469 (licensing discussion)
- https://huggingface.co/InstantX/InstantID/discussions/2 (insightface non-commercial weight license)
- https://onnx-interp.hexdocs.pm/facealign.html (2d106det input/output contract)
- https://huggingface.co/menglaoda/_insightface/blob/main/2d106det.onnx (mirror)
- https://huggingface.co/KlingTeam/LivePortrait/blob/main/insightface/models/buffalo_l/2d106det.onnx (mirror)
- https://github.com/PINTO0309/PINTO_model_zoo/tree/main/032_FaceMesh
- https://raw.githubusercontent.com/PINTO0309/PINTO_model_zoo/main/032_FaceMesh/LICENSE (Apache-2.0)
- https://github.com/PINTO0309/PINTO_model_zoo/discussions/404 (FaceMesh-with-Attention conversion)
- https://github.com/google-ai-edge/mediapipe/blob/master/docs/solutions/face_mesh.md
- https://github.com/Jiahao-UTS/SLPT-master (GPL-2.0)
- https://github.com/cleardusk/3DDFA_V2 (MIT; ONNX path; BFM dependency)
- https://github.com/1adrianb/face-alignment (BSD-3-Clause FAN)
- https://github.com/1adrianb/face-alignment/issues/344 (ONNX version request)
- https://github.com/facefusion/facefusion/blob/master/facefusion/face_landmarker.py (2dfan4 heatmap decode + score)
- https://docs.facefusion.io/usage/cli-arguments/face-landmarker (landmarker score threshold)
- https://github.com/atksh/onnx-facial-lmk-detector

tract compatibility:

- https://github.com/sonos/tract (README: opset 9-18, ~85% backend tests)
- https://github.com/sonos/tract/blob/main/doc/intro.md (supported-op list incl. PRelu)
- https://raw.githubusercontent.com/sonos/tract/0.21.13/onnx/src/ops/mod.rs (GridSample ABSENT in 0.21.13)
- https://raw.githubusercontent.com/sonos/tract/main/onnx/src/ops/mod.rs (GridSample registered on main only)

EAR and landmark schemes:

- https://www.emergentmind.com/topics/eye-aspect-ratio-ear (EAR definition + 0.2 threshold)
- https://www.ncbi.nlm.nih.gov/pmc/articles/PMC9044337/ (adjusted EAR, 68-pt blink detection)
- https://arxiv.org/pdf/2401.11284 (driver-readiness paper: WFLW 98-pt EAR formula, indices 60-75)
- https://github.com/Dehim1/98-facial-landmarks-with-Caffe-and-DNNDK/blob/master/docs/datasets.md (WFLW region index table)
- https://github.com/wywu/LAB/issues/17 (WFLW dataset definition)
- https://www.researchgate.net/figure/The-98-facial-landmarks-defined-in-the-WFLW-dataset-Blue-points-are-used-for-the_fig3_378128358
- https://www.researchgate.net/figure/The-106-point-landmark-make-up_fig2_335193878
- https://learnopencv.com/driver-drowsiness-detection-using-mediapipe-in-python/ (FaceMesh EAR indices)
- https://github.com/Pushtogithub23/Eye-Blink-Detection-using-MediaPipe-and-OpenCV (FaceMesh EAR code)
- https://www.researchgate.net/figure/MediaPipe-Facemesh-Left-Eye-Landmarks-for-calculating-Eye-Aspect-Ratio-EAR_fig1_368318088

Occlusion field practice:

- https://github.com/BSI-OFIQ/OFIQ-Project (ISO/IEC 29794-5 reference implementation)
- https://eab.org/files/events/2023-11-07-09_Face_Image_Quality/2023-11-09_23-Benjamin_Tams-secunet.pdf (OFIQ measures: ADNet 98-pt landmarks, parsing + occlusion segmentation)
- https://pages.nist.gov/ifpc/2025/presentations/05.pdf (OFIQ overview)
- https://publica-rest.fraunhofer.de/server/api/core/bitstreams/40d0b62b-7b0c-4117-907d-86a438b49530/content (occlusion detection for face image quality)
- https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11487968 (landmark-confidence occlusion patent)
- https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11934955 (landmark-confidence occlusion patent)
- https://www.researchgate.net/publication/361444481_FaceOcc_A_Diverse_High-quality_Face_Occlusion_Dataset_for_Human_Face_Extraction
- Local field reference: `_source_checks/python-ofiq/README.md` (EyesOpen / EyesVisible measures, ~400 MB model set)

</topic>
