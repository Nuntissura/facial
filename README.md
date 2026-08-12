# facial

Lightweight desktop app that merges selected source-app face quality, identity, and dedupe behaviors into one local Rust workflow.

The front of the app is a book-style **media browser** (WP-042..WP-063): persistent
folder tabs above a left **Library panel** for navigation and the virtualized
thumbnail overview, and a right **Viewer panel** for selected-media playback and
metadata. Each tab keeps an independent viewport while all tabs share the same redb
metadata database and one explicitly leased Library-or-Viewer LibVLC player. It also provides tags/notes/color labels,
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
  - `cargo run --manifest-path product/Cargo.toml --bin facial -- --background`
- Set a project-specific runtime root:
  - `cargo run --manifest-path product/Cargo.toml --bin facial-cli -- set_workspace_root --path D:/path/to/project`
- Standard release packaging command:
  - `powershell -ExecutionPolicy Bypass -File product/scripts/package-release.ps1`
- Image handling defaults to non-destructive copy mode and supports explicit in-place mode.

## Workspace portability
- `repo_root` is the app install root and only locates app assets such as docs, plugins, and config.
- `workspace_root` is the selectable runtime root for the active project.
- Runtime state defaults to `<workspace_root>/.facial/data` and `<workspace_root>/.facial/worktrees`.
- Set `workspace_root` through the GUI Project tab, `FACIAL_WORKSPACE_ROOT`, config `workspace_root`, or `facial-cli set_workspace_root --path DIR`.
- Use `facial-cli get_state` to verify `workspace_root`, `api_root`, and `worktrees_root` before queue or pipeline work.

## Canonical delivery-artifact rule
- `installer/` contains exactly one current portable executable (`facial-portable-<version>.exe`) and one current installer (`facial-setup-<version>.exe`).
- Superseded installers and portable builds live only under `installer/installer-portable-archive/`.
- Cargo build/test scratch in `product/target/` is transient and removed once the build/test is validated; `package-release.ps1` deletes it automatically, and `cargo clean` clears it after interactive `cargo run`/`cargo test`. Nothing is written outside the repo.
- Every successful `package-release.ps1` run bumps the Cargo patch version once, archives the prior pair, publishes a version-matched new pair at `installer/`, and removes scratch. Failed pre-publication builds restore the previous version.
- `product/facial.exe`, `product/archive/exe/`, `product/dist/`, `product/release/`, and `installer/out/` are retired delivery surfaces.
- Enforced by `product/scripts/check-exe-layout.ps1` (run automatically by `package-release.ps1`, or standalone) - it exits non-zero if any of the above is violated.

## Installer
- `package-release.ps1` compiles `installer/facial-setup-<version>.exe` via Inno Setup (`ISCC.exe`; install once with `winget install --id JRSoftware.InnoSetup -e`).
- Setup asks whether to create Desktop and Windows Start-menu/All-apps shortcuts and offers a checked **Launch Facial** action when installation completes. Windows Pinned-grid placement remains a user-controlled action.
- Installs the GUI-subsystem `facial.exe` and console-subsystem `facial-cli.exe` to `%ProgramFiles%\Facial` (admin). Shortcuts launch `facial.exe` directly; the installed app resolves writable settings and the default workspace internally under `%LOCALAPPDATA%\Facial` without a batch launcher.
- Re-running setup offers four modes, least→most destructive (Update default): Update · Soft reinstall · Full reinstall · Uninstall. Update/Soft keep settings + projects; Full/Uninstall delete them and prompt per-item before deleting any relocated workspace.
- Models (`product/models/`) are not bundled; drop them into the install's `product/models/` to enable landmark/identity features.

## Repository layout
- `CODEX.md` — governance authority and operating guidance.
- `topology.yaml` — machine-readable topology and safety constraints.
- `governance/` — taskboard, packets, inspection notes.
- `specs/` — app specification.
- `product/` — Rust runtime (`src`) and plugin metadata (`plugins`).
- `worktrees/` — per-project working directories.
