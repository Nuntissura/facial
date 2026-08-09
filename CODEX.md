# CODEX

## 1) Purpose
This repository is the governance and implementation container for **facial**.
Goal: combine source-app behaviors into one lightweight desktop Rust app with:
- a backend service layer,
- a desktop navigation GUI,
- a model-safe visual debugger.

## 2) Source-of-truth artifacts
- `topology.yaml` (runtime + workspace map, safety constraints, and entrypoints)
- `specs/app-spec.md` (operational specification for building and extending Facial)
- `governance/taskboard.yaml` (project work planning)
- `governance/workpackets/*.yaml` (work packet contracts + status)
- `governance/work_packet_template.yaml` (new packet schema)
- `governance/source_app_registry.yaml` (source app feature inheritance, by app family)
- `governance/existing_app_audit.md` (inspection notes and parity basis)
- `product/Cargo.toml` and `product/src/**` (runtime and UI implementation)

## 3) Repo split
- `governance/` contains planning and machine-readable workflow state.
- `product/` contains executable app behavior, debug sinks, and plugin runtimes.
- `specs/` contains durable product behavior narrative.
- `worktrees/` contains per-project runtime workspaces.
- No production behavior belongs in `governance/`.

## 4) Runtime contract and language mandate
- Facial runtime must remain Rust-native.
- Plugin execution must occur through `product/src/plugins/*.rs`, not external Python processes.
- `product/plugins/*.json` is metadata only, used for discoverability and attribution.
- Python files under `product/` are out-of-band legacy artifacts and must not be used for runtime execution.
- The app install root and runtime workspace root are separate concepts.
- `repo_root` locates installed app assets and must not force another project to use the facial repo as its runtime root.
- `workspace_root` is selectable by GUI, `FACIAL_WORKSPACE_ROOT`, config `workspace_root`, or `set_workspace_root --path DIR`.
- Default runtime state for a selected workspace lives under `<workspace_root>/.facial/`.
- Models working from another project should set `workspace_root` to that project folder before running imports, pipelines, or queue work.

## 5) Required startup workflow
- Read `governance/taskboard.yaml` and `topology.yaml` before edits.
- Run app:
  - `cargo run --manifest-path product/Cargo.toml`
- Set/select a project runtime root:
  - `facial set_workspace_root --path DIR`
- Package a distributable executable:
  - `powershell -ExecutionPolicy Bypass -File product/scripts/package-release.ps1`
- Verify the single-canonical-exe invariant (cannot deviate):
  - `powershell -ExecutionPolicy Bypass -File product/scripts/check-exe-layout.ps1`
- Consult active spec in `specs/app-spec.md` for behavior and tests-by-model.
- Code changes should update:
  - `governance/taskboard.yaml` (status linkage),
  - corresponding work packet(s),
  - and `topology.yaml` where execution paths/constraints change.

## 5.1) Canonical executable artifact rule (WP-023)
- There is exactly one canonical executable in the repo: `product/facial.exe`.
- There is no `product/release/` folder; it was removed. The canonical exe lives directly at `product/facial.exe`.
- Superseded executables must be moved to `product/archive/exe/facial-<timestamp>.exe`; they are never left loose beside the canonical exe.
- Cargo build/test scratch lives in the in-repo default `product/target/` only transiently, during an active build or test. It is invalid once the build/test is validated and must be removed immediately: `package-release.ps1` deletes `product/target/` automatically after publishing; for interactive `cargo run`/`cargo test`, run `cargo clean` (or delete `product/target/`) once the run is validated. Nothing is written outside the repo.
- Do not manually overwrite `product/facial.exe`; use `product/scripts/package-release.ps1`, which archives the current canonical exe (sha256-deduped), publishes the fresh build as `product/facial.exe`, and then deletes the `product/target/` scratch.
- `product/scripts/package-release.ps1` archives the superseded exe only when its sha256 differs from every existing `product/archive/exe/` build, so repeat packaging without a code change does not accumulate duplicate archive copies.
- `product/dist/` and `product/release/` are retired and are not executable surfaces; any executable found in them must be drained into `product/archive/exe/` (or promoted to the canonical exe) and the folder left absent.
- Steady-state invariant: the repo holds exactly one `facial.exe` — the canonical `product/facial.exe` — plus archived older builds in `product/archive/exe/`. A second `facial.exe` exists only transiently in `product/target/` during an active build/test and is removed the moment that build/test is validated.
- Enforcement: `product/scripts/check-exe-layout.ps1` verifies this invariant (one canonical `product/facial.exe`; all other builds archived as `facial-<timestamp>.exe`; no `product/target/` scratch; retired `release/`+`dist/` absent; nothing built outside the repo) and exits non-zero on any violation. `package-release.ps1` runs it automatically after publishing, and it can be run standalone at any time. Treat a non-zero result as a release-quality defect to fix before handoff.

