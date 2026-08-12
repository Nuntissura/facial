---
file_id: REF-WP-063-GUI-CLI-MEDIA-TABS-FOLDER-COMMIT-V1
file_kind: refinement
updated_at: "2026-08-10"
---

<topic id="operator-request" status="active" version="1" wp="WP-063" summary="Split GUI and CLI executables, align modal behavior, stage folder browsing, and add persistent Media document tabs." updated_at="2026-08-10">

## Operator request

- Build `facial.exe` as a true Windows GUI-subsystem executable.
- Move terminal and model commands to `facial-cli.exe` while sharing Rust command implementation.
- Point installer shortcuts and post-install launch directly to `facial.exe`; remove the batch-launch dependency.
- Resolve `%LOCALAPPDATA%\Facial` internally for installed GUI settings and workspace state.
- Make the folder navigator use the same softened blurred backdrop behavior as Settings.
- Browsing inside the folder navigator must not switch the Media folder in the background; only an explicit Open commits it.
- Add `Open in new tab` to the folder navigator.
- Restructure Media as `Media > tabs > existing Library/Viewer viewport`, with folder-named tabs and per-tab state over the same shared database and services.
- Add missing structured navigation, manipulation, inspection, diagnostics, and built-in Manual instructions whenever the app cannot otherwise prove the outcome.
- Restore controller input without requiring Steam and reserve Start/Menu for one focus-gated Alt+Tab action.
- Restore reliable, responsive video playback in both the Library thumbnail overview and Viewer panel, including folders containing 600, 16,000, or more videos.
- Make double-click/Open file hand the exact selected video to its registered Windows application.
- Keep the planned Library-versus-Viewer playback split as one explicit-owner decoder design rather than two competing playback engines.

</topic>

<topic id="evidence-and-research" status="complete" version="1" wp="WP-063" summary="Current source has one mixed CUI binary, batch-targeted shortcuts, immediate folder commits, one Media lane, and separate Settings/folder backdrop paths." updated_at="2026-08-10">

## Baseline project evidence

- `product/src/main.rs` routes GUI and every headless command through one binary and calls `hide_console_for_gui()` only after process startup.
- `installer/facial.iss` targets `launch-facial.cmd` for Start-menu, Desktop, and post-install launch actions.
- `installer/launch-facial.cmd` injects install/config/workspace environment variables before starting `facial.exe`.
- `product/src/ui.rs::media_navigator_navigate_to` mutates the active lane folder and requests a scan for every navigator enter, parent, drive, or direct-location action.
- `product/src/ui.rs::draw_media_tab` hardcodes the first compare lane as the single Media viewport.
- Settings captures, downsamples, and Gaussian-blurs the unobscured framebuffer; the folder navigator currently uses only the neutral fallback veil.
- Async scan/search/stat/folder keys already carry lane and generation fields that can be extended into stable per-tab attribution.
- `MediaDb` already provides a shared settings table suitable for a versioned tab-session record.

## Current external sources checked

