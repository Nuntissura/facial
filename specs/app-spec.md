# facial specification

## 1) Purpose and intended use
`facial` is a local Rust desktop app that combines headshot-quality, identity, dedupe, and face-IQ behaviors from five source families:
- `facet`
- `python-ofiq`
- `deepface`
- `imagededup`
- `eDifFIQA`

The app is designed for high-volume headshot preselection workflows and model/operator testing workflows with deterministic, local, non-destructive defaults.

## 2) Runtime model and constraints
- Runtime language is Rust.
- Plugin execution uses `product/src/plugins/*.rs`; no Python runtime is used for primary feature execution.
- Plugin manifests in `product/plugins/**/metadata.json` are source-of-truth for discoverable features.
- App install root and runtime workspace root are separate.
- Runtime events must be emitted to `<workspace_root>/.facial/data/events.jsonl` unless `FACIAL_DATA_ROOT` overrides the data root.
- `set_copy_location` is required before running pipeline or sort actions.
- `set_workspace_root` selects the runtime root for `.facial/data`, `.facial/worktrees`, API queues, receipts, and debug events.
- Default ingest mode is `copy`; `in_place` is explicit and surfaced in state.
- No plugin, pipeline, or debug action may launch external windows.

## 3) Source-app behavior to copy

### 3.1 `facet` behavior contracts
`facet` provides lightweight quality and cleanup behaviors used as the first-pass culling layer.

- `quality_pass`
  - Output file: `runs/<run_id>/facet/quality_pass/quality_pass.json`
  - Payload: per-image scoring with:
    - `path`, `file_size`, `width`, `height`
    - `quality`, `technical_sharpness`, `eyes_sharpness`
    - `exposure`, `color_balance`, `dynamic_range`, `noise_estimate`
    - `quality_band` (`excellent|good|usable|weak|reject`)
    - `headshot_candidate` (quality>=68)
    - `source: facet_native`
- `composition_pass`
  - Output file: `runs/<run_id>/facet/composition_pass/composition_pass.json`
  - Payload:
    - `composition_score`, `center_bias`, `entropy`, `dynamic_range`, `noise`
- `faces_pass`
  - Output file: `runs/<run_id>/facet/faces_pass/faces_pass.json`
  - Payload:
    - `face_count`, `face_confidence`, `face_region`, `eye_open`, `region_score`
- `duplicate_pass`
  - Output file: `runs/<run_id>/facet/duplicate_pass/duplicate_pass.json`
  - Output summary:
    - `feature`, `method`, `count`, `policy`, `groups`, `total_members_in_duplicate_groups`, `coverage_percent`
    - per-group fields: `group_key`, `member_count`, `member_files`, `avg_similarity`, `min_similarity`, `max_similarity`, `avg_quality`, `total_file_size`, `representative`, `signature`, `matches_threshold`
  - Policy: `min_group_size` = 2, `min_avg_hash_similarity` = 98.0, similarity units = percent
- `burst_blink_pass`
  - Output file: `runs/<run_id>/facet/burst_blink_pass/burst_blink_pass.json`
  - Output payload:
    - `count`, `mean_embedding_similarity`, `policy`, `blocks`
    - per-block fields: `burst_key`, `count`, `blink_frames`, `blink_ratio`, `mean_embedding_similarity`, `recommended_keep`, `items`, `burst_contains_blink`
    - per-item fields: `path`, `face_count`, `capture_unix_ms`, `eye_open`, `quality`, `blink_like`, `headshot_ready`
  - Policy: `min_burst_size` = 2, `blink_closed_threshold` = 35.0
- `diagnostics_pass`
  - Output file: `runs/<run_id>/facet/diagnostics_pass/diagnostics_pass.json`
  - Payload: run root plus per-image `sha256`, `ahash`, `dhash`, `phash`, `capture_unix_ms`, `quality`

Use cases:
- Build an initial headshot shortlist and reject duplicates/near-duplicates.
- Stabilize burst sessions by selecting a deterministic keep candidate per burst group.
- Keep reproducible diagnostics for visual review.

### 3.2 `python-ofiq` behavior contracts
`python-ofiq` contributes deterministic face-quality scoring with explicit scalar and vector outputs.

- `setup_data`
  - Output file: `runs/<run_id>/python-ofiq/setup_data/setup_data.json`
  - Payload:
    - `status`, `engine`, `version`, `dimensions`, `dimension_count`, `score_0_100`
    - `thresholds`: `scalar_quality_headshot_min` (68.0), `vector_quality_gap_tolerance` (25.0), `quality_score_range`
- `scalar_quality`
  - Output file: `runs/<run_id>/python-ofiq/scalar_quality/scalar_quality.json`
  - Payload:
    - per-image `path`, `scalar_quality`, `quality_band`, `dimension_sum`, `dimension_count`
    - `face_count`, `face_confidence`, `face_eye_open`, `face_region`, `source`
  - Contract: values are deterministic per input image.
- `vector_quality`
  - Output file: `runs/<run_id>/python-ofiq/vector_quality/vector_quality.json`
  - Payload:
    - `schema`: versioned dimension list (`version: 0.2-native`, `dimension_count`, `dimensions`)
    - per-image `scalar_quality`, `quality_band`, `quality_vector`, `dimensions`
    - `quality_gap_vs_dimension_mean`, `quality_band_summary`, `metadata`, `vector_cells`

Use cases:
- Headshot rank filtering by deterministic scalar thresholds.
- Audit fine-grained quality dimensions before final pruning.
- Derive calibration from shared dimension schema.

### 3.3 `deepface` behavior contracts
`deepface` contributes identity-style workflows and face-region/embedding proxies.

- `detect`
  - Output file: `runs/<run_id>/deepface/detect/detect.json`
  - Payload:
    - `feature`, `count`, `policy` and per-row fields:
    - `path`, `skin_region_proxy`, `face_confidence`, `detection_score`, `passed`,
      `quality_band`, `face_quality`, `regions`
  - Policy: face considered valid when `detection_score >= 25.0`
- `analyze`
  - Output file: `runs/<run_id>/deepface/analyze/analyze.json`
  - Payload:
    - `feature`, `count`, `policy` and per-row fields:
    - `path`, `mean_luma_proxy`, `luma_std_proxy`, `contrast_proxy`,
      `exposure_proxy`, `noise_proxy`, `skin_ratio_proxy`, `eye_open_proxy`,
      `face_confidence`, `skin_region_proxy`, `face_quality`, `quality_band`, `region`,
      `vector_hint`
  - Policy: no age/gender/emotion prediction; measured image proxies only.
- `represent`
  - Output file: `runs/<run_id>/deepface/represent/represent.json`
  - Payload:
    - `feature`, `count`, `items`, `notes`
    - per-row: `id` (sha256), `path`, `embedding_dim`, `embedding_sum`, `embedding_norm`,
      `embedding_unit` (`head`, `max_component`, `min_component`)
    - identity-engine mode: with provisioned embedder, output switches to ArcFace ONNX path
      and includes `engine`, `model_sha256`.
- `register`
  - Output file: `runs/<run_id>/deepface/register/register.json`
  - Payload:
    - `index` list items with `id`, `path`, `quality_score`, `face_count`, `face_confidence`
    - `index_size`, `index_quality.avg_quality`
- `find`
  - Output file: `runs/<run_id>/deepface/find/find.json`
  - Payload:
    - top-level: `feature`, `count`, `rows`, `top_k` (5), `accepted_queries`, `threshold`
    - per-row:
      - `query`, `query_quality`, `query_face_count`, `candidates`, `candidates_found`,
        `best_similarity`, `top_gap`, `threshold`, `decision`
- `verify`
  - Output file: `runs/<run_id>/deepface/verify/verify.json`
  - Payload:
    - `feature`, `count`, `pairs`, `threshold` (0.86), `soft_threshold` (0.76), `verified_pairs`
    - per-pair fields: `a`, `b`, `similarity`, `similarity_percent`, `distance`,
      `verified`, `decision`, `decision_confidence`, `quality_gap`, `quality_a`,
      `quality_b`, `face_count_delta`, `explain`

Use cases:
- Build quick face-index checkpoints for a model campaign.
- Run nearest-neighbor checks and pairwise verification for identity consistency.
- Keep all identity decisions transparent via structured decision fields.

### 3.4 `imagededup` behavior contracts
`imagededup` contributes dedupe grouping and candidate removal.

- `hash_duplicates`
  - Output file: `runs/<run_id>/imagededup/hash_duplicates/hash_duplicates.json`
  - Payload:
    - `count`, `duplicates_found`, `total_candidate_pairs`, `coverage_percent`, `groups`
    - per-group fields: `group_key`, `type`, `paths`, `count`, `type_size_total`, `best_keep`, `method`