## 5.2) Installer (WP-025)
- `product/scripts/package-release.ps1` also compiles a Windows installer to `installer/out/facial-setup-<version>.exe` on every run via Inno Setup (`ISCC.exe`); if Inno Setup is absent, packaging hard-fails with the install command.
- Source lives in `installer/`: `facial.iss` (Inno Setup script) + `launch-facial.cmd` (per-process launcher). Build outputs (`installer/out`, `installer/payload`) are git-ignored.
- Installs to `%ProgramFiles%\Facial` (admin elevation). Assets are read-only there; settings + projects are kept per-user under `%LOCALAPPDATA%\Facial`, wired by the launcher via `FACIAL_REPO_ROOT` / `FACIAL_CONFIG_PATH` / `FACIAL_WORKSPACE_ROOT`.
- Re-running setup on an existing install offers four modes, least->most destructive, Update default: Update (keep data) · Soft reinstall (keep data) · Full reinstall (delete data) · Uninstall (delete data). A relocated workspace is deleted only on explicit per-item confirmation.
- App contract: `product/src/config.rs` honors `FACIAL_CONFIG_PATH` so a read-only Program Files install keeps settings writable; unset, settings stay in-repo (dev unchanged).

## 6) Built-in manual contract (operator + model required)
The application must expose an in-UI manual that is sufficient for a model with no prior context.

Required manual sections:
- App purpose and runtime state model.
- How to add/select projects and worktrees.
- How to ingest images (copy default, explicit in-place mode).
- How to load plugin features, run pipelines, and read results.
- Where artifacts are written (`runs/<run_id>/...`).
- How to debug failures from the built-in visual debugger.
- Recovery steps for common failures.

This manual must be discoverable from the app UI and mirrored in:
- this `CODEX.md` file,
- `specs/app-spec.md`.

## 7) Model-safe interaction and no-context operation
- No external file-manager/browser windows may be launched by standard UI controls.
- No global keyboard/mouse capture is allowed for app automation workflows.
- All model debugging must be observable through in-app text surfaces:
  - feature run state,
  - plugin execution traces,
  - errors and warnings,
  - event stream in the visual debugger.
- Model actions must be deterministic and recoverable:
  - actions are logged,
  - outputs are timestamped,
  - run IDs are explicit.

## 7.1) GUI inspection contract (keep the GUI clean and organised)
- A built-in, self-contained GUI inspector exists: `facial ui-inspect [--out DIR] [--tab VOCAB ...]`.
- It renders every tab headlessly (no window) and writes, per tab, a `<tab>.png`,
  `<tab>.svg` wireframe, and a `<tab>.layout.json` (model-readable
  rects + text) under `<workspace_root>/.facial/ui-snapshots/<timestamp>/`.
- Inspector metadata uses an isolated redb file inside the snapshot workspace, so
  inspection can run beside a live GUI without locking or mutating operator metadata.
- GUI inspection is required, not optional:
  - When GUI features, panels, buttons, fields, or items are added or moved, run the
    inspector and review the affected tab(s) for overflow, overlap, duplication,
    cramped spacing, and wasted space before considering the change done.
  - GUI changes must be inspected as part of testing; the layout JSON is deterministic,
    so snapshots diff for visual-regression review.
- New widgets must render through `ui::FacialApp::render_ui` (the single path shared by
  the live app and the inspector) so they appear in snapshots automatically.
- Goal: the GUI stays clean, focused, and organised as the product grows; the inspector
  is the standing check that enforces it.

## 8) Data safety behavior
- Default image handling is non-destructive copy mode.
- In-place mode is explicit and surfaced in UI/config.
- Default ingestion and pipeline run must not delete source assets.

## 8.1) Media navigation and playback contract (WP-053)
- Media scans and searches are limited to the selected folder and its explicit Tree setting; the app must never imply PC-wide search.
- Media Refresh/F5 rescans that selected folder; header Global Refresh only reloads app metadata and retryable thumbnail state.
- The media-kind selector is one Images/Videos/All dropdown.
- Folders must open in-app from Ctrl+G and accept local paths, mapped drives, and reachable UNC shares; no standard UI control launches Explorer or creates a network mapping.
- Embedded video loops by default, has a persisted Settings → Playback toggle, and exposes `media_video_control --action loop --value 0|1` for no-context models.
- Controller video transport uses A/Enter for play-pause and right stick for seek/volume; mappings remain visible and remappable in the resize-aware Controls table.
- Media context menus must copy absolute and portable paths as text. Portable means workspace-relative when possible and selected-media-root-relative otherwise.

## 9) Task hygiene
- Every actionable change must reference one or more work packet IDs.
- New work packets are created from `governance/work_packet_template.yaml`.
- `governance/taskboard.yaml` links active packets and current status.

## 10) Taskboard + packet linkage
- `governance/taskboard.yaml` is the project switchboard.
- `governance/workpackets/*.yaml` define scope, acceptance, validation, and status.
- Spec updates must mention the affected work packet in `specs/app-spec.md` or local notes before implementation.

## [OPERATOR-AUTHORITY] Operator Authority Over Pace, Scope, and Stopping

- [OPERATOR-AUTHORITY-001] The assistant/agent is FORBIDDEN to decide pace, scope, or when it stops working.
- [OPERATOR-AUTHORITY-002] The operator alone decides scope, pace, and when work stops.
- [OPERATOR-AUTHORITY-003] The assistant must not defer, split, subset, reprioritize, hand off, or drop any operator-requested work on its own judgment.
- [OPERATOR-AUTHORITY-004] The assistant must not stop, pause, slow down, or declare work "done for now" or "the rest is optional" unless the operator explicitly says so.
- [OPERATOR-AUTHORITY-005] When the operator lists multiple requirements, the assistant implements ALL of them and may not hand back a partial result and call it done.
- [OPERATOR-AUTHORITY-006] The assistant may not use tokens, session limits, capacity, or effort as a reason to stop, slow, or narrow operator-requested work.