- Microsoft PE format defines subsystem 2 as `IMAGE_SUBSYSTEM_WINDOWS_GUI` and subsystem 3 as `IMAGE_SUBSYSTEM_WINDOWS_CUI`: <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format>
- The Rust Reference states that `#![windows_subsystem = "windows"]` is a binary-crate-root attribute and that the console subsystem is the default: <https://doc.rust-lang.org/reference/runtime.html#the-windows_subsystem-attribute>
- Microsoft Windows TabView guidance defines dynamic document tabs, requires an active tab, recommends folder/document names as labels, and documents Ctrl+Tab, Ctrl+T, Ctrl+W, and deterministic neighbor selection: <https://learn.microsoft.com/en-us/windows/apps/design/controls/tab-view>
- Microsoft Win32 tab guidance says changing tabs must not have side effects and page-scoped controls belong inside the tab content: <https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-tabs>
- egui 0.27 documentation requires stable unique IDs for multiple dynamic windows and retains immediate-mode GUI state by ID: <https://docs.rs/crate/egui/0.27.0>
- The upstream egui repository and issue/release history were checked for immediate-mode persistence, modal/window identity, and bounded rendering patterns: <https://github.com/emilk/egui>
- The `egui_dock` source was checked as an adjacent tab-state implementation; its generic docking surface is broader than Facial needs: <https://github.com/Adanos020/egui_dock>
- Inno Setup official documentation/FAQ was checked for direct executable targets in `[Icons]` and `[Run]`: <https://jrsoftware.org/ishelp/>
- Reddit field discussions were checked for egui persistence and immediate-mode state ownership; they reinforced explicit app-owned state but are not normative implementation authority.
- Hugging Face, Civitai, and X/Twitter searches produced no directly relevant implementation evidence for a Rust/egui local media browser tab architecture.
- Current GUI-agent research was checked for structured state-transition verification; the relevant pattern is to separate persistent task state from transient observations and verify action results through structured state, not screenshot-only inference.
- Microsoft `joyGetPosEx` documentation confirms WinMM can query joystick position, extended axes, POV, and button state for device IDs 0–15: <https://learn.microsoft.com/en-us/windows/win32/api/joystickapi/nf-joystickapi-joygetposex>
- Microsoft `ShellExecuteW` documentation confirms the `open` verb operates on the exact supplied file and delegates document handling to the registered association: <https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecutew>
- VideoLAN's LibVLC header documents `libvlc_media_player_set_hwnd` as the supported Win32/Win64 drawable attachment and `get_hwnd` as its diagnostic inverse: <https://videolan.videolan.me/vlc-3.0/libvlc__media__player_8h_source.html>

## Selected approach

- Create a Rust library target shared by `facial.exe` and `facial-cli.exe`; keep GUI startup and CLI dispatch in separate binary crate roots.
- Apply the Windows GUI subsystem only to the GUI binary and retain the console subsystem for CLI.
- Let the GUI select installed per-user defaults internally while preserving environment overrides and repo-local development behavior.
- Remove the installer batch launcher and install/launch both executables directly.
- Introduce versioned Media-tab session state with stable IDs and a stable lane identity per tab; keep metadata/database/cache/runtime services workspace-scoped.
- Render a document-style Media tab strip above the existing Library/Viewer viewport; add, switch, close, and folder-name labeling without rebuilding the viewport hierarchy.
- Treat navigator location as staged state. Enter/parent/drive/Go only navigates the modal; `Open folder` commits to the active tab and `Open in new tab` creates a new tab.
- Reuse Settings backdrop capture/blur and the shared modal shell for the folder navigator.
- Extend receipt-backed intents and diagnostics so model verification can prove active tab, staged folder, committed folder, tab inventory, and scan attribution without foreground automation.
- Accept a WinMM joystick with capability/VID/PID normalization before entering WGI, avoiding a broken WGI startup when Windows already exposes the device directly; retain gilrs/WGI for pads absent from WinMM. Gate Start/Menu Alt+Tab on app focus and a rising edge.
- Keep one LibVLC player and explicitly lease it to Library or Viewer. Prioritize playback I/O, retain exact pending selections while asynchronous large-folder display order builds, and trace owner/surface/timing state.
- Use `ShellExecuteW(L"open", exact_utf16_path)` for explicit external handoff instead of assuming a `vlc.exe` executable or reconstructing a command line.

## Rejected options

- Keep a single CUI binary and hide or detach the console after startup: Windows can create the console before Rust code runs.
- Mark the mixed GUI/CLI binary as GUI subsystem: terminal commands would lose normal console attachment and reliable stdout/stderr behavior.
- Keep the batch wrapper but hide cmd.exe: this retains a second launch boundary and environment coupling.
- Use `egui_dock`: Facial needs one bounded Media tab strip, not general docking/tear-out complexity.
- Share one mutable Media viewport object across tabs without stable lane IDs: in-flight asynchronous results can land in the wrong folder after a switch.
- Commit every navigator row activation: it violates browse-before-open semantics and causes background churn.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-063" summary="Executable, persistence, async attribution, modal, and folder-commit failures have explicit controls and independent proof gates." updated_at="2026-08-10">