- `cnn_duplicates`
  - Output file: `runs/<run_id>/imagededup/cnn_duplicates/cnn_duplicates.json`
  - Payload:
    - `count`, `pairs`, `threshold` (75.0), `pairs_selected`, `pairs_considered`, `method`
    - per-pair fields: `a`, `b`, `similarity`, `a_quality`, `b_quality`, `method`, `units`
- `remove_candidates`
  - Output file: `runs/<run_id>/imagededup/remove_candidates/remove_candidates.json`
  - Payload:
    - `count`, `remove_list`, `pairs_threshold` (78.0), `policy`, `images_scanned`
    - per-action fields: `path`, `action`, `keep`, `component_id`, `similarity_to_keep`, `decision` (`keep_score`, `remove_score`, `score_delta`, `reason`)

Use cases:
- Generate conservative removal candidates from hash/CNN dedupe evidence.
- Preserve traceability by storing keeper/removal rationale.
- Review component-level duplicate connectivity before deleting anything.

### 3.5 `eDifFIQA` behavior contracts
`eDifFIQA` contributes multiple deterministic quality profiles and a batch consensus mode.

- `model_t`, `model_m`, `model_s`, `model_l`
  - Output files:
    - `runs/<run_id>/ediffiqa/<model_id>/<model_id>.json`
  - Payload:
    - `feature`, `count`, `model`, `model_profile`, `items`
    - per-image fields: `path`, `model`, `score`, `score_components`, `dims`, `pass_quality`, `face_count`, `face_confidence`, `eye_open`, `quality_delta`, `best_model_for_image`, `status`
- `batch_inference`
  - Output file: `runs/<run_id>/ediffiqa/batch_inference/batch_inference.json`
  - Payload:
    - `matrix` rows with per-model scores and winning model info
    - `model_summary`, `winner_counts`, `winner_score_stats`

Use cases:
- Compare profile sensitivity under one pass across five quality variants.
- Export comparative scoring for manual calibration by campaign type.

## 4) Unified feature registry and overlap handling
- Runtime features are exposed as `plugin_id:feature_id` keys from manifests.
- No-context model or operator can consume outputs from:
  - `quality` features (`facet`, `python-ofiq`, `ediffiqa`)
  - `identity` features (`deepface`)
  - `dedupe` features (`facet`, `imagededup`)
- Overlap is handled by:
  - explicit source attribution in each payload,
  - deterministic feature ordering,
  - operator-selectable feature selection.
- Plugin run status is surfaced immediately in UI and debug log, then in `results.json`.

## 5) Navigation and state surfaces
### 5.1 UI surfaces
The GUI is a single window organized as a header strip, eight tabs, and a status bar
(layout per WP-009/013/014, identity per WP-015). 

- Header strip (one row, every tab)
  - logomark + `facial` wordmark,
  - icon+label tab strip (`Media | Project | Quality & IQ | Identity | Duplicates |
    Run | Compare | Manual`) with accent underline on the active tab,
  - right-aligned unified Settings and Global Refresh controls (F5 refresh).
- Status bar (one row, every tab)
  - workspace root (elided, hover for full), copy/output-folder readiness,
  - last run state; accent "working…" while a scan/decode/pipeline is in flight.
- Project tab
  - project name input, `work in place` toggle, `new worktree`,
  - worktree selector (per project), import-images paths box + import action,
  - model registry list + add-model form.
- Quality & IQ / Identity / Duplicates tabs
  - feature checkboxes for the plugins mapped to that tab
    (mapping: deepface->Identity; imagededup + facet duplicate/burst->Duplicates;
    facet diagnostics->Run & Debug; remaining facet + python-ofiq + ediffiqa->Quality & IQ),
  - Identity tab additionally holds the identity-engine setup (ArcFace/YuNet paths).
- Run tab
  - selected-feature summary, run controls, run output + run summary,
  - sort-run-into-folders controls (keep/review/cull; optional in-parent dirs).
- Manual tab
  - rendered MANUAL.md with quick-link section jumps.
- Compare tab (operator-facing side-by-side visual compare tool; see below)
- Unified Settings window
  - header-adjacent entry with Media, Playback, Controls, and App categories,
  - App contains workspace root + copy/output folder, theme/font controls, and the
    Advanced / Debug surface (last model action/receipt, events, snapshot, artifacts).
- Parallel lane model/headless workflow (WP-028/WP-029/WP-030; distinct from the Compare tab)
  - primary purpose: make large-folder work parallel, explicit, attributable, and recoverable.
  - target scale: multiple project folders with 10,000+ images each; 40,000+ image sessions must not
    require one serial lane or one model guessing from a screenshot.
  - each lane is a durable work unit with a stable `lane_id`, `name`, `mode`, `folder`,
    `recursive` flag, scan status, item count, current cursor, optional selected features,
    optional claim owner, and last receipt/action.
  - lane modes:
    - `compare`: visual folder review and side-by-side navigation for the operator.
    - `review`: model/human review over a lane-bound folder or review session.
    - `batch`: headless processing over a lane-bound folder with selected features.
  - lane layout presets: `2`, `4`, `8`, and `custom` lanes, with add/remove controls capped by
    runtime configuration. The default visible workspace remains two lanes for readability, but
    expanding lanes must be obvious from the UI and manual.
  - lane setup helpers: name lane, clone lane settings, clear lane, scan lane, scan all lanes,
    optional sync navigation for compare mode, and explicit per-lane status.
  - model workflow helpers: lanes are listable, claimable, releasable, and inspectable through
    command/receipt APIs; no model should infer lane state from screenshots alone.
  - processing helpers: batch lanes run through the file-based queue with bounded concurrency;
    each lane writes receipts and run outputs separately, so failures isolate to one lane.
  - data safety: lane scans do not mutate source images; batch processing keeps existing copy/in-place
    semantics and must not delete source assets.
  - Lanes are a model/automation concept and command/receipt surface. They must not rename,
    hide, or replace the operator's Compare tool.
- Compare tab (WP-013 base behavior, WP-014 presentation + interaction)
  - one card per compare pane combining folder controls, an image-dominant viewport, and a
    navigation footer (prev/next, current/total, numeric jump, current filename);
  - per-pane folder path, recursive scan flag, current image cursor, scan/decode status;
  - folder selection through an in-app egui folder browser (drive buttons, up, descend,
    confirm) — never a native OS dialog (no-window rule applies to folder picking);
  - navigation inputs: footer buttons, numeric jump + Enter, mouse wheel over the image,
    and arrow keys when no widget holds focus (arrows act on the hovered compare pane, or the
    only pane);
  - optional "Sync panes" toggle (default off) that applies relative navigation to all
    compare panes by the same step; with it off, panes are fully independent (WP-013 contract);
  - the previously decoded image stays visible while the next decodes (no blank flash);
  - disabled controls (e.g. prev/next before a scan) are grayed out, never hidden.
- Manual panel (right panel)
  - startup and run flow instructions,
  - safety rules,
  - common failures + recovery.

### 5.1.1 Flat GUI theme and visual identity (WP-014 layout, WP-015 identity)
- Two palettes, one structure (`theme_mode` config, `FACIAL_THEME` env, Settings → App toggle):
  - Paper (light, default): warm paper desk (`#F0EDE4`), sheet cards (`#FBF9F3`),
    recessed wells (`#E5E0D3`), denim ink text (`#263859`), quiet rules (`#CDC7B8`).
  - Ink (dark): slate desk (`#1B212E`), ink sheets (`#242C3D`), dark wells (`#141924`),
    paper text (`#E9E4D6`), dark rules (`#3A4358`).
- One vermilion accent (`#D6452A`) in both modes, reserved for: active tab underline,
  at most one primary button per surface, focus/busy state. Never inside data surfaces
  (images, logs, event streams). Semantic inks: ok green, warn amber, error brick.
- Typography: Inter (vendored TTF, SIL OFL, `product/assets/fonts/`) as the UI face,
  Inter SemiBold for headings (~1.35x), monospace reserved for code/log/event text;
  egui default fonts remain in the fallback chain for glyph coverage. Counts render
  with thousands separators.
- Flat means flat: no background texture, no grid overlay, no shadows, no gradients;
  small uniform corner rounding (3-4 px).
- Identity: painted logomark (face-detection corner brackets + accent dot) beside the
  wordmark; Phosphor icon set on tabs and key buttons; OS window icon from the same
  motif; minimum window 980x640; window geometry persists (user app-data store).
- Single-row header (logomark, wordmark, icon+label tab strip with accent underline,
  right-aligned refresh) and a slim bottom status bar (workspace root, output-folder
  state, last run state / accent "working…" while busy) on every tab.
- Compare light-table: each photo sits on a white mat with a hairline edge, filename
  caption under the print (hover the image for the full path); designed empty/loading/
  error states (large faint icon + one line) centered in the well.
