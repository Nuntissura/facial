# facial

Lightweight desktop app that merges selected source-app face quality, identity, and dedupe behaviors into one local Rust workflow.

The front of the app is a book-style **media browser** (WP-042..WP-049): thumbnail
grid + preview page, folder strip, tags/notes/color labels in a redb database,
favorites, full game-controller navigation with capture-based remapping, and
name/fuzzy/semantic search (CLIP embeddings via tract when models are provisioned,
local metadata fallback otherwise). The visual language is brutalist flat paper:
white with rough grain, black ink, thin black rules, sharp corners, no cards
(Ink mode inverts the same structure).

- Backend/runtime: Rust
- GUI: Rust `eframe/egui`
- Plugin execution: Rust executors in `product/src/plugins/*.rs`
- Plugin metadata: `product/plugins/<source>/metadata.json`
- Visual debugging: in-app event stream, headless `ui-inspect` snapshots, and run output manifests.

## Runtime contract
- No external Python runtime is required to run the main app.
- Standard run command:
  - `cargo run --manifest-path product/Cargo.toml`
- Set a project-specific runtime root:
  - `facial set_workspace_root --path D:/path/to/project`
- Standard release packaging command:
  - `powershell -ExecutionPolicy Bypass -File product/scripts/package-release.ps1`
- Image handling defaults to non-destructive copy mode and supports explicit in-place mode.

## Workspace portability
- `repo_root` is the app install root and only locates app assets such as docs, plugins, and config.
- `workspace_root` is the selectable runtime root for the active project.
- Runtime state defaults to `<workspace_root>/.facial/data` and `<workspace_root>/.facial/worktrees`.
- Set `workspace_root` through the GUI Project tab, `FACIAL_WORKSPACE_ROOT`, config `workspace_root`, or `facial set_workspace_root --path DIR`.
- Use `facial get_state` to verify `workspace_root`, `api_root`, and `worktrees_root` before queue or pipeline work.

## Canonical executable rule
- The one canonical executable: `product/facial.exe` (there is no `release/` folder).
- Superseded executables: `product/archive/exe/facial-<timestamp>.exe`.
- Cargo build/test scratch in `product/target/` is transient and removed once the build/test is validated; `package-release.ps1` deletes it automatically, and `cargo clean` clears it after interactive `cargo run`/`cargo test`. Nothing is written outside the repo.
- Package with `package-release.ps1`: it archives the current canonical exe (sha256-deduped), publishes the fresh build as `product/facial.exe`, and removes the `product/target/` scratch.
- `product/dist/` and `product/release/` are retired; any stray executable belongs in `product/archive/exe/`.
- Steady state: exactly one `facial.exe` in the repo (the canonical `product/facial.exe`) plus archived older builds; a second exists only transiently during an active build/test.
- Enforced by `product/scripts/check-exe-layout.ps1` (run automatically by `package-release.ps1`, or standalone) - it exits non-zero if any of the above is violated.

## Installer
- `package-release.ps1` compiles a Windows installer to `installer/out/facial-setup-<version>.exe` on every build, via Inno Setup (`ISCC.exe`; install once with `winget install --id JRSoftware.InnoSetup -e`).
- Installs to `%ProgramFiles%\Facial` (admin); settings + projects stay per-user under `%LOCALAPPDATA%\Facial` (the launcher sets `FACIAL_REPO_ROOT`/`FACIAL_CONFIG_PATH`/`FACIAL_WORKSPACE_ROOT`).
- Re-running setup offers four modes, least→most destructive (Update default): Update · Soft reinstall · Full reinstall · Uninstall. Update/Soft keep settings + projects; Full/Uninstall delete them and prompt per-item before deleting any relocated workspace.
- Models (`product/models/`) are not bundled; drop them into the install's `product/models/` to enable landmark/identity features.

## Repository layout
- `CODEX.md` — governance authority and operating guidance.
- `topology.yaml` — machine-readable topology and safety constraints.
- `governance/` — taskboard, packets, inspection notes.
- `specs/` — app specification.
- `product/` — Rust runtime (`src`) and plugin metadata (`plugins`).
- `worktrees/` — per-project working directories.