## Risks, failure scenarios, and controls

- GUI still flashes a console because the attribute lands on a library or CLI crate: inspect the packaged PE optional-header subsystem directly for both binaries.
- CLI silently loses output or exit codes: run help, successful receipt, invalid command, and failing command from PowerShell and inspect stdout, stderr, and `$LASTEXITCODE`.
- Installed GUI cannot find assets or write settings without launcher variables: install into a clean test location with Facial environment variables removed, then inspect resolved state and written config path.
- Packaging publishes only the GUI and omits the CLI: validate installer payload/install tree and preserve the two-artifact installer-root delivery invariant separately from installed payload contents.
- Tab switch redirects an old scan/search/stat result: bind every producer and consumer to stable lane/tab plus generation IDs and run concurrent/stale-result tests.
- Session corruption removes all tabs or points at missing paths: version the record, validate on load, retain at least one default tab, and surface unavailable folders without deleting state.
- Duplicate leaf folder names make tabs ambiguous: add deterministic visual disambiguation and full-path hover/diagnostics while retaining exact path identity.
- Shared database is accidentally duplicated per tab: construct one workspace `MediaDb` and pass only tab-local viewport state through tab changes.
- Staged navigator actions still mutate the background through an unchanged caller: compare active folder, scan ID, file inventory, selection, and playback before/after enter/parent/Go; only commit actions may change them.
- Folder and Settings backdrops drift: route both through the same capture/blur function and inspect their rendered PNGs side by side.
- Closing or switching tabs destroys unsaved viewport state: save the outgoing state before activation changes and transactionally persist the session.
- Model tooling cannot prove the state transition: add tab list/select/open/close and staged-folder diagnostics/intents, deterministic inspector fixtures, and Manual recovery steps before completion.
- WGI omits a connected controller while HID/Windows joystick APIs expose it: enumerate and poll the fallback independently; report both paths in `controller-probe`; normalize only after acquisition.
- Start/Menu repeats Alt+Tab while held or fires in the background: require a focused rising edge, release pointer buttons, and unit-test held/focus-loss counterfactuals.
- A 100k+ inventory publishes after a file-play intent and the action targets a stale row: retain the exact requested file index/path for a bounded wait, then resolve against the completed generation before scrolling and starting playback.
- Library and Viewer create competing decoders or a hidden surface keeps playing: enforce a single owner lease, stop/reattach on owner change, and diagnose native child visibility/bounds/HWND attachment.
- External handoff starts VLC but does not open the file because of quoting or executable discovery: pass the exact UTF-16 file path to the Windows `open` verb and verify a successful handoff receipt on the operator's exact path.

## Verification

- Focused unit tests for binary-path mode selection, installed path resolution, tab-session migration, tab close/neighbor selection, duplicate labels, and staged-versus-committed navigation.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Build both binaries and directly inspect PE subsystem values `2` and `3`.
- Run CLI positive and negative commands and inspect stdout/stderr/exit codes.
- Run `facial-cli ui-inspect --tab media` with presets for one tab, multiple tabs, duplicate names, staged navigation, Settings, and folder modal; open affected PNGs and compare layout JSON.
- Launch GUI only with `facial.exe --background`, drive receipt-backed tab/folder/playback intents through `facial-cli`, and capture exact live `ui_snapshot` without foreground activation.
- Probe the connected controller with Steam-independent primary/fallback diagnostics and run focused mapping plus focus-gated Start/Menu tests.
- On both operator-provided folders, prove Library and Viewer playback time advances with the native child visible at the requested bounds; record recursive inventory size and frame responsiveness on the 100k+ case.
- Run packaging, installer compile, clean install/launch, shortcut-target inspection, and canonical delivery-layout validation.
- Perform the required high-risk adversarial review with diff-derived attack surfaces, independent checks, counterfactuals, boundary probes, negative paths, findings, and residual uncertainty.

</topic>