- Every non-Compare tab body scrolls vertically so content can never sit unreachable
  below the window edge; Compare sizes itself to the viewport instead.
- Theme guardrail: `Visuals.widgets.noninteractive.weak_bg_fill` must stay opaque —
  egui fades disabled widgets toward this color, and a transparent value makes every
  disabled widget invisible app-wide instead of grayed out.

### 5.2 State model
- `project_name`: current campaign bucket.
- `worktree_path`: selected project worktree.
- `selected_features`: chosen `plugin:feature` keys.
- `run_output`: latest `results.json` path (latest completed run summary).
- `run_summary`: not stored inline in state; derive from `run_output`.
- `debug_lines`: event stream from runtime bus.
- `workspace_root` / `copy_location`: runtime roots (Settings → App; gate on copy_location).
- `in_place`: explicit ingest mode flag, surfaced in state and snapshots.
- sort fields: `sort_run_id`, `sort_in_parent`, keep/review/cull dirs, sort status.
- identity engine: model/detector paths + load status.
- lanes: per-lane id, name, mode, folder, recursive flag, file list/count, cursor, scan/decode
  state, claim owner/status, selected features, last action/receipt, errors; `compare_sync` toggle;
  folder-picker open state.
- display: `font_size_pt`, `theme_mode` (persisted to config).

## 6) Built-in manual requirement
The app must expose an in-UI manual suitable for no-context model use.

Must include:
1. how to start and select a project
2. how to ingest assets
3. how to toggle copy vs in-place
4. how to run each source-app feature
5. where outputs are written
6. where errors and event traces appear
7. how to recover and re-run

Manual must remain visible/accessible in UI and include:
- no-window rule (`no Explorer/Browser launch from app controls`)
- explicit feature key format (`plugin_id:feature_id`)
- default copy-mode path contract (`<copy_output_folder>/runs/<run_id>/<plugin>/<feature>/`)
- in-place path contract (`<source_parent>/.facial/runs/<run_id>/<plugin>/<feature>/`)

Spec must also include:
- identity configuration paths/vars:
  - `identity_model_path` / `FACIAL_IDENTITY_MODEL`
  - `identity_detector_path` / `FACIAL_IDENTITY_DETECTOR`
  - `identity_reference_dir` / `FACIAL_IDENTITY_REF_DIR`
  - `identity_negative_dir` / `FACIAL_IDENTITY_NEG_DIR`
  - `identity_threshold` / `FACIAL_IDENTITY_THRESHOLD` (match cutoff, default 0.5)
  - `identity_margin` / `FACIAL_IDENTITY_MARGIN` (required ref-vs-neg margin, default 0.1)
  - `identity_count_threshold` / `FACIAL_IDENTITY_COUNT_THRESHOLD` (min YuNet score for a face to be *counted* in gate output, default 0.9; separate from the lower internal alignment floor)
- the copy/output location gate (`set_copy_location`) required before running any pipeline or sort action.
- general configuration matrix (file `product/config/default.json` + env overrides):
  - `workspace_root` / `FACIAL_WORKSPACE_ROOT`; `FACIAL_REPO_ROOT` (install-root override)
  - `FACIAL_DATA_ROOT` (relocate `.facial/data`), `FACIAL_WORKTREES_ROOT`
  - `copy_location` / `FACIAL_COPY_LOCATION`
  - `ingest_in_place_default` / `FACIAL_INGEST_IN_PLACE`
  - `max_debug_events` / `FACIAL_MAX_DEBUG_EVENTS`
  - `font_size_pt` / `FACIAL_FONT_SIZE` (10-48, default 19)
  - `theme_mode` / `FACIAL_THEME` ("paper" | "ink", default "paper")
  - `framing_closeup_min` / `FACIAL_FRAMING_CLOSEUP_MIN`;
    `framing_threequarter_min` / `FACIAL_FRAMING_THREEQUARTER_MIN`

### 6.2) Headless CLI and file-based command/receipt protocol
The app is fully model-drivable without a window or socket. Detailed reference lives in
MANUAL topic `swarm-command-api`; the contract:

- Directory protocol under `<api_root>` (= `<workspace_root>/.facial/data/api`, or
  `FACIAL_DATA_ROOT/api`): `commands/` (input queue, `<action_id>.json`), `processing/`
  (atomic rename while running), `receipts/` (terminal receipt per action),
  `intents/` (ui-intents the live GUI applies on its own frames, one per frame).
- Executable entry points: GUI-subsystem `facial.exe [--background]`; console-subsystem
  `facial-cli ui-inspect [...]`, `facial-cli run-queue [--once | --watch [--poll-ms N]]` (drains `commands/`; `--watch`
  loops until `<api_root>/stop` exists), `facial-cli command <path>`,
  `facial-cli command --json '<json>'`, and convenience builders for single commands.
- Backend command kinds: `list_features | list_models | list_worktrees | get_state |
  start_run | get_run_status | get_run_summary | list_artifacts | read_artifact |
  set_workspace_root | set_copy_location | sort_run | identity_status | identity_gate |
  identity_gate_dir | identity_dedup | render_eval | calibrate_threshold |
  anchor_montage | review_init | review_claim | review_decide | review_status |
  review_montage | review_export` (review queue per section 14.1, identity tooling
  per sections 14.2/14.3; full reference in MANUAL topic `swarm-command-api`).
- UI-intent kinds (applied by the live GUI, receipt on apply): `set_project |
  set_worktree | select_tab | set_features | set_in_place | import_paths | start_run_ui`.
- Receipts carry `action_id`, kind, status (`applied|rejected|ok|error`), actor,
  protocol_version, timestamps, result, error/note. Exit codes: 0 = ok/accepted/applied;
  1 = error/rejected/parse failure.
- On startup the CLI path recovers stranded `processing/` entries before draining.
- `sort_run` contract: sorts a completed run's images into `keep | review | cull`
  buckets (copy, non-destructive, into `<copy_output_folder>/keep|review|cull`), or — with
  `--in-parent` and explicit per-bucket dirs — into operator-chosen folders; returns
  per-bucket counts + total in the receipt.

### 6.3) Lanes command/receipt protocol (WP-029/WP-030)
Lanes are the broad parallel-work unit for model/headless processing and recovery.
WP-031 restores the visible operator tool as Compare; WP-029 implements the durable
headless lane registry and scan/claim/status commands. WP-030 implements bounded
per-lane batch execution through headless command/receipt verbs.

- Lane state root: `<workspace_root>/.facial/lanes/`.
- Canonical lane registry: `<workspace_root>/.facial/lanes/lanes.json`.
- Lane claim files: `<workspace_root>/.facial/lanes/claims/<lane_id>.json`; claim-by-create,
  release-by-owner, steal only when explicitly requested and recorded.
- Lane receipts: regular API receipts plus lane-specific result payloads; aggregate batch
  commands also write normal child receipts at `receipts/<lane_action_id>.json` for each
  lane result.
- Lane IDs are stable strings (`lane-001`, `lane-002`, ... or persisted equivalents), never
  positional UI labels.
- Implemented WP-029 lane command kinds:
  - `list_lanes`
  - `set_lane`
  - `scan_lane`
  - `scan_all_lanes`
  - `claim_lane`
  - `release_lane`
  - `lane_status`
- Implemented WP-030 lane batch command kinds:
  - `start_lane_batch`
  - `start_all_lane_batches`
- Planned lane UI-intent kinds:
  - `set_lane_ui`
  - `scan_lane_ui`
  - `select_lane`
  - `set_lane_preset`
- `list_lanes` returns every lane's id, name, mode, folder, recursive flag, feature keys,
  file list/count, claim owner/timestamp, and last error.
- `set_lane` sets lane name/mode/folder/recursive/features without scanning.
- `scan_lane` recursively inventories supported images for one lane and records deterministic
  sorted paths plus count; it must not decode every image or allocate UI textures.
- `scan_all_lanes` scans every configured lane and returns per-lane counts/errors, including
  failed lanes rather than hiding them. The command honors the top-level actor for owned
  claimed lanes and reports other-owner claim conflicts as lane-local errors unless `steal=true`.
- `claim_lane` atomically claims one lane for an actor. A claimed lane can still be viewed, but
  mutation and batch execution by another actor are rejected unless `steal=true`.
- `release_lane` releases a lane only for the owning actor unless `steal=true`.
- `lane_status` returns one lane or all lanes with claim, scan, last error, and latest
  batch state (`batch_status`, `batch_action_id`, `batch_updated_at`, `last_run_id`,
  `last_batch_error`).
