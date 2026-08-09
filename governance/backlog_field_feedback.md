---
file_id: backlog_field_feedback
file_kind: backlog
updated_at: 2026-06-11
---

<topic id="scope" summary="Where these items came from">

## Scope

Backlog captured from a field model running facial.exe at scale for the leeseo LoRA
work (a 5,267-image identity gate + a 70-render eval) on 2026-06-10. Two note sets:
LoRA-curation features (Tier 1-3) and general product improvements (scale/perf/trust/
integration). Items are work-packet *stubs* per [GLOBAL-BUILD-079] — promote to full
work packets (with research basis + refinement) before building. Priority is the field
model's, adjusted for what already shipped.

</topic>

<topic id="already-shipped" summary="Field asks already delivered by WP-007">

## Already shipped (reconciliation — the notes predate WP-007)

The field model's top-three ("ship these first") are already in the binary:

- **Batch/folder identity gate** (its #1) -> `identity_gate_dir` emits a per-image CSV,
  engine loaded once. Replaces its command-queue batching harness. **DONE (WP-007).**
- **Real face box + size + count in gate output** (its #2) -> `face_box`, `face_frac`,
  `face_count` (real YuNet, post-NMS), `face_score`. Replaces its `leeseo_i5_measure.py`
  and makes collages rejectable via `face_count > 1`. **DONE (WP-007).**
- **Scale/framing classification** (its #3) -> `face_frac` is now emitted; only the
  derived `close-up | three-quarter | full-body` *label* is missing. **PARTIAL (WP-007)**;
  see STUB-A.

Action for the field model: re-pull the build; CSV columns and `identity_gate_dir`
already cover most of its external scripts.

</topic>

<topic id="tier1-curation" summary="Remaining Tier-1 LoRA curation stubs">

## Tier 1 — curation gaps (high ROI)

- **STUB-A — scale/framing bucket label.** Emit `framing = close-up|three-quarter|
  full-body` from `face_frac` thresholds in the gate row + CSV. Tiny add on top of
  WP-007. Replaces hand-bucketing. (Field #3.)
- **STUB-B — embedding-based near-duplicate dedup** (`deepface:identity_dedup`). Dedup a
  folder by ArcFace cosine (not perceptual hash) so "same look, different crop/filter"
  collapses while genuine angle/expression variety survives. imagededup (pHash) let burst
  frames through. Reuses the embedder + the batch-gate plumbing. (Field #4.)
- **STUB-C — one-shot LoRA-dataset builder** (`loraprep:build` or a `sort_run` mode).
  Pipeline as one command: identity-gate -> face-count/quality cull -> dedup -> scale-
  bucket -> copy into a per-scale folder tree + manifest. Copy-mode, non-destructive.
  `build-lora-dataset --ref <anchors> --sim-min 0.57 --per-scale ... --out <dir>`.
  Composes A+B with the existing gate/sort. The field model calls this the "killer
  feature." (Field #5.)

</topic>

<topic id="tier2-quality" summary="Tier-2 per-image quality/pose stubs">

## Tier 2 — catch problems we found by eye

- **STUB-D — perspective / wide-angle "selfie warp" flag.** From the 5 landmarks + face
  box, estimate apparent focal length / perspective and flag close wide-angle (often
  from-above) shots that distort geometry. Field model flags this as genuinely novel (no
  off-the-shelf curation tool does it) and directly targeting the "type not person"
  failure. One of its top-3-to-ship. (Field #6.)
- **STUB-E — face-region quality (not whole-image).** ediffiqa/ofiq score the whole
  frame; a sharp-background/soft-face shot passes but is bad data. Add a face-crop
  sharpness/quality score (laplacian-variance on the crop). (Field #7.)
- **STUB-F — occlusion / landmark-visibility check.** Flag hands/hair/mask/mic/sunglasses
  over the face via landmark confidence + face-region coverage. (Field #8.)
- **STUB-G — pose yaw/pitch/roll estimation.** From the 5 landmarks: drop extreme
  profiles where identity smears; report angle-diversity to balance a set. Also feeds the
  sibling OpenRepose yaw work. (Field #9.)

</topic>

<topic id="tier3-dataset" summary="Tier-3 dataset-level intelligence stubs">

## Tier 3 — dataset-level judgment

- **STUB-H — identity-consistency report for a dataset.** Answer "is this ONE consistent
  person?": intra-set cosine distribution, outliers, sub-cluster detection (eras/makeup/
  weight). Quantifies the "her type vs her person" question; would have flagged the >=0.50
  set's spread upfront. (Field #10.)
- **STUB-I — threshold auto-calibration from the reference set.** Compute anchor pairwise
  self-consistency + a negative set's distribution and recommend the gate threshold. The
  ~0.564 anchor-pairwise number should come from the app, not eyeballing. (Field #11.)
- **STUB-J — batch render-eval report.** Score a folder of generated renders vs anchors,
  grouped by config, emit mean/min/max. Closes the train->eval loop in one tool. (Field #12.)

</topic>

<topic id="platform-scale" summary="General product/platform improvements">

## General — scale, trust, machine-driveability

- **STUB-K — warm-model daemon** (`facial serve`, file-watched, no socket/window). Avoids
  reloading the 175 MB ArcFace per one-shot call (~5s -> ~1.3s/img). The field model's #1
  overall pain.
- **STUB-L — GPU + multi-core batch inference.** tract CPU made the 5,267-image gate take
  ~1h; a GPU path (ort/CUDA or candle) + multi-core would make it minutes. NOTE: must be
  weighed against the Rust-native / no-native-runtime mandate (CODEX section 4) — research
  candle-GPU vs ort before committing.
- **STUB-M — consolidated results store** (append-only JSONL or SQLite per run) instead of
  one receipt JSON per item (5,277 files for one run). "mean sim by config" becomes a
  query. WP-007's batch manifest already moves this direction for the gate; generalize it.
- **STUB-N — batch exit codes + summary.** `run-queue` returns exit 1 if any item errored;
  a fully-successful 5,277-image gate reported "failed" over a few no-face images. Reserve
  nonzero for fatal/setup failure; completed batch with per-item errors exits 0 + emits an
  n-ok/n-no-face/n-error summary. (WP-007's batch already returns a summary; align run-queue.)
- **STUB-O — progress/heartbeat events** to events.jsonl for long batches (processed/total,
  eta) instead of polling the receipts dir.
- **STUB-P — dry-run preview** for copy/sort: report counts per bucket before moving.
- **STUB-Q — `facial models pull <name>`**: auto-download + sha-verify ONNX weights; make
  model_registry.json actuated, not just descriptive.
- **STUB-R — run provenance manifest + schema_version.** Per-run manifest (app version, all
  model shas, params, thresholds); add `schema_version` to JSON outputs.
- **STUB-S — MCP stdio server** over the existing command/receipt verbs, so any LLM drives
  it natively while keeping the no-socket/no-window stance. Field model: "the single
  feature that most changes who can use it."
- **STUB-T — tabular export** (`--format csv|jsonl|parquet`) for consolidated results.
- **STUB-U — golden-sample regression suite** for the scorers (fixed images -> expected
  sims/verdicts) in CI; cheap because the scorers are deterministic.
- **STUB-V — config presets per project** (named threshold/feature/ref-dir bundles:
  "gate this folder as Leeseo at 0.57").
- **STUB-W — trust labeling / real-model backbone.** The field model's strongest critique:
  proxy plugins (facet/deepface/imagededup skin-color + pHash heuristics) emit
  authoritative-looking fields (`quality_band`, proxy `face_count`) a consumer can't
  distinguish from real. Either make the real YuNet+ArcFace the backbone of every face
  feature, or stamp `"source": "proxy" | "real"` in every JSON. Promote identity out of
  "experimental." HIGH trust value.

</topic>

<topic id="non-goals" summary="Explicitly out of scope per the field model">

## Deliberate non-goals (scope discipline, from the field model)

- Do NOT reinvent kohya/training — stop at the training-ready dataset + manifest the
  trainer consumes.
- Keep captioning pluggable, not core — a thin optional caption plugin that shells to a
  tagger + applies an identity-blacklist is fine; the binary must not own a tagger.
- Do NOT add a GUI dependency — everything above fits the headless command/receipt model.

</topic>

<topic id="field-session-2" summary="Second field session (2026-06-11): 7 asks, reconciled + promoted" wp="WP-016">

## Second field session (2026-06-11) — reconciliation

A second field model (LoRA training session: anchors, 450-image hand pass, kohya
training) proposed 7 features. NOTE: it ran an OLD build — `identity_gate_dir` verdict
taxonomy (`no_face` is its own verdict, never a pass), `face_count`, and `source:
real|proxy` already answer parts of asks 1/2. Reconciliation against this backlog:

- **Ask 1 (identity-eval harness)** -> core = STUB-J (render-eval report) + STUB-I
  (threshold calibration) + NEW montage artifact with stable tile IDs + explicit failure
  taxonomy. "Fixed-seed renders across config matrix" is ComfyUI-side; facial consumes
  images. "Never score through a detailer" is a pipeline rule; facial's half is `source`
  labeling (shipped, WP-012). **Promoted -> WP-017.**
- **Ask 2 (review queue: stable IDs, served montages, persisted accept/reject+reason,
  claimable shards, kohya export)** -> NEW, the strongest ask; composes STUB-C (export
  tail), STUB-M (results store), STUB-T. Metadata flags split: sharpness (STUB-E) + yaw
  (STUB-G) + hair-color heuristic are wave-1; eyes-open EAR + occlusion are NOT honest
  with 5-point YuNet landmarks -> landmark-model research spike first (STUB-F refined).
  **Promoted -> WP-016 (queue+lineage) and WP-019 (metadata waves).**
- **Ask 3 (anchor comparison first-class)** -> NEW; rides Compare machinery + identity
  reference dir. **Promoted -> WP-017.**
- **Ask 4 (near-dup grouping in review)** -> presentation of STUB-B (embedding dedup)
  inside the WP-016 queue. **Promoted -> WP-018.**
- **Ask 5 (dataset lineage/provenance)** -> extends STUB-R/M into a cross-stage funnel
  ledger; designed jointly with the queue (decisions are lineage events).
  **Promoted -> WP-016.**
- **Ask 6 (GPU/lane registry)** -> **DECLINED for facial** (wrong home): facial owns no
  GPU processes (tract CPU); lane/PID/VRAM ownership is comfyui-workbench orchestration
  governance. Hand the notes to that repo.
- **Ask 7 (training launch endpoint)** -> **DECLINED**: existing non-goal ("do NOT
  reinvent kohya/training"); host launch quirks belong to the training host runbook.
  facial's contribution is the kohya-ready export (WP-016 tail).

Spec contracts for the promoted work: `specs/app-spec.md` section 14.

</topic>

<topic id="suggested-order" summary="Suggested promotion order">

## Suggested promotion order (operator to confirm)

Field model's "ship first" was #1/#2/#6 — #1 and #2 are done (WP-007).

Status:
- **STUB-A** -> **WP-011 DONE** (framing label; 100% correct on leeseo buckets).
- **STUB-W** -> **WP-012 DONE** (source=real on gate + real embeddings; source=proxy on
  ALL proxy plugins - deepface/facet/ofiq/ediffiqa/imagededup - verified at runtime;
  identity de-labeled from "experimental").
- **2026-06-11 promotions (operator-confirmed):** STUB-C/M + ask-2/5 -> **WP-016**;
  STUB-J/I + ask-1/3 -> **WP-017**; STUB-B + ask-4 -> **WP-018**; STUB-E/G + hair-color
  + landmark spike (STUB-F refined) -> **WP-019**; bundled-default YuNet detector
  (operator ask, composes STUB-Q research) -> **WP-020** (future work).

Still unpromoted: **STUB-D** (selfie-warp), **STUB-K** (warm daemon), **STUB-L** (GPU
inference), **STUB-N/O/P/R/S/U/V** (scale/trust/integration). Awaiting operator pick.

</topic>
