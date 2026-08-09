---
file_id: facial-manual
file_kind: built_in_manual
updated_at: 2026-06-10
---

# FACIAL — Built-in Manual

<topic id="contents" summary="Index and quick links to every section">

## Contents

1. [Purpose](#purpose)
2. [Quick Start](#quick-start-no-context-model)
3. [Start / Select a Project](#start-the-app-create--select-a-project)
4. [Ingest Assets](#ingest-assets-in-app-path-entry-no-file-window)
5. [Features per Plugin](#features-per-plugin-and-their-outputs)
6. [Tools Overview](#tools-overview)
7. [Visual Debug Tool](#visual-debug-tool-run--debug-tab)
8. [Swarm Backend Command API](#swarm-backend-command-api-file-based-command--receipt-protocol)
9. [Headless CLI](#headless-cli)
10. [Driving the Frontend](#driving-the-frontend-through-the-debug-tool)
11. [Options & UI Preferences](#options--ui-preferences)
12. [Output / Artifact Paths](#output--artifact-paths)
13. [Errors & Events](#where-errors-and-events-appear)
14. [Failure Recovery & Rerun](#failure-recovery-and-rerun)
15. [No-Window Safety Rule](#no-window-safety-rule)
16. [Sort & Copy-Location Gate](#sort--copy-location-gate)
17. [Identity](#identity)
18. [Picture Compare](#picture-compare)

In the in-app **Manual** tab, use the **Quick links** row at the top to jump to
any section; in the GUI the manual is grouped under the same topic ids.

</topic>

<topic id="purpose" summary="What facial is and what it does">

## Purpose

FACIAL is a single self-contained Rust desktop application for running
facial-image quality, identity, and duplicate-detection passes over batches of
images. It bundles five source families as Rust modules — `facet`,
`python-ofiq`, `deepface`, `imagededup`, and `ediffiqa` (spec name `eDifFIQA`) —
and exposes every feature through both an egui GUI and a headless CLI.

Design constraints (all enforced in the shipped binary):

- One Rust binary, no external API serving layer (no HTTP, no sockets).
- No action opens an external OS window or grabs focus. There are NO file
  pickers, NO explorer/finder/browser launches, and NO UI-spawning subprocesses.
- Model and backend navigation happen entirely through a FILE-BASED command +
  receipt protocol and a headless CLI mode.
- All execution and model actions emit events to
  `<workspace_root>/.facial/data/events.jsonl` unless `FACIAL_DATA_ROOT`
  overrides the data root.
- Default ingest mode is `copy` (non-destructive); `in_place` is explicit and
  surfaced in state.

This manual is generated from the actual implementation (`product/src/api.rs`,
`service.rs`, `ui.rs`, `main.rs`, and the five `product/plugins/*/metadata.json`)
so a no-context model can operate the app end to end, and an operator can read
it straight through.

</topic>

<topic id="quick-start" summary="Fastest path from zero to a completed run">

## Quick Start (no-context model)

Fully headless, no GUI required. Replace paths with real ones on your machine.

1. Set or verify the runtime workspace root for the project you are operating:
   `facial set_workspace_root --path C:/my-project`
   `facial get_state`
2. List what features exist:
   `facial list_features`
3. Ensure a copy/output folder is configured, then start a run end to end
   (normalizes inputs, copies by default, runs the passes, writes
   `results.json`, prints the receipt):
   `facial set_copy_location --path C:/facial-output`
   `facial start_run --project demo --image C:/photos --feature facet:quality_pass --feature deepface:detect`
4. Read the run id from the printed receipt (`result.run_id`), then read the
   summary:
   `facial get_run_summary --run-id <run_id>`
5. List and read artifacts:
   `facial list_artifacts --run-id <run_id>`
   `facial read_artifact --path <one path from the list>`

To drive the live GUI instead, drop ui-intent command files into
`<api_root>/commands/` (or use `facial command ...`) and run the GUI; see the
`swarm-command-api` and `frontend-driving` topics.

</topic>

<topic id="start-select-project" summary="Create or select a project and worktree">

## Start the App, Create / Select a Project

Launch:

- `facial` or `facial gui` — launch the egui GUI (default with no args). The GUI
  opens on the **Manual** tab on first paint.
- Any other first argument is treated as a headless CLI subcommand and the GUI is
  never launched (see `headless-cli`).

A **project** is just a name. The **workspace root** is the folder that owns
runtime state for the active external project. It can be selected in the GUI,
through `FACIAL_WORKSPACE_ROOT`, config `workspace_root`, or
`facial set_workspace_root --path DIR`. Runtime state lives under
`<workspace_root>/.facial/`. A **worktree** is still available under
`<workspace_root>/.facial/worktrees/<project-slug>/<timestamp>_<short-uuid>/`,
but normal run output follows the selected destination mode: copy mode writes
under the configured copy/output folder, and in-place mode writes under the
source parent's `.facial/runs` folder.

In the GUI **Project** tab:

1. Type a name into "Project name".
2. (Optional) toggle "Work in place".
3. Click "New worktree" only when you need an explicit internal workspace, OR
   just import images and run features using the selected destination mode.
4. The worktree tree (project -> run dirs) is shown under "Worktrees"; click any
   run to make it the current worktree.

Headless equivalents:

- `facial list_worktrees` — list every project and its run dirs (canonical
  backend state).
- `facial set_project --project NAME` — ui-intent: set the live GUI project name.
- `facial set_worktree --worktree PATH` — ui-intent: select an existing worktree.

Note: backend commands (`start_run`, etc.) create their own worktree when none is
supplied, so you do not have to create one first.

</topic>

<topic id="ingest" summary="In-app path-entry ingest, copy vs in-place">

## Ingest Assets (in-app path entry, no file window)

There is no OS file picker. You provide paths as text.

GUI (Project tab, "Import images"): paste one file OR directory path per line
into the multiline box, then click "Import images". Directories are accepted —
the pipeline walks them recursively. Supported extensions:
`jpg, jpeg, png, webp, bmp, tif, tiff, gif`. Unsupported extensions are dropped.

Headless / model-driven:

- `facial import_paths --project NAME --image PATH [--image PATH ...] [--in-place]`
  — ui-intent: ingests into the live GUI's project worktree (requires a running
  GUI to apply).
- `facial start_run --project NAME --image PATH ... --feature KEY ...` — backend
  command: normalizes the image paths (files + recursive directory walk) and runs
  immediately, fully headless. This is the path a no-context model should prefer.

### Copy vs in-place

- **Copy (default, non-destructive):** each source file is copied into
  `<copy/output folder>/images/`. Source files are never mutated. Name collisions
  get a uuid suffix. Runs write to `<copy/output folder>/runs/<run_id>/`.
- **In-place:** opt-in. The app keeps the original source paths and does not
  create symlink/hardlink surrogates. Runs write to
  `<source parent>/.facial/runs/<run_id>/`.

Toggle copy vs in-place:

- GUI: the "Work in place" checkbox in the Project tab.
- Headless: `--in-place` on `import_paths` / `start_run`, or the
  `facial set_in_place [--in-place]` ui-intent.
- Default: controlled by `ingest_in_place_default` (config `default.json` /
  env `FACIAL_INGEST_IN_PLACE`); ships `false` (copy).

Important: `start_run` (backend) also honors the destination mode. In copy mode
it copies valid image inputs into `<copy/output folder>/images/` before running.
With `--in-place`, it runs against the original paths and writes results under
the source parent.

</topic>

<topic id="features" summary="Every feature of every plugin and its output file">

## Features per Plugin and Their Outputs

Feature keys use the format `plugin_id:feature_id`. All outputs are JSON written
under the active run path. Copy mode uses
`<copy/output folder>/runs/<run_id>/<plugin_id>/<feature_id>/<feature_id>.json`.
In-place mode uses
`<source parent>/.facial/runs/<run_id>/<plugin_id>/<feature_id>/<feature_id>.json`.
A per-run `results.json` aggregates every plugin result. The plugin set and
feature ids below come directly from the five `metadata.json` manifests; output
field contracts come from spec section 3.

### facet (id `facet`) — first-pass scoring, dedupe, cleanup, diagnostics

- `facet:quality_pass` — per-image quality-like scoring. Output
  `.../facet/quality_pass/quality_pass.json` with per-image `path, file_size,
  width, height, quality, technical_sharpness, eyes_sharpness, exposure,
  color_balance, dynamic_range, noise_estimate, quality_band
  (excellent|good|usable|weak|reject), headshot_candidate (quality>=68),
  source: facet_native`.
- `facet:composition_pass` — heuristic composition + exposure/quality proxies.
  Output `.../facet/composition_pass/composition_pass.json` with
  `composition_score, center_bias, entropy, dynamic_range, noise`.
- `facet:faces_pass` — pixel-based skin-region face proxy. Output
  `.../facet/faces_pass/faces_pass.json` with `face_count, face_confidence,
  face_region, eye_open, region_score`.
- `facet:duplicate_pass` — exact duplicate detection by hash with grouped map.
  Output `.../facet/duplicate_pass/duplicate_pass.json` with `feature, method,
  count, policy, groups, total_members_in_duplicate_groups, coverage_percent`
  and per-group `group_key, member_count, member_files, avg_similarity,
  min/max_similarity, avg_quality, total_file_size, representative, signature,
  matches_threshold`. Policy: `min_group_size=2`, `min_avg_hash_similarity=98.0`.
- `facet:burst_blink_pass` — burst + similarity culling ordered by capture time
  (file mtime, not EXIF) using pixel-embedding cosine similarity + eye-open proxy.
  Output `.../facet/burst_blink_pass/burst_blink_pass.json` with `count,
  mean_embedding_similarity, policy, blocks`; per-block `burst_key, count,
  blink_frames, blink_ratio, mean_embedding_similarity, recommended_keep, items,
  burst_contains_blink`. Policy: `min_burst_size=2`, `blink_closed_threshold=35.0`.
- `facet:diagnostics_pass` — runtime diagnostics for traceability. Output
  `.../facet/diagnostics_pass/diagnostics_pass.json` with run root + per-image
  `sha256, ahash, dhash, phash, capture_unix_ms, quality`.

### python-ofiq (id `python-ofiq`) — deterministic quality scoring

- `python-ofiq:scalar_quality` — single 0-100 score per image. Output
  `.../python-ofiq/scalar_quality/scalar_quality.json` with per-image `path,
  scalar_quality, quality_band, dimension_sum, dimension_count, face_count,
  face_confidence, face_eye_open, face_region, source`. Deterministic per input.
- `python-ofiq:vector_quality` — per-image quality dimension vector. Output
  `.../python-ofiq/vector_quality/vector_quality.json` with `schema
  (version 0.2-native, dimension_count, dimensions)` and per-image
  `scalar_quality, quality_band, quality_vector, dimensions,
  quality_gap_vs_dimension_mean, quality_band_summary, metadata, vector_cells`.
- `python-ofiq:setup_data` — live capability probe (in-memory decode +
  output-dir writability), advertises active dimensions, status `ready` or
  `degraded`. Output `.../python-ofiq/setup_data/setup_data.json` with `status,
  engine, version, dimensions, dimension_count, score_0_100, thresholds
  (scalar_quality_headshot_min 68.0, vector_quality_gap_tolerance 25.0,
  quality_score_range)`.

### deepface (id `deepface`) — identity / face-region proxies

- `deepface:detect` — skin-color heuristic proxy (NOT a real detector; warm
  region => face_count 1). Output `.../deepface/detect/detect.json` with `path,
  faces_detected, face_confidence, detection_score, passed, quality_band,
  face_quality, regions`. Valid when `detection_score >= 25.0`.
- `deepface:analyze` — measured pixel statistics (mean luma, std, contrast,
  exposure, noise, skin ratio, eye-open proxy, face confidence). Output
  `.../deepface/analyze/analyze.json` with `age, age_confidence, dominant_gender,
  dominant_emotion, dominant_emotion_score, emotion_scores, face_quality,
  face_count, face_confidence, region, quality_band, eye_open`. Age proxy 16-75,
  gender model `heuristic_proxy`. Does NOT truly predict age/gender/emotion.
- `deepface:represent` — deterministic face embeddings. Output
  `.../deepface/represent/represent.json` with `id (sha256), path, embedding_dim,
  embedding_sum, embedding_norm, embedding_unit (head, max_component,
  min_component)`.
- `deepface:verify` — pairwise verification across selected images. Output
  `.../deepface/verify/verify.json` with `count, pairs, threshold 0.86,
  soft_threshold 0.76, verified_pairs` and per-pair `a, b, similarity,
  similarity_percent, distance, verified, decision, decision_confidence`.
- `deepface:find` — nearest-candidate retrieval per image. Output
  `.../deepface/find/find.json` with `count, top_k 5, accepted_queries,
  threshold` and per-query `query, query_quality, query_face_count, candidates,
  candidates_found, best_similarity, top_gap, decision`.
- `deepface:register` — index builder for the image set. Output
  `.../deepface/register/register.json` with `index [{id, path, quality_score,
  face_count, face_confidence}], index_size, index_quality.avg_quality`.

### imagededup (id `imagededup`) — dedupe grouping + removal candidates

- `imagededup:hash_duplicates` — hash-style duplicate grouping. Output
  `.../imagededup/hash_duplicates/hash_duplicates.json` with `count,
  duplicates_found, total_candidate_pairs, coverage_percent, groups` and
  per-group `group_key, type, paths, count, type_size_total, best_keep, method`.
- `imagededup:cnn_duplicates` — perceptual-hash hybrid (ahash+dhash+phash); NO
  neural inference despite the name. Output
  `.../imagededup/cnn_duplicates/cnn_duplicates.json` with `count, pairs,
  threshold 75.0, pairs_selected, pairs_considered, method` and per-pair `a, b,
  similarity, a_quality, b_quality, method, units`.
- `imagededup:remove_candidates` — conservative remove list from dedupe/similarity
  evidence. Output `.../imagededup/remove_candidates/remove_candidates.json` with
  `count, remove_list, pairs_threshold 78.0, policy, images_scanned` and
  per-action `path, action, keep, component_id, similarity_to_keep, decision
  (keep_score, remove_score, score_delta, reason)`.

### ediffiqa (id `ediffiqa`, spec name eDifFIQA) — quality profile variants

- `ediffiqa:model_t` / `model_m` / `model_s` / `model_l` — tiny / medium /
  standard / large deterministic scoring variants. Output
  `.../ediffiqa/<model_id>/<model_id>.json` with `feature, count, model,
  model_profile, items` and per-image `path, model, score, score_components,
  dims, pass_quality, face_count, face_confidence, eye_open, quality_delta,
  best_model_for_image, status`.
- `ediffiqa:batch_inference` — runs all variants, writes a consolidated matrix.
  Output `.../ediffiqa/batch_inference/batch_inference.json` with `matrix` rows
  (per-model scores + winning model), `model_summary, winner_counts,
  winner_score_stats`.

### GUI tab grouping of features

The GUI groups feature checkboxes by tab: `deepface:*` -> Identity;
`imagededup:*` and `facet:duplicate_pass`/`facet:burst_blink_pass` ->
Duplicates; `facet:diagnostics_pass` -> Run & Debug; all other `facet:*`,
all `python-ofiq:*`, all `ediffiqa:*` -> Quality & IQ. Any unknown prefix falls
back to Run & Debug so nothing is hidden.

</topic>

<topic id="tools-overview" summary="The three tool surfaces">

## Tools Overview

Three tool surfaces plus one dedicated compare surface let a model or operator drive and
inspect the app:

1. **Visual debug tool** — the GUI "Run & Debug" tab: live event stream,
   last-applied model action, last receipt, the full `AppStateSnapshot`, and
   artifact links. See `visual-debug-tool`.
2. **Swarm backend command API** — a file-based command + receipt protocol plus a
   headless CLI. Backend commands run fully headless; ui-intents are applied by a
   live GUI frame. See `swarm-command-api` and `headless-cli`.
3. **Frontend driving via the debug tool** — a model issues ui-intent commands
   that the running GUI polls and applies one per frame, then watches the result
   in the Run & Debug tab. See `frontend-driving`.
4. **Picture Compare** — open the **Compare** tab to compare multiple folders
   simultaneously, with independent per-lane navigation.

</topic>

<topic id="visual-debug-tool" summary="The Run and Debug GUI tab">

## Visual Debug Tool (Run & Debug tab)

The "Run & Debug" tab is the built-in visual debugger. It launches no external
windows ("No external windows are launched from here."). It shows:

- **Selected features** count and list.
- Buttons: "Refresh plugins" (reload manifests, clear selection) and
  "Run selected features" (start the pipeline on the current selection +
  imported/worktree images).
- **Run output** — path to the latest `results.json`.
- **Run summary** — per-plugin `plugin::feature -> status (message)` lines plus
  each artifact path, run_id, status, output path, feature list, image count.
- **facet diagnostics_pass** feature checkbox (the Run & Debug tab's own feature).
- **Visual debugger / model-drivable control surface**: "Last applied model
  action" and a collapsible "Last receipt" (the JSON of the most recent applied
  ui-intent).
- **Events** — the chronological event stream from the runtime bus, newest
  context preserved (`[ts] LEVEL source - message`), the same records appended to
  `<workspace_root>/.facial/data/events.jsonl`.
- **AppStateSnapshot** (collapsible) — the full live state JSON (see schema in
  `swarm-command-api`): active_tab, project_name, worktree_path, in_place,
  selected_features, running_pipeline, run_output, plus models, plugins (with
  nested features), worktrees, `repo_root`, `workspace_root`, `api_root`, and
  `worktrees_root`.
- **Artifact links** (collapsible) — the `output=` and `artifact:` lines from the
  last run summary.

A no-context model should read AppStateSnapshot to learn the current GUI state,
issue ui-intents, then re-read the snapshot and the event stream to confirm.

</topic>

<topic id="picture-compare" summary="Lane-based image comparison with in-app folder browser, keyboard navigation, and optional lane sync">

## Picture Compare

Use the **Compare** tab for visual side-by-side review. Each lane is one card that
contains its own folder controls, a large image viewport, and a navigation footer.

1. Click **+ Lane** / **- Lane** in the toolbar to set the number of lanes (1-16).
2. Pick a folder per lane: click **Browse…** to open the in-app folder browser
   (drive buttons, **⬆ Up**, click a folder to enter it, **Use this folder** to
   confirm) — no external window is ever opened — or paste a path into the box
   and press Enter (or click **⟳**) to scan. **Include subfolders** rescans on toggle.
3. Navigate with **◀ Prev** / **Next ▶**, the **go to** box (type an image number,
   press Enter), the mouse wheel over an image, or the **arrow keys** (the keys act
   on the lane under the pointer; with a single lane no pointer is needed).
4. **Sync lanes** (toolbar toggle, off by default) links navigation: Prev/Next,
   arrow keys, and wheel then move every lane together by the same step — useful
   for A/B-comparing two exports of the same shoot. With Sync off, lanes stay
   fully independent: one lane's moves never affect another.
4b. **Anchors** (toolbar toggle) pins the identity reference set as a thumbnail
   strip above the lanes, so ground truth stays in view while judging candidates.
   Enabled when `identity_reference_dir` / `FACIAL_IDENTITY_REF_DIR` is set
   (hover the disabled toggle for the hint); thumbnails decode off-thread,
   capped at 24 anchors, hover a thumb for its filename.
5. The footer shows `current / total`, plus the current filename (hover it for the
   full path). The status line at the top of each card shows scan/decode state.

To keep memory use predictable on very large folders, compare scans only file paths up
front and decodes only the currently shown image per lane; the previous image stays
visible while the next one decodes. Prev/Next are grayed out until a folder is scanned.

</topic>

<topic id="swarm-command-api" summary="Complete file-based command and receipt reference" ingestable="true">

## Swarm Backend Command API (file-based command + receipt protocol)

There is no socket and no window interaction. Backend models drive the app by
dropping command files into `<api_root>/commands/` and reading the resulting
receipts, or by calling the headless CLI (`headless-cli`). The GUI applies
ui-intents from `<api_root>/intents/` on its own frames.

### On-disk directory layout

`<api_root>` defaults to `<workspace_root>/.facial/data/api` (override with
`FACIAL_DATA_ROOT`).
`ApiPaths::ensure_dirs()` creates:

```
<api_root>/
  commands/            # producers drop <action_id>.json here (input queue)
  processing/          # a command is atomically renamed here while running
  receipts/            # <action_id>.json terminal receipt written here (output)
  intents/             # ui-intents persisted here, awaiting live GUI apply
  intents/applied/     # applied ui-intent receipts archived here (audit)
  dead/                # unparseable/quarantined commands moved here
  state/state.json     # latest AppStateSnapshot (written by get_state/capture)
  stop                 # sentinel file: create it to stop `run-queue --watch`
```

Queue rules: files mid-write must use a `.tmp` suffix (the queue skips `*.tmp`
and any non-`.json`). A command is claimed by atomic rename into `processing/`,
dispatched, its receipt written to `receipts/<action_id>.json`, then the
processing file is removed. Idempotent: if `receipts/<id>.json` already exists the
command is dropped without reprocessing. On startup, `recover_processing` moves
any orphaned `processing/<id>.json` (no matching receipt) back to `commands/`.

### Command envelope

`protocol_version` is currently `1`. The variant is selected by a flat `kind`
discriminator flattened into the object alongside its fields.

```json
{
  "action_id": "11111111-1111-1111-1111-111111111111",
  "protocol_version": 1,
  "actor": "swarm-model-7",
  "issued_at": "2026-06-08T12:00:00Z",
  "kind": "start_run",
  "project_name": "demo",
  "image_paths": ["C:/photos/a.jpg", "C:/photos"],
  "feature_keys": ["facet:quality_pass", "deepface:detect"],
  "worktree_path": null,
  "in_place": false
}
```

Fields: `action_id` (required join key across command/receipt/intent/events; if
blank, a uuid is generated), `protocol_version` (defaults to 1), `actor`
(optional attribution), `issued_at` (optional rfc3339), and the flattened
`kind` + variant fields.

### Receipt schema (always written, never panics)

```json
{
  "action_id": "11111111-1111-1111-1111-111111111111",
  "kind": "start_run",
  "status": "ok",
  "actor": "swarm-model-7",
  "protocol_version": 1,
  "started_at": "2026-06-08T12:00:00.100Z",
  "finished_at": "2026-06-08T12:00:01.250Z",
  "result": { "run_id": "...", "status": "completed", "...": "..." },
  "error": null,
  "note": null
}
```

`status` is one of: `ok` (backend command completed), `error` (backend command
failed), `accepted` (ui-intent validated + persisted, awaiting GUI apply),
`applied` (ui-intent applied by a live GUI frame), `rejected` (refused: bad
vocab, path escape, run already active, unparseable). `result` is omitted when
null; `error` and `note` are omitted when absent. Every receipt is also mirrored
to `events.jsonl` (source attribution `api`); `ok`/`accepted`/`applied` map to
`applied=true`.

### Backend-executable commands (terminal receipt, run fully headless)

- `list_features` — `result` = array of plugin manifests (each with nested
  `features`). Status `ok`.
- `list_models` — `result` = array of model records. Status `ok`.
- `list_worktrees` — `result` = object `{ "<project>": ["<run dir>", ...] }`.
  Status `ok`.
- `get_state` — `result` = full `AppStateSnapshot` (also persisted to
  `state/state.json`). Status `ok`.
- `set_workspace_root` — field `path`. Creates/persists the runtime root used
  for `.facial/data`, `.facial/worktrees`, API queues, receipts, and debug
  events.
- `start_run` — fields `project_name` (req), `image_paths`, `feature_keys` (req,
  non-empty or `error`), `worktree_path` (optional; created if null/blank/"no
  worktree yet"), `in_place`. `result` = `RunSummary`. Status `ok` on success,
  `error` on failure (e.g. "no features selected", "No images available").
- `get_run_status` — field `run_id`. `result` = `{ "status":
  "completed"|"unknown", "found": bool }`. Status `ok`.
- `get_run_summary` — field `run_id`. `result` = the parsed `results.json`.
  Status `ok`, or `error` if the run is not found / unreadable.
- `list_artifacts` — field `run_id`. `result` = sorted array of every file path
  under the run dir. Status `ok`, `error` if not found, `rejected` if the run dir
  escapes the allowed artifact roots (`worktrees_root`, `api_root`, copy
  output root, or current in-place run roots).
- `read_artifact` — field `path`. Path is canonicalized and must live under
  an allowed artifact root or it is `rejected`. `result` = parsed JSON if
  parseable, else the raw string. Status `ok`/`error`/`rejected`.

### UI-intent commands (persisted to intents/; applied by a live GUI)

These return `accepted` from the backend (persisted to `intents/<id>.json`), then
`applied` or `rejected` when a live GUI frame consumes them. They require a
running GUI to take effect.

- `set_project` — field `project_name`. Sets the GUI project name.
- `set_worktree` — field `worktree_path`. Selects an existing worktree.
- `select_tab` — field `tab`, vocab one of
  `project | quality_iq | identity | duplicates | run_debug | manual | compare |
  options`. Unknown vocab is `rejected` at validation time.
- `set_features` — field `feature_keys`. Unknown keys are dropped (noted).
- `set_in_place` — field `in_place` (bool).
- `import_paths` — fields `project_name`, `paths`, `in_place`. Ingests into the
  live GUI worktree (copy or in-place).
- `start_run_ui` — asks the live GUI to press "Run selected features". `rejected`
  if a run is already active or no features are selected.

### AppStateSnapshot schema (result of get_state)

```json
{
  "protocol_version": 1,
  "captured_at": "2026-06-08T12:00:00Z",
  "repo_root": "D:/Projects/LLM projects/facial",
  "workspace_root": "D:/Projects/other-project",
  "worktrees_root": "D:/Projects/other-project/.facial/worktrees",
  "api_root": "D:/Projects/other-project/.facial/data/api",
  "ingest_in_place_default": false,
  "models": [ /* ModelRecord */ ],
  "plugins": [ /* PluginManifest with nested features */ ],
  "worktrees": { "<project>": ["<run dir>", "..."] },
  "active_tab": "manual",
  "project_name": "default-project",
  "worktree_path": "no worktree yet",
  "in_place": false,
  "selected_features": ["facet:quality_pass"],
  "running_pipeline": false,
  "run_output": "no run yet"
}
```

In a headless `get_state` the live-GUI fields (`active_tab`, `project_name`,
`worktree_path`, `selected_features`, `running_pipeline`, `run_output`) are
defaults; only a running GUI populates them with real session state.

</topic>

<topic id="headless-cli" summary="Headless CLI subcommands and examples">

## Headless CLI

Any first argument other than `gui` puts the app in headless CLI mode; the egui
GUI is never launched. Exit codes: `0` = ok/accepted/applied;
`1` = error/rejected/parse failure.

```
facial gui                              launch GUI (default with no args)
facial run-queue [--once | --watch [--poll-ms N]]
                                        drain commands/ (default --once;
                                        --watch loops until <api_root>/stop)
facial command <path>                   parse + dispatch a command file, print receipt JSON
facial command --json '<json>'          parse + dispatch an inline JSON command
facial <kind> [--flags...]              convenience builder for a single command
```

Convenience kinds and flags:

```
list_features | list_models | list_worktrees | get_state
start_run --project NAME [--feature plugin:feat ...] [--image PATH ...] [--worktree PATH] [--in-place]
get_run_status --run-id ID | get_run_summary --run-id ID | list_artifacts --run-id ID
read_artifact --path PATH
set_workspace_root --path DIR | set_copy_location --path DIR
sort_run --run-id ID [--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]
identity_status | identity_gate --image PATH | identity_gate_dir --dir DIR
identity_dedup --dir DIR [--threshold 0.90]
render_eval --dir DIR
calibrate_threshold
anchor_montage --image PATH
review_init --dir DIR [--shards N] [--gate-manifest PATH] [--clusters PATH]
review_claim --session S [--shard K] [--actor A] [--steal]
review_decide --session S --id ID --decision accept|reject|hold [--reason TEXT] [--actor A]
review_status --session S
review_montage --session S [--shard K] [--page N] [--face-crop] [--filter k=v ...]
review_export --session S --out DIR --name NAME [--repeats N] [--allow-partial]
set_project --project NAME | set_worktree --worktree PATH | select_tab --tab VOCAB
set_features [--feature plugin:feat ...] | set_in_place [--in-place]
import_paths --project NAME [--image PATH ...] [--in-place] | start_run_ui
```

Review queue (WP-016): `review_init` walks a folder (recursive), assigns every image a
stable content ID (sha256; the 16-char short form works everywhere), splits the set
into N shards, and writes a session under `<copy_location>/review/<session_id>/` (or
`<workspace_root>/.facial/review/` when no copy location is set). Optional joins at
init: `--gate-manifest` attaches each image's identity-gate row (verdict, framing,
face box, sharpness, yaw, hair flag) as filterable metadata; `--clusters` attaches
near-dup cluster ids from `identity_dedup`. Parallel agents each `review_claim` a
shard (atomic; `--steal` takes over a dead agent's claim and is ledgered), work
through the returned worklist with `review_decide`, and anyone can read
`review_status` for funnel counts (candidates/accepted/rejected/hold/undecided),
per-shard/per-actor/per-cluster progress, live claims, and surfaced decision
conflicts. All state is an append-only `ledger.jsonl` + derived views — nothing is
tracked in chat.

`review_montage` renders paged contact sheets (6x5 tiles, 256px) with a
`.map.json` keyed by image ID — never positional inference; near-dup clusters tile
together. `--face-crop` crops tiles to the joined gate face box (+30% margin, flagged
per tile, full image fallback). `--filter` terms compose: `decision=undecided`,
`framing=close-up`, `hair_color=pink_purple`, `yaw_estimate=profile`,
`face_crop_sharpness_min=50`, `cluster=c0000`, `shard=1`.

`review_export --format kohya` (the default and only format) verifies every accepted
image's sha256, copies into `<out>/<repeats>_<name>/`, and writes
`dataset_manifest.json` with the full lineage funnel (source -> candidates -> decided
-> exported), per-file hashes, and explicit problems (changed/missing files are
reported, never copied). Undecided images block the export unless `--allow-partial`.

Identity tooling (WP-017/WP-018): `identity_dedup` groups a folder by ArcFace cosine
(greedy clustering, deterministic; emits groups + a recommended keeper, deletes
nothing; 20k-image cap). `render_eval` scores a folder of renders against the anchor
set grouped by config key (immediate subfolder name, else filename stem with the
trailing index stripped) and emits per-group mean/min/max — `no_face`/`error` rows
are counted separately and NEVER enter the statistics. `calibrate_threshold` reports
anchor pairwise self-consistency + negative-set distribution and recommends a gate
threshold (refuses below 4 anchors; report-only, never applied). `anchor_montage`
renders candidate-vs-anchors as one grid with per-anchor cosine similarity in the
tile map.

Common flags: `--action-id ID` (join key; uuid auto-generated when omitted),
`--actor ID` (attribution, e.g. swarm model id). `--feature`/`--features` and
`--image`/`--images` are repeatable.

Examples:

- `facial list_features` — print every plugin + feature as a receipt.
- `facial get_state` — print and persist the AppStateSnapshot, including active
  `workspace_root`, `api_root`, and `worktrees_root`.
- `facial set_workspace_root --path C:/my-project` — select another project's
  runtime root before queue or pipeline work.
- `facial start_run --project demo --image C:/photos --feature facet:quality_pass --feature deepface:detect`
- `facial get_run_summary --run-id 20260608_120000_ab12cd34`
- `facial list_artifacts --run-id 20260608_120000_ab12cd34`
- `facial read_artifact --path <worktree>/runs/<run_id>/results.json`
- `facial command --json '{"action_id":"","kind":"list_models"}'` (blank
  action_id auto-fills a uuid).
- Drain a producer-fed queue once: `facial run-queue --once` (prints one receipt
  JSON line per processed command).
- Long-running drainer: `facial run-queue --watch --poll-ms 250` (loops until you
  create the file `<api_root>/stop`).

</topic>

<topic id="frontend-driving" summary="How a model drives the GUI via intents and the debug tool">

## Driving the Frontend Through the Debug Tool

A model controls the live GUI without touching the screen, mouse, or keyboard:

1. Issue a ui-intent — either `facial <intent> ...`, `facial command --json
   '...'`, or drop `<api_root>/commands/<id>.json` and run `facial run-queue
   --once`. The backend validates it and writes it to `intents/<id>.json`,
   returning an `accepted` receipt.
2. The running GUI polls `intents/` every ~250 ms and applies at most ONE intent
   per frame (FIFO by modified time). It then writes a follow-up receipt
   (`applied` or `rejected`) to `receipts/<id>.json`, archives the intent to
   `intents/applied/<id>.json`, and records a model-action event.
3. Observe the result: read `receipts/<id>.json`, re-read `get_state` (or the
   AppStateSnapshot panel in the Run & Debug tab), and read the event stream. The
   Run & Debug tab also shows "Last applied model action" and "Last receipt".

Typical drive sequence to run features through the GUI:

1. `select_tab --tab quality_iq` (or whichever tab holds the features).
2. `select_tab --tab compare` for manual review and loading independent folders per
   lane (optional).
3. `set_project --project demo`.
4. `import_paths --project demo --image C:/photos` (copy) or add `--in-place`.
5. `set_features --feature facet:quality_pass --feature python-ofiq:scalar_quality`.
6. `start_run_ui` — the GUI presses "Run selected features".
7. Poll `get_state` until `running_pipeline` is false, then read
   `get_run_summary` / `list_artifacts`.

All ui-intents require a running GUI to reach `applied`; until then they sit in
`intents/` as `accepted`. For fully headless runs, prefer the backend `start_run`
command instead of the `start_run_ui` intent.

</topic>

<topic id="options" summary="The Options tab: theme, font size, and configuration readout">

## Options & UI Preferences

The GUI **Options** tab holds interface preferences:

- **Theme** — `Paper (light)` or `Ink (dark)`. Both share the same flat layout and
  the vermilion accent; Paper is a warm paper desk with denim-ink text, Ink is a
  dark slate desk with paper-toned text. Switching applies instantly and writes
  `theme_mode` ("paper" | "ink") into `product/config/default.json`. The
  `FACIAL_THEME` environment variable overrides it at startup (useful for
  capturing `ui-inspect` snapshots of either mode headlessly).
- **Interface font size** — a slider (12–40 pt) that rescales all UI text live as
  you drag. The UI typeface is Inter (headings use the SemiBold face at ~1.35x);
  monospace is reserved for logs/code. Default is **19 pt**.
- **Reset to default (19 pt)** — restores the shipped size.
- **Current configuration** readout — the settings-file path, current font size,
  `max_debug_events`, the in-place default, and the worktrees root.

Persistence: theme and font-size changes write to `product/config/default.json`,
so both survive restarts. Window size/position is remembered separately by the
windowing layer in the user profile's app-data store. `FACIAL_FONT_SIZE`
(clamped 10–48 pt) and `FACIAL_THEME` take effect at startup.

A slim status bar at the bottom of every tab shows the active workspace root,
whether the copy/output folder is set, and the last run state ("working…" in
accent color while a scan, decode, or pipeline is in flight).

There is no headless command for font size or theme — they are display
preferences and do not affect runs, artifacts, or the command/receipt protocol.

</topic>

<topic id="outputs" summary="Where artifacts, summaries, state and events live">

## Output / Artifact Paths

- Worktree: `worktrees/<slug>/<timestamp_id>/` (internal workspace surface)
- Imported images in copy mode: `<copy/output folder>/images/`
- In-place images: original source paths, unchanged.
- Copy-mode run root: `<copy/output folder>/runs/<run_id>/`
- In-place run root: `<source parent>/.facial/runs/<run_id>/`
- Per-feature artifact:
  `<run root>/<plugin_id>/<feature_id>/<feature_id>.json`
- Per-run summary: `<run root>/results.json` (this path is the
  `output_path`/`run_output` shown in the GUI and returned by `start_run`).
- Latest captured state: `<api_root>/state/state.json`.
- Receipts: `<api_root>/receipts/<action_id>.json`.
- Event log (append-only): `<workspace_root>/.facial/data/events.jsonl`.

`repo_root` is the app install root. `workspace_root` is the selectable runtime
root for the external project being processed. `worktrees_root` defaults to
`<workspace_root>/.facial/worktrees`; `<api_root>` and the data root default
under `<workspace_root>/.facial/data`. The copy/output folder is set by the GUI,
`facial set_copy_location --path DIR`, config `copy_location`, or
`FACIAL_COPY_LOCATION`. Internal roots are relocatable via `FACIAL_WORKSPACE_ROOT`,
`FACIAL_DATA_ROOT`, `FACIAL_WORKTREES_ROOT`, and `FACIAL_REPO_ROOT`.

</topic>

<topic id="errors-events" summary="Where errors and event traces appear">

## Where Errors and Events Appear

- **Event stream**: the Run & Debug tab "Events" panel, mirrored to
  `<workspace_root>/.facial/data/events.jsonl` (`[ts] LEVEL source - message`). Levels include
  INFO, WARN, ERROR. Sources include Service, Pipeline, Ingest, ModelRegistry,
  plugin_host, and `api` (command receipts).
- **Run summary**: failed features appear immediately as
  `plugin::feature -> failed (message)` lines in the Run & Debug "Run summary"
  panel and inside `results.json` (`totals` counts ok/skipped/failed; run
  `status` is `completed` when nothing failed, else `partial`).
- **Receipts**: backend command failures are `error`; refusals are `rejected`,
  each carrying an `error` and/or `note` string explaining why.
- **Quarantine**: unparseable commands are moved to `<api_root>/dead/<id>.json`
  with a paired `rejected` receipt (kind `unparseable`).

</topic>

<topic id="recovery-rerun" summary="Failure recovery and rerun">

## Failure Recovery and Rerun

- **Orphaned in-flight command**: a file left in `processing/` with no matching
  receipt is automatically moved back to `commands/` at startup
  (`recover_processing`), so it is retried on the next queue drain.
- **Quarantined command**: inspect `<api_root>/dead/<id>.json`, fix the JSON, and
  re-drop it into `commands/` with a fresh `action_id` (the same id is treated as
  already-processed and dropped).
- **Invalid feature key** (`plugin:feature` malformed or unknown): the pipeline
  emits a failure event, records the reason in the run summary, and continues the
  remaining features where possible.
- **No valid images**: the run is refused with `No images available`; correct the
  paths/extensions and rerun.
- **Pipeline failure**: the current run stops, partial outputs are preserved under
  the run dir, and an explicit rerun is required after correction.
- **Run already active**: a new `start_run_ui` intent is `rejected`; wait for
  `running_pipeline` to clear (poll `get_state`) then retry.
- **Missing plugin manifest**: the result surfaces a missing-plugin failure that
  includes the `plugins_root` and the list of loaded plugin ids for diagnosis.
- **Reruns are idempotent at the queue level**: reusing an `action_id` that
  already has a receipt is dropped without reprocessing; use a new id to force a
  rerun.

</topic>

<topic id="sort" summary="The copy-location gate and deterministic sort-into-folders action">

## Sort & Copy-Location Gate

### Copy/output location (required gate)
No run, sort, or task may start until a **copy/output location** is set. Set it in
the GUI **Project** tab ("Copy / output location" + Set), via
`facial set_copy_location --path DIR`, or via config `copy_location` /
`FACIAL_COPY_LOCATION`. Use `facial get_state` to verify the active `api_root`
and `workspace_root` if a model is operating from another project. Until set,
the Run button is disabled and both `run_pipeline` and `sort_run` refuse with
"Set a copy/output location before starting any task".

### Sort run into folders
`sort_run` deterministically sorts a completed run's images into **keep /
review / cull** from the run's on-disk verdicts (copy-only, non-destructive):

- **cull**: in `imagededup:remove_candidates` remove list, `keep: false`, a blink
  frame, or `quality_band: reject`.
- **review**: `quality_band: weak` (and not already cull).
- **keep**: everything else in the run's per-image universe.

Modes: default copies into `<copy location>/keep | review | cull`. With
**work-in-parent** on, you supply an explicit folder per bucket instead.

GUI: Run & Debug tab -> "Sort run into folders" (run id blank = latest run).
Headless: `facial sort_run --run-id ID [--in-parent --keep-dir DIR --review-dir DIR --cull-dir DIR]`.
Result JSON: `run_id, mode, total, keep, review, cull, keep_dir, review_dir,
cull_dir, errors`.

</topic>

<topic id="identity" summary="Deterministic face-identity engine (real YuNet + ArcFace)">

## Identity

Optional, pure-Rust ONNX face identity (Phase 2). DISABLED unless an embedder
model is provisioned; when disabled the app reports `identity: unavailable` and
never fakes a verdict. Provision via:

- `product/config/default.json` keys:
  - `identity_model_path`
  - `identity_detector_path`
  - optional `identity_reference_dir` / `identity_negative_dir`
  - optional `identity_threshold` / `identity_margin`
- environment variables at launch:
  - `FACIAL_IDENTITY_MODEL`
  - `FACIAL_IDENTITY_DETECTOR`
  - `FACIAL_IDENTITY_REF_DIR`
  - `FACIAL_IDENTITY_NEG_DIR`
  - `FACIAL_IDENTITY_THRESHOLD`
  - `FACIAL_IDENTITY_MARGIN`

Detector (WP-020): **YuNet ships built into the binary** (OpenCV Zoo 2023mar float
model, MIT — license at `product/assets/models/YuNet-LICENSE.txt`), so face detection
needs zero provisioning. Resolution order: `identity_detector_path` /
`FACIAL_IDENTITY_DETECTOR` override -> bundled YuNet -> none (resize alignment). A
configured path that fails to load falls back to the bundled model with origin
`bundled_fallback`. Every load runs a startup self-check (blank-frame inference must
expose the 12-output 2023mar layout) so a wrong export can never silently produce
wrong geometry. `identity_status` reports `detector_origin`
(`override|bundled|bundled_fallback|none`) and `detector_sha256`, and the model
registry carries a `yunet-detector` record with the same provenance. Only the ArcFace
**embedder** (`identity_model_path` / `FACIAL_IDENTITY_MODEL`, ~166 MB) still needs
provisioning — without it the whole identity engine stays disabled.

Example (PowerShell):

```powershell
# Configure both identity dependencies for a no-context run.
$env:FACIAL_IDENTITY_MODEL = "D:/Projects/LLM projects/facial/product/models/w600k_r50.onnx"
$env:FACIAL_IDENTITY_DETECTOR = "D:/Projects/LLM projects/facial/product/models/yunet_2023mar.onnx"
facial identity_status
```

Equivalent config-file mode in `product/config/default.json`:

```json
{
  "identity_model_path": "D:/Projects/LLM projects/facial/product/models/w600k_r50.onnx",
  "identity_detector_path": "D:/Projects/LLM projects/facial/product/models/yunet_2023mar.onnx"
}
```

Alignment: provision a **YuNet** detector ONNX via `identity_detector_path` or
`FACIAL_IDENTITY_DETECTOR` (`face_detection_yunet_2023mar.onnx`, pure-Rust
tract-compatible). When present, faces are detected and aligned via a 5-point
similarity transform to the canonical ArcFace template (`align="yunet_112"`); with
no detector (or if the detector misses a face), it falls back to a whole-image resize
(`align="resize_112"`). The per-image `align` field reports which path was used.

Method (deterministic): detect+align (or resize) -> 112x112 -> embed via `tract`
-> L2-normalize -> cosine vs the reference and negative sets. Verdict =
`match` / `no_match` / `unsure` / `no_reference`, plus `no_face` (no face detected,
align fell back to resize) and `error` (image failed to decode/infer). Similarities,
margin, and the model sha256 are stamped into the result for audit. YuNet alignment
materially sharpens separation (validated: same-person cosine ~0.70+, different ~0.0,
vs a fuzzy 0.24-0.56 spread under resize). The proxy `deepface:*` features are
unchanged and remain labelled as proxies.

Face geometry (same YuNet pass, no external `cv2`): every gate row also carries
`face_count` (faces at/above `identity_count_threshold`, default 0.9, after IoU NMS
0.3 — use it to reject collages / group shots), `face_box` (`{x,y,w,h}` original px
of the strongest face), `face_frac` (face area / image area — scale-bucket hint),
`face_score`, and `framing` (`close-up` | `three-quarter` | `full-body` | `none`,
derived from `face_frac` via `framing_closeup_min`/`framing_threequarter_min`). This
lets a model do scale-bucketing, framing-bucketing, and collage-rejection in one tool.

Trust: every face/quality output carries `source` = `real` (the YuNet+ArcFace engine:
the identity gate and deepface represent/verify/find when a model is provisioned) or
`proxy` (heuristic plugins: deepface detect/analyze, facet, ofiq, ediffiqa, imagededup).
Trust-weight `proxy` outputs accordingly — they look authoritative but are heuristics.

Curation metadata (WP-019, every gate row + CSV): `face_crop_sharpness` (laplacian
variance over the face box — face-region focus, not whole-frame), `yaw_estimate`
(`frontal|quarter|profile` bucket from the 5-point landmark geometry, with
`yaw_ratio`; buckets only — 5 points cannot give degrees), and `hair_color`
(+`hair_confidence`, `hair_source: "proxy"` — an HSV heuristic over the strip above
the face box; a triage hint for wig/dye outliers, never a gate). These columns join
into review sessions via `review_init --gate-manifest` and drive `--filter` terms.

Eyes-open (WP-021, wave 2 — `source: real`): provision the PIPNet 98-pt landmark
model (`pipnet_r18_wflw_98.onnx`, ~47 MB, MIT — license beside it in
`product/models/`) via `landmark_model_path` in the config, `FACIAL_LANDMARK_MODEL`,
or simply by placing it at `product/models/pipnet_r18_wflw_98.onnx` (auto-detected).
It lazy-loads on the first gate call. Every gate row then adds `eyes_open`
(`open|closed`), raw `ear_left`/`ear_right` (simplified WFLW EAR: mid-lid vertical /
eye width; open eyes measure ~0.32-0.42, the bucket threshold `ear_open_min` 0.15 and
`ear_method` are stamped per row so you can re-bucket downstream), and
`landmark_conf_min`. Filter reviews with e.g. `--filter eyes_open=closed`. Without
the model the fields are null and everything else works unchanged.

Occlusion: deliberately NOT emitted. The landmark-confidence occlusion proxy failed
its validation gate (painted eye/mouth occlusions produced no confidence separation,
while EAR separated cleanly), so per the WP-021 contract the flag is withheld rather
than shipped as a misleading signal. Honest occlusion detection needs a face-parsing
segmentation model — a future packet if field feedback demands it.

Commands:
- `facial identity_status` — availability + provenance (incl. `detector_origin`).
- `facial identity_gate --image PATH` — one image, returns the row JSON.
- `facial identity_gate_dir --dir DIR` — gate every top-level image in `DIR` in one
  call. Writes `runs/<run_id>/identity_gate.csv` + `manifest.json` (schema_version 2)
  under the copy-root (else `<DIR>/.facial/runs/<run_id>/`); the receipt returns
  `run_id`, the artifact paths, and a per-verdict `summary`. Per-image errors are
  isolated to their row (the batch never aborts); output is deterministic (sorted
  inputs, stable NMS tiebreak). Tune `identity_count_threshold` /
  `FACIAL_IDENTITY_COUNT_THRESHOLD` to change the face-count cutoff.
- `facial identity_dedup --dir DIR [--threshold 0.90]` — ArcFace-cosine near-dup
  groups + recommended keeper per group (see the command API topic).
- `facial render_eval --dir DIR` / `facial calibrate_threshold` /
  `facial anchor_montage --image PATH` — train->eval loop tools (command API topic).

</topic>

<topic id="gui-inspector" summary="Headless GUI inspector for layout review">

## GUI Inspector (built-in, no external dependencies)

The GUI inspector renders every tab **headlessly** — egui computes each widget's
rectangle on the CPU, so **no window appears** — and writes, per tab, a vector
wireframe you can see plus a structured layout a model can read. Use it to keep the
GUI clean, focused, and organised: review it whenever you add or move panels,
buttons, or fields, and as part of testing a GUI change.

Run:

```
facial ui-inspect [--out DIR] [--tab VOCAB ...]
```

- No flags → captures all 8 tabs. `--tab` (repeatable) limits to specific tabs
  (`project | quality_iq | identity | duplicates | run_debug | manual | compare |
  options`).
- Output (default `<workspace_root>/.facial/ui-snapshots/<timestamp>/`):
  - `<tab>.svg` — a labelled wireframe of panels/buttons/fields. **Open it in
    Firefox** to *see* the layout.
  - `<tab>.layout.json` — `screen` size, and `texts` / `rects` each with `x,y,w,h`.
    A model reads this to detect problems precisely.
  - `index.html` (links every tab SVG) + `index.json`.
- Output is deterministic: an unchanged GUI produces a byte-identical `layout.json`,
  so two snapshots diff cleanly for visual-regression review.

How to read it (find issues without opening the app):
- **Off-canvas**: any text/rect where `x + w > screen.w` (1280) or `y + h > screen.h`
  (800) is clipped — e.g. a long path that overflows the panel.
- **Overlap**: two texts at nearly the same `y` whose `x` ranges intersect.
- **Cramped / wasted space**: many single-line rows with tiny or uneven `y` gaps;
  zero-width text rows are empty placeholders eating vertical space.
- **Duplicates**: the same label text appearing twice usually means a redundant control.

When you add a GUI widget, keep it inspectable: render through `FacialApp::render_ui`
(both the live app and the inspector call it), so new widgets appear in the next
snapshot automatically.

</topic>

<topic id="safety" summary="The no-window safety rule">

## No-Window Safety Rule

No plugin, pipeline, GUI control, or debug action may launch an external OS
window or grab focus. There are NO file pickers (paths are typed/pasted as text),
NO Explorer/Finder/Browser launches from app controls, and NO UI-spawning
subprocesses. All model and backend navigation is file-based (commands/receipts/
intents) or via the headless CLI. Every execution and model action emits an event
to `<workspace_root>/.facial/data/events.jsonl` so activity is observable without
any foreground interruption. The feature key format is always `plugin_id:feature_id`, and the
default run path contract is
`<copy/output folder>/runs/<run_id>/<plugin>/<feature>/` in copy mode or
`<source parent>/.facial/runs/<run_id>/<plugin>/<feature>/` in in-place mode.

</topic>