- `start_lane_batch` runs selected features over that lane's current inventory using existing
  pipeline semantics and returns a normal run summary plus lane id. Direct `start_lane_batch`
  records `lane_status.batch_action_id` as the command `action_id`, so the status entry joins
  to the normal command receipt.
- `start_all_lane_batches` runs batch-mode lanes with bounded concurrency and returns one receipt
  summarizing per-lane child action ids, statuses, run ids, counts, and errors. Each child
  action id is also written as a normal `start_lane_batch` receipt for independent audit.
- Concurrency default is conservative and configurable; the first implementation must not launch
  unbounded threads for 40,000+ image sessions.
- Failure isolation: one lane scan or batch failure must not abort unrelated lanes; the all-lanes
  receipt reports mixed success.
- Recovery: stranded lane claims and in-progress lane batches must be visible in `lane_status`;
  recovery should prefer explicit operator/model release over silent mutation.
- Backward compatibility: existing `compare` tab vocab remains accepted as an alias during the
  transition; new documentation and UI use `lanes`.
- Research basis for the lanes design:
  - Rayon official docs: data parallelism should be bounded by a thread pool rather than ad-hoc
    unbounded worker creation.
  - Rust filesystem docs: coordination should use atomic operations such as create-new/rename-style
    claims instead of check-then-write races.
  - egui documentation and issue discussions: large scrollable views can be expensive when every row
    is laid out each frame, so lane UI must summarize large folders and render bounded visible state.

### 6.1) Identity gate output (single + batch)
The identity gate embeds an image (ArcFace ONNX), compares it to the reference/negative
sets, and — when a YuNet detector is provisioned — reports face geometry decoded from the
same detection pass (no second model run, no external `cv2`).

- `facial identity_gate --image PATH` returns one row; `facial identity_gate_dir --dir DIR`
  gates every top-level image in `DIR` in one call.
- Shared per-image fields: `verdict`, `source` (`real` — YuNet+ArcFace), `reference_similarity`,
  `negative_similarity`, `margin`, `threshold`, `required_margin`, `reference_count`,
  `negative_count`, `face_count`, `face_box` (`{x,y,w,h}` original px of the strongest face),
  `face_frac` (face area / image area — scale-bucket hint), `face_score`,
  `framing` (`close-up` | `three-quarter` | `full-body` | `none`, from `face_frac`),
  `image_w`, `image_h`, `align` (`yunet_112` | `resize_112`), `model_sha256`,
  `count_threshold`, `error`.
- `framing` thresholds are config/env driven (`framing_closeup_min` default 0.09,
  `framing_threequarter_min` default 0.03; calibrated on real buckets) and stamped in the
  manifest. `source` distinguishes the real engine path from the heuristic proxy plugins
  (deepface detect/analyze, facet, ofiq, ediffiqa, imagededup emit `source: "proxy"`).
- `verdict` vocab: `match` | `no_match` | `unsure` | `no_reference`, plus
  `no_face` (detector found no face → align fell back to resize) and `error`
  (decode/inference failed; in batch this is isolated to the one row, never aborts the run).
- `face_count` counts faces at/above `identity_count_threshold` (collage signal); the
  bounding boxes are de-duplicated by greedy IoU NMS (0.3) so overlapping anchors of one
  face are not double-counted. Decode matches OpenCV `FaceDetectorYN`.
- Curation metadata columns (WP-019 wave 1, on every gate row): `face_crop_sharpness`
  (laplacian variance over the face box; `source: real` geometry), `yaw_estimate`
  (`frontal|quarter|profile` from 5-point landmark geometry, plus `yaw_ratio`; buckets
  only), `hair_color` + `hair_confidence` (`hair_source: "proxy"` — HSV strip heuristic,
  triage-only).
- Wave 2 (WP-021, SHIPPED): when the PIPNet 98-pt landmark engine is provisioned
  (`landmark_model_path` / `FACIAL_LANDMARK_MODEL` / conventional
  `product/models/pipnet_r18_wflw_98.onnx`; lazy-loaded at first gate), every gate row
  adds `eyes_open` (`open|closed`, `source: real`), raw `ear_left`/`ear_right`
  (simplified WFLW EAR; `ear_method`/`ear_open_min` stamped so consumers can
  re-bucket), and `landmark_conf_min` (cls-map localization confidence). The decode is
  graph-external Rust (1.2x crop, ImageNet norm, argmax+offset+reverse-neighbor merge
  from the vendored WFLW meanface). **Occlusion is deliberately NOT emitted**: the
  cls-confidence proxy failed its validation gate (conf_min 0.565-0.580 across clean,
  black-eye-band, and mouth-band manipulations — no separation — while EAR separated
  0.39 clean vs 0.25-0.27 eye-banded); per the spike contract the flag is withheld,
  and honest occlusion requires a segmentation model (future packet).
  `identity_status` reports the landmark engine block.
- Detector provisioning (WP-020): YuNet 2023mar (float, MIT) is COMPILED INTO the
  binary as the default detector. Resolution: configured path override -> bundled ->
  none; a failing override falls back to bundled (`detector_origin:
  "bundled_fallback"`). Every detector load self-checks the 12-output 2023mar layout
  on a blank frame before use. `identity_status` reports `detector_origin` +
  `detector_sha256`; the model registry holds an actuated `yunet-detector` record.
  Only the ArcFace embedder remains operator-provisioned.
