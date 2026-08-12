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
- `governance/build_rules.yaml` (mandatory implementation, style, app-behavior, executable, tab-state, and verification rules)
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
- Before implementation, read `governance/build_rules.yaml`, `governance/taskboard.yaml`, `topology.yaml`, the active work packet, its refinement, and the touched spec anchors.
- Run app:
  - `cargo run --manifest-path product/Cargo.toml --bin facial -- --background`
- Set/select a project runtime root:
  - `facial-cli set_workspace_root --path DIR`
- Package a distributable executable:
  - `powershell -ExecutionPolicy Bypass -File product/scripts/package-release.ps1`
- Verify the single-canonical-exe invariant (cannot deviate):
  - `powershell -ExecutionPolicy Bypass -File product/scripts/check-exe-layout.ps1`
- Consult active spec in `specs/app-spec.md` for behavior and tests-by-model.
- Code changes should update:
  - `governance/taskboard.yaml` (status linkage),
  - corresponding work packet(s),
  - and `topology.yaml` where execution paths/constraints change.

## 5.1) Canonical delivery-artifact rule (WP-059 supersedes the WP-023 layout)
- `installer/` is the only current delivery surface and contains exactly two root-level executable artifacts: `facial-portable-<version>.exe` and `facial-setup-<version>.exe`.
- Every successful `product/scripts/package-release.ps1` run increments the numeric Cargo patch version exactly once before compilation and uses that same version in both current artifact names and `topology.yaml`.
- A failed build or installer compile before publication restores the prior Cargo/topology/lock version and leaves the current delivery pair untouched.
- Before a new pair is published, every superseded installer and portable executable is moved to `installer/installer-portable-archive/`; older artifacts must never remain loose in `installer/`.
- `product/facial.exe`, `product/archive/exe/`, and `installer/out/` are retired delivery surfaces. The packaging script migrates their existing executable artifacts into `installer/installer-portable-archive/` and removes the retired surfaces.
- Cargo build/test scratch remains the in-repo `product/target/` directory only while a build or test is active. Packaging removes it after validation; interactive work must clean it after validation. Nothing is written outside the repository.
- Do not manually publish, rename, or overwrite delivery executables. Use `product/scripts/package-release.ps1` so versioning, archiving, installer compilation, publication, cleanup, and invariant validation happen as one workflow.
- `product/scripts/check-exe-layout.ps1` requires exactly the version-matched portable/setup pair at the `installer/` root, permits historical executables only in `installer/installer-portable-archive/`, rejects legacy/stray/transient executable surfaces, and exits non-zero on deviation.

## 5.2) Installer (WP-025 extended by WP-059)
- `product/scripts/package-release.ps1` compiles `installer/facial.iss` through Inno Setup (`ISCC.exe`) and publishes `installer/facial-setup-<version>.exe`; missing ISCC hard-fails before the version is changed.
- Installer source is `installer/facial.iss`; shortcuts and the completion-page launch target the GUI-subsystem `facial.exe` directly. Transient `installer/payload/` staging is removed after packaging.
- The installer presents explicit checkbox tasks for a Desktop shortcut and a Windows Start-menu **All apps** shortcut.
- The installer must not claim to force an application into the Windows Start menu's Pinned grid; Windows reserves that user choice. The installed Start-menu shortcut makes Facial discoverable and manually pinnable.
- The completion page presents a checked `Launch Facial` action and launches it as the original, normally non-elevated user; silent installations do not launch it.
- Installs to `%ProgramFiles%\Facial` (admin elevation). Assets are read-only there; the installed GUI resolves writable settings and its default workspace internally under `%LOCALAPPDATA%\Facial`, without a batch launcher or launcher-defined environment variables. Explicit `FACIAL_REPO_ROOT` / `FACIAL_CONFIG_PATH` / `FACIAL_WORKSPACE_ROOT` overrides remain available for development and automation.
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
- A built-in, self-contained GUI inspector exists: `facial-cli ui-inspect [--out DIR] [--tab VOCAB ...]`.
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

## 7.2) Background-only model navigation and live inspection
- [FACIAL-MODEL-INSPECT-001] Models must not activate, focus, raise, foreground, or set the Facial window always-on-top to navigate, test, inspect, or capture it.
- [FACIAL-MODEL-INSPECT-002] Model navigation must use receipt-backed UI intents; model visual inspection must use `ui-inspect` for deterministic fixtures or `ui_snapshot` for the exact live framebuffer.
- [FACIAL-MODEL-INSPECT-003] Automated live-GUI runs must launch with `facial.exe --background` (or `cargo run --bin facial -- --background` in development); this launch mode must never request initial window activation.
- [FACIAL-MODEL-INSPECT-004] `ui_snapshot` must capture without foreground activation and preserve the active embedded-video region at the native surface's diagnosed bounds, compositing a LibVLC snapshot when available or retaining the exact live-framebuffer crop as its sidecar fallback.
- [FACIAL-MODEL-INSPECT-005] If an app state cannot be navigated or visually inspected through these background-safe structured routes, the tooling is incomplete; add the missing intent, capture, diagnostics, inspector fixture, and Manual instructions before continuing that model workflow.

## 8) Data safety behavior
- Default image handling is non-destructive copy mode.
- In-place mode is explicit and surfaced in UI/config.
- Default ingestion and pipeline run must not delete source assets.

## 8.1) Media navigation and playback contract (WP-053, WP-063)
- Media owns a persistent document-tab strip above Library and Viewer. Each tab snapshots its folder, selected item(s), cursor, search/filter/sort/layout, Library scroll, and staged folder-navigator state while sharing the same metadata database and single playback backend.
- Folder navigation is transactional: Browse/Parent/Go only change the staged navigator path. `Open folder` commits it to the active tab and scans; `Open in new tab` creates and selects a tab for the exact staged path without changing the prior tab.
- One LibVLC player is leased explicitly to either a visible Library tile or Viewer; starting one owner replaces the other. Playback work has priority over thumbnail work, and large-folder inventory caches keep the last-good tab viewport visible while reconciliation scans run.
- Installed `facial.exe` is a Windows GUI-subsystem executable. Terminal, model, inspection, and controller-probe commands use `facial-cli.exe` so they retain stdout/stderr without causing GUI console flash.
- Controller acquisition must not depend on Steam. A directly enumerated Windows joystick is accepted before WGI initialization, while gilrs/WGI remains available for controllers absent from that route; Start/Menu is a focus-gated rising-edge Alt+Tab action. `facial-cli controller-probe` reports both acquisition paths.
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