- **Batch artifacts** (written to `<copy_output_folder>/runs/<run_id>/`, else the gated
  dir's `.facial/runs/<run_id>/`):
  - `identity_gate.csv` — header: `path,verdict,source,reference_similarity,negative_similarity,margin,face_count,face_box_x,face_box_y,face_box_w,face_box_h,face_frac,face_score,framing,face_crop_sharpness,yaw_estimate,yaw_ratio,hair_color,hair_confidence,eyes_open,ear_left,ear_right,landmark_conf_min,image_w,image_h,align,error`
  - `manifest.json` (`schema_version: 2`) — run metadata, `summary` (per-verdict counts),
    stamped `count_threshold`, `nms_threshold`, `model_sha256`, and the full per-image rows.
  - The receipt carries `run_id`, `run_dir`, `csv_path`, `manifest_path`, and `summary`.
- Batch output is deterministic: inputs are sorted, NMS ties break by anchor index, so two
  runs over the same directory produce a byte-identical CSV.

## 7) Visual debugger behavior
- Must show chronological events with timestamps and source tags.
- Must show failed feature messages immediately after pipeline errors.
- Must expose run summary links and artifact locations.
- Must preserve recent event context for replay and model recovery.

## 7.1) GUI inspector
A built-in, self-contained GUI inspector renders every tab **headlessly** (egui computes
widget rects on the CPU; no window opens) for layout review and visual-regression checks.

- Command: `facial-cli ui-inspect [--out DIR] [--tab VOCAB ...]` (no flags = all 9 tabs).
- Per tab under `<workspace_root>/.facial/ui-snapshots/<timestamp>/`:
  - `<tab>.png` — directly viewable raster produced in-process by pure-Rust `resvg`.
  - `<tab>.svg` — labelled vector wireframe of panels/buttons/fields.
  - `<tab>.layout.json` — `screen` size + `texts`/`rects` with `x,y,w,h` and a `clipped`
    flag (geometry cropped by a ScrollArea/TextEdit clip — designed cropping vs real
    overflow) for machine review. Right/center-aligned galleys are normalized to
    top-left geometry.
  - Compare uses canonical `compare.svg` / `compare.layout.json`; `--tab lanes` may remain
    accepted temporarily as a compatibility alias but must not rename the operator-facing
    Compare tool.
  - `index.html` + `index.json`, with PNG, SVG, and layout links.
- Layout settles over 3 passes per tab (ScrollArea content memory needs the extra frame).
- When Compare mode is captured, the in-app folder browser is additionally force-opened and
  captured as `compare_dialog.svg/.layout.json` using 10 passes, so any window auto-size
  feedback loop shows up as pass-over-pass growth.
- Deterministic: an unchanged GUI yields a byte-identical `layout.json` (the
  `compare_dialog` capture lists real drive letters/folders, so it is deterministic per
  machine, not cross-machine).
- Uses egui/eframe for layout and pure-Rust `resvg` for rasterization; no browser,
  ImageMagick, desktop automation, or external converter is required.
- The inspector opens its media metadata database under the snapshot directory, not the
  configured workspace database. It can therefore run beside a live GUI without a false
  lock banner, without mutating operator metadata, and without weakening the visible
  configured-path fixtures.
- The live app and the inspector share `ui::FacialApp::render_ui`, so new widgets are
  captured automatically. Per the GUI inspection contract (CODEX 7.1), run it when GUI
  items are added/moved and as part of testing GUI changes.
- Exact live-state inspection is receipt-backed and background-safe: `facial.exe
  --background` starts without requesting activation, UI intents navigate without input
  injection, and `facial-cli ui_snapshot [--out FILE.png]` captures the live framebuffer.
  When native video is active, its diagnosed region is preserved as a sidecar: use a
  LibVLC snapshot when available, otherwise the exact visible framebuffer crop. Models must never raise or focus the
  app to inspect it; missing background coverage is a tooling defect to implement first.

## 8) Data policy and safety
- Copy mode default:
  - source files are not mutated.
  - images are copied into `<copy_output_folder>/images`.
  - processing writes into `<copy_output_folder>/runs`.
- In-place mode:
  - explicit opt-in only,
  - shown in status and run parameters.
  - source image paths are used directly.
  - processing writes into `<source_parent>/.facial/runs`.
- On ingest/import:
  - validate path set,
  - normalize directory/image list,
  - drop unsupported extensions.

## 9) Error behavior and recovery
- If feature key is invalid:
  - emit failure event,
  - show reason in run summary,
  - continue remaining features where possible.
- If no valid images:
  - emit `No images available` failure and refuse run start.
- If pipeline fails:
  - stop current run,
  - preserve partial outputs,
  - require explicit rerun after correction.
- If plugin manifest missing:
  - surface missing-manifest failure and include manifest discovery paths.

## 10) Artifact contract
- Worktree path:
  - `worktrees/<slug>/<timestamp_id>/`
- Copy-mode imported images:
  - `<copy_output_folder>/images/<image>`
- Copy-mode run path:
  - `<copy_output_folder>/runs/<run_id>/<plugin_id>/<feature_id>/<artifact>.json`
- In-place run path:
  - `<source_parent>/.facial/runs/<run_id>/<plugin_id>/<feature_id>/<artifact>.json`
- Per-run summary:
  - `<run_path>/results.json`
- Event log:
  - `<workspace_root>/.facial/data/events.jsonl`
- API root:
  - `<workspace_root>/.facial/data/api`

## 11) Extensibility
Each source-app domain remains an integration module and manifest:
- `metadata.json` defines features
- Rust module implements feature execution and artifacts

When adding a new feature:
- add manifest feature entry,
- add UI discoverability via `run_feature`,
- add deterministic schema fields in Rust output,
- add governance notes in `existing_app_audit.md`.

## 12) Out-of-scope
- direct model re-training (facial stops at the training-ready dataset + manifest;
  launching/monitoring kohya or any trainer belongs to the training host),
- GPU/process fleet management (ComfyUI lane registries, VRAM ownership, PID
  authorization belong to the orchestration repo, not this app),
- external API serving layer,
- non-debug model evaluation automation outside the local GUI pipeline.

## 13) Minimal acceptance criteria
- all five source-app feature groups are discoverable in UI
- copy and in-place mode are both observable
- no external windows/focus grabs are initiated by UI actions
- each selected feature produces JSON artifacts in run paths
- manual panel is populated and usable by first-time model/operators
- artifact paths and payload fields match the contracts in section 3

## 14) Feature contracts from field feedback (recorded + SHIPPED 2026-06-11)
Adopted from field-model feedback (second field session; see
`governance/backlog_field_feedback.md` reconciliation). Status: **14.1-14.4 wave 1 and
14.5 are implemented and runtime-validated** (WP-016..WP-020 packets carry the
evidence); 14.4 wave 2 remains research-gated. The contracts below are the live
behavior; sections 6.1/6.2 carry the integrated reference.

### 14.1 Review queue with persisted decisions + dataset lineage (WP-016)
- Every reviewable image gets a stable ID (content hash based, not positional).
- A review session is initialized from a gate run, a pipeline run, or a folder; state
  lives in a per-session machine-readable store (JSONL ledger + manifest), not chat.
- Programmatic verbs (headless command/receipt kinds): init session, claim a shard
  (parallel agents review disjoint slices), decide `accept|reject|hold` with reason,
  status/progress, export.
- Review montages are served as artifacts with stable per-tile IDs mapped to image IDs
  (no positional inference).
- Lineage: the session ledger records every stage transition
  (source -> candidates -> decided -> deduped -> exported dataset) with counts, reasons,
  and exact invocations, queryable by a no-context agent.
- Export tail: reviewed set -> deduped -> kohya-ready folder tree + manifest
  (composes the STUB-C dataset builder; facial stops at the dataset per section 12).
- GUI: a Review surface is a later slice over the same store; headless-first.

### 14.2 Anchor comparison + render-eval report (WP-017)
- Anchor set (identity_reference_dir) becomes a first-class view: anchor-paired montage
  artifact for any candidate set; GUI Compare mode gains a pinned anchor strip.
- Batch render-eval: score a folder of generated renders vs anchors, grouped by a
  config key parsed from filename/subfolder, emit per-group mean/min/max + explicit
  failure taxonomy (`no_face`/`error` are never counted as passes) (STUB-J).
- Threshold calibration: anchor pairwise self-consistency + negative-set distribution
  -> recommended gate threshold, reported not silently applied (STUB-I).

### 14.3 Embedding-based near-dup grouping (WP-018)
- `deepface:identity_dedup`-class feature: ArcFace cosine clustering so "same look,
  different crop/filter" collapses while pose/expression variety survives (STUB-B).
- Cluster IDs surface in the review-queue manifest so review groups near-dups up front
  ("these N are the same moment — pick one").

### 14.4 Per-image curation metadata, wave 1 + landmark spike (WP-019)
- Wave 1 (current 5-point landmarks suffice): face-crop sharpness (STUB-E),
  coarse yaw from landmark geometry (STUB-G), hair-color heuristic flag (HSV over the
  above-face region; labeled `source: proxy`).
- Wave 2 (research gate, do NOT build on 5-point data): eyes-open (EAR needs >=6 pts/eye)
  and occlusion detection require a landmark-model upgrade; research spike decides the
  model + tract compatibility before any contract is written.

### 14.5 Bundled default face detector, swappable (WP-020 — SHIPPED)
- YuNet 2023mar (float, MIT; license vendored at `product/assets/models/YuNet-LICENSE.txt`,
  research basis in `governance/research_bundled_detector.md`) is embedded in the binary
  (`include_bytes`, ~232 KB) as the default detector: face detection works out of the box
  with `source: real`.
- The detector remains swappable: `identity_detector_path` / `FACIAL_IDENTITY_DETECTOR`
  override the bundled model; a failing override falls back to bundled
  (`detector_origin: "bundled_fallback"`, runtime-verified). `identity_status` and the
  actuated `yunet-detector` registry record carry origin + sha256.
- Startup self-check: every detector load must execute a blank frame and expose the
  12-output 2023mar layout, or the load is rejected — wrong exports can never silently
  produce wrong geometry.
- Scope note vs the original ask: bundling covers the DETECTOR (the operator ask). The
  ArcFace embedder (~166 MB) stays operator-provisioned; the full identity gate (verdicts)
  therefore still requires the embedder. A zero-provisioning gate would require bundling
  or auto-pulling the embedder (STUB-Q) — explicitly out of this packet.

## 15) Media browser front surface — redo contract (WP-042..WP-049, 2026-08-06)

Operator directive: the first media-browser pass (WP-033..WP-041) was produced by a
low-reasoning, non-multimodal model and is assumed failed; multimodal baseline
inspection confirmed the core asks were not delivered (no thumbnails anywhere,
compare-lane vocabulary leaking into the front surface, placeholder controller and
remap surfaces, token-overlap "semantic" search, per-tab overlap/clipping defects).
The redo re-founds the front surface on eight packets:

- **WP-042** media metadata database: redb store at `<workspace_root>/.facial/media/media.redb`
  (notes, tags, the original seven-color single-label schema, favorites, settings), one-shot migration from the JSON
  scaffold, workspace-relative keys, and headless `media_meta_*` / `media_fav_*`
  receipt commands so models drive metadata without the GUI. WP-061 supersedes only
  that original fixed-seven/single-label schema with the dynamic multi-label contract.
- **WP-043** thumbnail engine: off-thread decode workers, sharded disk cache under
  `.facial/media/thumbs/`, RAM/texture LRUs with eviction, Exif orientation, error-tile
  memoization, bounded per-frame texture uploads.
- **WP-044** book-style explorer rebuild: dedicated `media_explorer.rs` surface — two
  canonical panels (**Library panel** left, **Viewer panel** right) with a draggable gutter, full-window grid
  mode, folder strip pinned at the top of the grid scroll (scrolls away), chrome-hide
  shortcut, minimalist single-row toolbar, clickable favorites overlay, empty-state
  Browse affordance. Compare-lane file-op plumbing is reused through an adapter;
  Compare tab itself is untouched.
- **WP-045** Explorer action parity: cut/move, rename (F2, collision-safe), new folder,
  refresh (F5), sort; one unified themed context menu on every media surface, including
  color-label and favorite actions. Windows reserved-name validation.
- **WP-046** input layer: action-mapped keyboard + full controller navigation (d-pad
  grid moves, stick scroll, folder up/down and sibling jumps, R3 favorites default,
  zoom triggers), real rebind-by-capture UI with persisted bindings and conflict
  detection, repaint liveness only while a pad is connected (WP-010 idle guarantee kept).
- **WP-047** search v2: fzf-style fuzzy scorer, keyboard-navigable autocomplete
  dropdown, tag/label/kind filter chips, and real vector-semantic search — CLIP ONNX
  image+text encoders (operator-provisioned under `product/models/`, tract runtime)
  feeding a redb embedding index with cosine ranking; graceful labeled fallback to the
  local metadata scorer when models are absent; headless `media_index_build` /
  `media_search` receipts.
- **WP-048** app-wide visual overhaul to the operator's brutalist design language
  (refined 2026-08-06): flat white paper with a deterministic rough-grain texture,
  near-black ink at high contrast, thin SOLID BLACK 1px rules for panels/tabs/windows,
  sharp corners everywhere (rounding 0), no cards (structure from rules + whitespace),
  monochrome controls (black primary buttons, black active-tab underline) with
  vermilion demoted to functional signals only, Space Grotesk display headings +
  Inter body + IBM Plex Mono data faces (vendored, OFL). Ink mode inverts the same
  structure. Every baseline defect enumerated in the packet (tab-row collisions,
  Options/Identity field overlaps, Manual sidebar overflow, Compare duplicate action
  row + on-screen duplicate widget-ID warnings) is repaired with no functional
  redesign of legacy tabs.
- **WP-049** documentation + model-surface sync: Manual media chapter rewrite,
  reference CLI/intents, spec/topology/taskboard consistency, final visual + packaging
  validation sweep.

Acceptance for the front surface as a whole: an operator can browse a 5k-image folder
as a book (or full-window wall) with live thumbnails, drive everything from the
attached game controller alone, search by name/fuzzy/tags/notes/semantic, and a
no-context model can operate every new feature headlessly from the Manual.

### WP-050 operator presentation and scale correction (2026-08-09)

- Media remains the launch/front tab and fresh state defaults to recursive **All**
  media, 500-point thumbnails, and hidden filenames with a persisted **Names** toggle.
- The folder strip remains at the top of the scrolling **Library panel**. Its divider and
  the center book divider paint only short minimal handles while retaining practical
  drag hit targets.
- Ctrl+F replaces the former default favorites binding and toggles native borderless
  fullscreen with only the Library and Viewer panels visible; Ctrl+B opens Favorites and Escape
  always restores the normal window.
- Settings is a large, centered, resizable, vertically scrollable in-app window.
- Recursive scans publish bounded batches before final sorting; display-index work is
  cached by relevant state generations; each frame clones/requests only visible and
  overscan paths; thumbnail workers discard stale viewport generations.
- Scrollbars use a large 24-point floating grab region and a minimum 64-point handle.
  They are fully hidden at rest and appear/disappear on hover/interaction with a short
  animation, preserving both accessibility and the minimal visual surface.
- Thumbnail images and the full **Viewer panel** have no black frame. Tags and notes
  have no border and use a slightly darker recessed fill as their input affordance.
- The headless inspector must cover filename-on, settings-popup, fullscreen-book, and
  hovered-scrollbar states in addition to the existing Media presets.

### WP-051 couch-distance controller folder navigator (2026-08-09)

- Preserve the compact folder strip and its desktop interaction density.
- The compact strip and both in-app folder browsers expose assigned filesystem roots;
  Windows roots come from `GetLogicalDrives` rather than probing every letter.
- A visible **Folders** toolbar button, Ctrl+G, and the remappable controller
  Select/Back action open a large centered in-app folder navigator; no external
  Explorer window is launched.
- The window uses a stable 1800×1360 preferred size clamped to the current viewport;
  content must not cause it to grow between frames. Its non-interactive frosted veil
  slightly softens the Media surface underneath without hiding browsing context.
- Folder names and icons use 52-point text in 112-point rows.
  Controller focus uses a whole-row fill plus a vermilion marker and is always scrolled
  into view.
- Assigned disks use a fixed horizontal controller-focus rail above the virtual folder
  rows, so many drive letters never displace the current folder's children. Left/Right
  traverses the rail and Down returns directly to the first folder row.
- D-pad and left stick navigate, A/Right enters, B/Left moves to the parent (focusing
  the current drive instead of closing at a filesystem root), and Select/Back or Escape closes. Input is trapped by the
  navigator while open so hidden thumbnail actions cannot fire.
- Select/Back migrates only from the exact legacy Open-location default; custom
  bindings are preserved and Open file location remains keyboard/remap accessible.
- Facial must not steal Steam's Guide+Start/Menu Alt+Tab chord: Start/Menu is reserved
  outside the remappable table for Facial's built-in Alt+Tab fallback, the exact legacy
  Start->Settings default migrates away, controller actions are suppressed while Guide
  is held, and background Facial windows ignore controller input. Before switching,
  Facial releases simulated pointer buttons and disables pointer mode. Settings remains
  clickable, Ctrl+P accessible, and remappable.
- Child-folder enumeration reuses the cache and the large list virtualizes visible rows.
  Existing scan IDs reject stale results during rapid controller navigation.
- The receipt-backed `media_folder_navigate --action ACTION` UI-intent exposes
  open/close/toggle/move/page/home/end/enter/parent/refresh transitions to models and
  invokes the same navigator state changes without fragile foreground input injection.
- Inspector coverage includes normal, long-list, deep-path, empty-folder, and
  fullscreen navigator states.

### WP-052 video thumbnails and embedded playback (2026-08-09)

- Visible and overscan video paths enter the existing generation-cancelled thumbnail
  queue, but one dedicated video worker runs FFmpeg so video extraction can never
  consume image decoder workers. Extracted frames use the same sharded 256/512 JPEG
  cache; each attempt is capped at five seconds and failures are memoized.
- Resolve FFmpeg from `FACIAL_FFMPEG` or PATH. Absence/failure retains a film-strip
  fallback without blocking folder scans, selection, or scrolling.
- The **Viewer panel** loads LibVLC only after Play/Enter/controller A on the selected
  video. It embeds a native VLC surface and exposes large couch-readable play/pause,
  timeline scrub, volume, audio-track, and subtitle-track controls.
- Resolve VLC from `FACIAL_VLC_DIR`, a portable `vlc` folder beside Facial, standard
  Windows Program Files locations, or PATH. VLC absence is a local playback error and
  does not weaken media browsing or video thumbnails.
- **Open in VLC** explicitly launches VLC; **Choose app…** explicitly opens the Windows
  app selector. No external player or selector launches during scanning, selection,
  thumbnail generation, automated inspection, or model diagnostics.
- Native video is hidden behind in-app overlays and stopped when leaving Media or
  selecting another item so it cannot cover controls or leak audio into another view.
- `media_video_control --action ACTION [--value N] [--out FILE.png]` provides receipt-backed
  status/play/pause/stop/seek/volume/audio-track/subtitle-track control for no-context
  models; applied status/control receipts return structured live player and track state.
- The `capture_frame` action exports the current native LibVLC frame to a PNG and returns
  its resolved path/existence in the applied receipt. Together with the deterministic
  `media_video` inspector preset, this makes both the egui controls and otherwise
  uncapturable native video surface inspectable without desktop automation.
- `FACIAL_TEST_SILENT=1` adds LibVLC `--no-audio` for automated diagnostics; routine
  scan, thumbnail, and inspector tests never start playback.

### WP-053 playback, navigation, search scope, and unified settings (2026-08-09)

- Embedded video looping defaults on and is persisted in Media settings. The Playback
  category can toggle it; changing an active video recreates its in-app player at the
  current timestamp so the preference applies immediately.
- Controller video defaults preserve grid navigation: A/Enter plays or pauses, the right
  stick seeks left/right and changes volume up/down. The responsive Controls grid exposes
  every keyboard/controller binding and supports persisted remapping.
- The former Options tab is absorbed by one header-adjacent Settings surface with Media,
  Playback, Controls, and App categories. Ctrl+F is called **Fullscreen** everywhere.
- Media Refresh/F5 rescans only the selected folder using its media-kind dropdown and Tree
  setting. Search ranks only that selected scan; it never silently searches the whole PC.
  Header Global Refresh remains the separate reload of models, worktrees, features, manual,
  and retryable thumbnail failures.
- The folder navigator shows mapped network drives and accepts direct UNC input such as
  `\\server\share` in-app. It validates the target and reports unavailable shares without
  opening Explorer or creating a network mapping.
- Media context menus copy absolute file paths and portable paths. Portable means
  workspace-relative when possible and otherwise relative to the selected media root.
- `media_video_control --action loop --value 0|1` exposes the same persisted loop setting
  for no-context models.

### WP-055 NAS media responsiveness and Settings stability (2026-08-09)

- Recursive media discovery minimizes NAS round trips by using directory-entry metadata,
  preserving exact traversal semantics, publishing a small first batch, and sending
  efficient bounded follow-up batches that stale scan IDs can cancel.
- The existing redb store retains a generation-tagged last-good media inventory. The UI
  can present that inventory immediately while a background reconciliation runs; an
  incomplete, cancelled, or unavailable-root scan never commits deletions and instead
  leaves the inventory visibly stale/offline.
- Safely proven mapped-drive and UNC aliases for the same configured root share a stable
  media-root/cache identity. Unrelated shares must never be merged by hostname, IP, or
  path-string heuristics alone.
- The hot Media draw path performs no synchronous filesystem existence, metadata, or
  directory-enumeration calls. Selected-root validity and child folders are cached in
  background state, and long child-folder lists render only visible rows.
- Active video interaction repaints at 30-60 FPS and updates visible command state
  immediately; LibVLC state reconciliation is bounded and idle/paused polling remains
  low-frequency. Any remote-file caching is bounded/configurable and selected from
  measured start/seek behavior rather than a guessed constant.
- Settings uses one stable viewport-clamped outer rectangle independent of category
  content, with fixed title/footer controls and one explicit inner scroll region. The
  Close control remains visible across categories, font scales, and supported viewport
  sizes.
- Settings reuses the folder navigator's translucent softened-background veil. Clicking
  the veil or pressing Escape closes Settings, consumes the interaction before hidden
  Media controls receive it, and flushes the existing live auto-save path. There is no
  generic Apply/Save prompt or transactional Settings redesign.
- Structured diagnostics expose scan first-batch/total timing, errors, inventory state,
  filesystem stalls, thumbnail work, player polling, and input-to-command latency. The
  visual inspector covers long settling, category-switch sequences, constrained
  viewports, high font scale, backdrop close, and click-through prevention.

### WP-056 media query, sort, and stat responsiveness (2026-08-09)

- The Media UI performs no complete-collection search preparation, relevance ranking,
  final sorting, stat traversal, or cached child-folder preparation during a render
  frame.
- Normalized search rows are immutable per inventory generation. Query work is
  debounced, coalesced, cancellable, and computed off-thread; results publish atomically
  only when root identity, inventory generation, and query ID still match.
- Final sort and Size/Modified metadata work is off-thread, bounded, cancellable, and
  failure-aware. Unknown metadata is not presented or persisted as a valid zero value.
- Cached child-folder display entries are shared immutably and only the visible range is
  allocated and laid out during paint.
- Structured diagnostics expose query/index/sort/stat duration, queue depth,
  cancellations, stale-result drops, and UI-frame stalls.

### WP-057 remote media I/O arbitration and playback priority (2026-08-09)

- Remote media work uses one root-aware coordinator with bounded classes for visible,
  playback, interactive metadata, prefetch, and bulk reconciliation work.
- Visible requests have reserved capacity. Playback and direct interaction throttle
  prefetch and bulk work with bounded hysteresis while bulk reconciliation continues at
  reduced capacity and resumes normally afterward.
- Remote-root concurrency is independent of logical CPU count and is tuned separately
  from local-root concurrency.
- Mapped-drive/UNC proof is cached once per configured root generation; thumbnail cache
  identity derivation does not repeatedly call mapped-drive resolution and never merges
  unrelated roots.
- Coordinator permits are ownership-bound and return on success, error, cancellation,
  stale generations, and worker shutdown.
- Diagnostics attribute queue wait/depth, active work class, cache hits, filesystem
  latency, player command/poll latency, and UI-frame stalls. VLC caching remains bounded
  and configurable and is changed only from measured start/seek/stall evidence.

### WP-058 Media Settings interaction and immersive viewer correction (2026-08-09)

- The only Settings entry is beside header Global Refresh. Settings remains one in-app
  window with Media, Playback, Controls, and App categories; the obsolete Media-toolbar
  Settings toggle is absent.
- Settings captures the unobscured Media viewport before opening and presents it as a
  soft Gaussian-blurred, untinted backdrop. The backdrop never shifts global exposure or
  saturation, never blocks Settings controls, consumes outside clicks, and has a neutral
  usable fallback when capture is unavailable.
- Ctrl+F fullscreen allocates the complete **Viewer panel** to the selected image/video and
  hides tags, notes, favorite/rating-like star, and color labels. Normal viewing reduces
  fixed metadata/control reservations and fits media to the maximum intentional area.
- Fullscreen playback controls occupy a transparent bottom strip, appear only while the
  video/control region is hovered, and hide without stopping playback.
- WP-058 color-label definitions use stable IDs plus editable operator-facing names and opaque
  backend `#RRGGBB` values. Existing per-asset label IDs survive rename/recolor. The UI
  shows swatch pickers and names without hex; structured receipts expose all fields.
  WP-061 supersedes the fixed seven-slot/single-assignment limit while preserving those IDs.
- Video tiles retain a play affordance. Inline tile playback may use only the existing
  single lazy native LibVLC player and ships only if the WP-058 local/synthetic and exact
  available NAS responsiveness gates pass; no per-tile decoder pool or hover autoplay is
  allowed.

### WP-059 installer-root delivery artifact and versioning contract (2026-08-09)

- `installer/` contains exactly one current versioned portable executable and one current
  versioned setup executable: `facial-portable-<version>.exe` and
  `facial-setup-<version>.exe`.
- Every superseded delivery executable is preserved under
  `installer/installer-portable-archive/`; legacy delivery paths are migrated there and
  are not valid steady-state artifact surfaces.
- Each successful packaging run increments the Cargo patch version exactly once and uses
  that same version in Cargo, topology, and both root artifact names. A failure before
  publication restores the prior version authority and leaves the current delivery pair
  untouched.
- Setup offers explicit Desktop and Windows Start-menu/All-apps shortcut choices and a
  checked completion-page action to launch Facial as the installing user. It does not
  claim or attempt unsupported forced placement in the user's Start pinned grid.
- The delivery-layout guard rejects missing, extra, mismatched-version, legacy, transient,
  or out-of-repository executable surfaces.

### WP-060 embedded video visibility and canonical Media panels (2026-08-09)

- Canonical terminology is **Library panel** for the left folder/thumbnail overview and
  **Viewer panel** for the right selected-media/playback/metadata surface. UI copy, code,
  diagnostics, manual, topology, and model/operator handoffs use those exact terms.
- The layout setting is **Library / Viewer split**. Persisted `two_panel` / `full_grid`
  values remain compatible.
- The one lazy LibVLC player can visibly render in either a Library thumbnail tile or the
  Viewer panel, one owner at a time. Handoff never creates a second decoder.
- Embedded Windows playback defaults to VLC `wingdi`, which composes into the verified
  child HWND instead of relying on Direct3D overlay behavior that can leave affected
  DPI/driver combinations with audio and a visually blank host. `FACIAL_VLC_VOUT` is a
  validated expert override for accelerated renderers after machine-specific visual proof.
- `media_video_control --action play_library` is the receipt-backed Library placement;
  ordinary `play` targets the Viewer panel.
- The native video child binds to the authoritative eframe Win32 parent handle rather
  than a focus-derived active window. Diagnostics expose parent/child handles, requested
  and observed bounds, visibility, and LibVLC HWND attachment.
- Live native-surface proof is required for both placements. The background-safe
  `ui_snapshot` route combines the exact renderer framebuffer with a LibVLC snapshot or
  exact visible-region sidecar at the diagnosed native bounds, so models can prove Library and
  Viewer placement without activating, raising, focusing, or clicking the app.

### WP-061 dynamic multi-label catalog and assignments (2026-08-09)

- A label definition has an immutable stable ID, unique case-insensitive operator name,
  unique canonical opaque `#RRGGBB` color, and stable order. Catalog size is dynamic;
  create, rename, recolor, and confirmed remove are supported.
- Existing red/orange/yellow/green/blue/purple/gray IDs migrate intact. Per-file values
  migrate from one legacy ID to an ordered, deduplicated list of zero or more IDs.
- The Viewer label manager creates a label, adds existing labels, and removes assigned
  labels through one dropdown. Settings → Media lists the complete catalog with CRUD and
  usage counts; deleting an in-use label requires explicit confirmation and atomically
  removes its assignments.
- Library thumbnails show bounded top-right color badges plus `+N` overflow. Favorite and
  playback affordances occupy separate lanes and never overlap label badges.
- Render frames use in-memory path-to-small-vector and ID-to-definition maps only. No
  label database or filesystem work occurs while painting visible tiles.
- Current `label:` search resolves dynamic names or stable IDs and tests membership.
  This compatibility change is not the broader search overhaul.
- Live intent/receipt commands provide catalog and assignment operations while the GUI
  owns the exclusive redb handle.

### WP-062 compact Controls and couch-fullscreen Settings (2026-08-09)

- Normal Controls uses one centered, width-capped mapping table rather than inheriting
  the Media panel split or stretching across the complete Settings width.
- Action, Keyboard, and Controller headings remain explicit. Every mapping cell contains
  its binding or the word **Unassigned**; workflow groups and controller status/guidance
  remain visible.
- Narrow layouts use labeled stacked binding rows rather than clipping the Controller
  column.
- Settings offers a transient couch-fullscreen mode with a viewport-inset fixed surface,
  local 28–32 point typography, and 44–52 point hit targets. It does not mutate the
  persisted global font preference.
- Normal and couch modes use separate stable window identities and retain one content
  ScrollArea plus fixed header/footer allocation. Category content cannot grow the outer
  Settings bounds.
- Exiting couch mode restores the exact prior app fullscreen state. The first Escape
  leaves couch mode while keeping Settings open; normal Settings Escape then closes.

### WP-063 true GUI/CLI split, transactional folder navigation, Media tabs, and regression recovery (2026-08-10)

- The installed and portable `facial.exe` uses PE subsystem `WINDOWS_GUI` and starts the
  egui application without a console allocation or batch-file intermediary. Terminal,
  automation, inspection, and probe commands run through console-subsystem
  `facial-cli.exe`; both binaries call the same Rust library command implementation.
- Installer shortcuts and completion launch target `facial.exe` directly. The installed
  GUI derives writable configuration and its default workspace below
  `%LOCALAPPDATA%\Facial` internally; explicit environment overrides remain supported.
- Media hierarchy is **Media → persistent folder tabs → Library panel + Viewer panel**.
  Tabs share the media database, thumbnail/CLIP caches, and single LibVLC player but
  preserve independent folder, selection set, cursor, query/filter, sort, thumbnail
  layout, Library scroll, and staged folder-navigator state. The versioned tab document
  is written before any visible tab mutation; corrupt/duplicate state falls back safely
  while preserving the complete rejected raw value under `media_tabs_v1_rejected` for
  recovery. If the recovery write fails, the primary value remains untouched and tab
  persistence is blocked for that session rather than overwriting the only recoverable
  record.
- Tab commands cover list/select/open/close; keyboard access is Ctrl+T, Ctrl+Tab,
  Ctrl+Shift+Tab, and Ctrl+W. Opening a folder in a new tab preserves the old tab and
  selects the exact requested path. Activating a recently used large-folder tab restores
  a bounded last-good runtime inventory before background reconciliation.
- The Folders window uses the same captured, downsampled Gaussian backdrop lifecycle,
  neutral fallback, outside-click dismissal, and viewport clamping as Settings. Browse,
  Parent, drive selection, and Go mutate only the staged path. Only **Open folder**
  commits and scans the current tab; **Open in new tab** creates/selects another tab.
- Controller acquisition is Steam-independent: a Windows joystick route acquires
  supported HID/DirectInput-compatible pads directly before WGI initialization, while
  gilrs/WGI remains available for controllers absent from that direct route.
  Start/Menu performs one balanced Alt+Tab on its focused rising edge, releases pointer
  state, and cannot repeat while held. `facial-cli controller-probe` exposes both routes.
- Headless `ui-inspect` disables both acquisition routes before the first render so a
  connected controller cannot navigate the fixture or synthesize application switching.
- Playback has one explicit owner, `library` or `viewer`. Starting either placement
  replaces the other, playback work throttles lower-priority thumbnail/scan work, and an
  exact requested video remains pending for ten seconds during ordinary display
  publication or for a bounded maximum of 120 seconds while its same large-folder scan
  is still reconciling. Terminal publication relocates the exact canonical file and
  restores the ordinary ten-second cutoff. External open hands the exact UTF-16 path to
  the Windows registered application instead of spawning an assumed `vlc.exe` binary.
- LibVLC's one-time plugin/instance initialization is warmed on a bounded background
  startup worker while normal service/model startup runs; explicit Play never performs
  that warm-up on the UI thread.
- Verification includes native visible child-surface bounds and advancing playback in
  both Library and Viewer on the operator's local folder and the recursive 141,787-video
  mapped-drive folder, constrained folder-modal screenshots, full `ui-inspect`, complete
  Rust tests, package subsystem assertions, and the canonical installer layout guard,
  which independently extracts both binaries from the compiled setup and rejects shell
  wrapper shortcut targets.

### WP-064..WP-071 operator field-report correction (2026-08-12)

Corrections raised from operator testing of the WP-063 build. Each item below
supersedes the corresponding statement in the WP-050..WP-063 sections.

- **Folder-navigator command window (WP-064).** The navigator is logically active
  from the moment it is requested, including the pre-open backdrop-capture window
  during which its visible flag is deliberately false. Commands arriving in that
  window are accepted and settle the capture rather than being rejected with
  "folder navigator is closed". Every terminal outcome of `Open in new tab`,
  including a persistence failure or the 256-tab refusal, closes the navigator and
  states the reason. A backdrop request never overlaps a receipt-backed model
  capture, and a modal capture can no longer consume a model snapshot reply.
- **Tab restore (WP-064).** A tab's runtime inventory is cached whenever it has
  rows, no longer requiring a committed inventory generation, so a folder whose
  scan was interrupted or contained one unreadable subdirectory still restores.
  Activation republishes the tab's last display order in the activation frame; the
  authoritative order still recomputes and replaces it.
- **Video surface placement (WP-065).** The native child is clipped to the panel
  that owns it and hidden when that intersection is empty, so a partially scrolled
  tile cannot paint video over the toolbar or Viewer. `hide` is authoritative
  against live window visibility, clears the cached bounds, and invalidates the
  vacated parent rectangle. The Viewer yields the child to the Library only when
  the Library tile actually rendered in that frame. A folder change makes an
  explicit decision about active playback: media outside the incoming folder is
  stopped and released. Placement is traced (`vlc.show_at`, `vlc.clip`,
  `vlc.hide`, `vlc.stop`, `ui.folder_change.stop_playback`).
- **Search results are activatable (WP-066).** A file suggestion carries its
  canonical path, source index, and index generation, because a file name is not
  unique across a recursive inventory. Activation resolves within the producing
  generation, falls back to an exact-path relocation, and reports an explicit
  unavailable state rather than opening a different file. Plain activation selects
  and reveals in the current tab; Ctrl-activation opens a new tab rooted at the
  file's folder with that file selected.
- **Search scope and subtractive filters (WP-066).** A per-tab folder-only scope
  filters the loaded inventory by direct-child membership and never triggers a
  rescan, so the recursive inventory is retained. The chip grammar gains a leading
  `!` or `-` negation marker across `tag:`, `label:`, `kind:`, `note:`, the new
  `fav:` term, and bare words; quoted terms remain literal so hyphen-leading
  filenames are unaffected. All terms AND together, with subtraction applied after
  additive selection. Grouping and OR remain out of scope.
- **Favorites and labels as a collection tab (WP-067).** The tab record carries a
  kind discriminant (`folder` default, `collection`) plus a sub-view and a stable
  label ID. A collection tab renders through the same Library/Viewer viewport but
  builds rows from the in-memory metadata cache, never the filesystem, and
  publishes rows and a display order without starting a scan. Sub-views are
  favorite videos, favorite images, and the created color labels. Label CRUD
  remains solely in Settings. Records written before the discriminant existed load
  as folder tabs.
- **Per-tab ordering (WP-068).** Sort keys are Name, Modified, Size, and Created,
  each ascending or descending, held per tab. Creation time comes from the same
  single metadata call that yields size and modified time; values the volume does
  not record sort last in both directions.
- **Thumbnail-first load order (WP-069).** Every scan batch publishes an
  immediately renderable display order regardless of active query or sort key, and
  a published order is never blanked while a cached inventory reconciles. The
  canonical key for a visible tile is cached rather than recomputed per frame.
- **Presentation corrections (WP-070).** A scrollable folder strip nested inside
  the scrollable Library grid reserves its own scrollbar lane, and only while it
  actually scrolls, so the two bars no longer share an x band. Both Viewer
  scrubbers derive their width from the row's measured trailing widgets instead of
  a fixed reserve or egui's 100-point slider default. Japanese, Korean, Thai,
  Chinese, and emoji coverage is provided by optional Windows system faces
  resolved through the platform font directory; each load is independently
  fallible and absence degrades silently. Emoji render monochrome.
