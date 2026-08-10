---
file_id: REF-WP-060-EMBEDDED-VIDEO-PANEL-TERMINOLOGY-V1
file_kind: refinement
updated_at: "2026-08-10"
---

<topic id="operator-request" status="active" version="1" wp="WP-060" summary="Restore visible embedded video in both Media panels and establish one canonical panel vocabulary." updated_at="2026-08-09">

## Operator request

- Video audio currently plays while the embedded picture remains absent.
- A video must be playable in either the left thumbnail overview or the right full-media surface.
- Establish stable names that match app copy, source code, specification, manual, diagnostics, and operator/model conversations.
- Preserve the one-active-player and large-folder responsiveness contracts from WP-052, WP-057, and WP-058.
- Models must navigate and visually inspect the live app without activating, raising, focusing, or making it always-on-top. If the structured route is missing, add it immediately and document it as a Codex repo rule and in the built-in Manual.

## Canonical terminology

- **Library panel**: the left Media panel containing folder navigation and the virtualized thumbnail overview.
- **Viewer panel**: the right Media panel containing the selected image/video, playback controls, and metadata outside fullscreen.
- The resizable setting is **Library / Viewer split**.
- Playback ownership is `library` or `viewer`; only one native LibVLC player is active at a time.

</topic>

<topic id="evidence-and-research" status="complete" version="1" wp="WP-060" summary="Current source and primary documentation isolate the unverified failure to the native HWND attachment and visibility path." updated_at="2026-08-09">

## Current evidence

- `product/src/ui.rs` already calls the same player from the Library tile and Viewer paths; transport/audio state advances.
- `product/src/video_player.rs` discovers the parent through `GetActiveWindow` only when playback starts, creates a child `STATIC` HWND, and passes it to `libvlc_media_player_set_hwnd`.
- The operator hears audio, so media creation and playback advance; live video visibility remains the failed acceptance surface.
- Headless inspector fixtures intentionally do not start LibVLC and therefore cannot prove native child-window composition.
- Exact live root cause is `UNVERIFIED` until a fresh build is exercised against the operator-reported playback path.

## Sources checked

- eframe `Frame` implements `HasWindowHandle`, providing the actual application window handle rather than a focus-derived guess: <https://docs.rs/eframe/0.27.2/eframe/struct.Frame.html>
- Microsoft documents that `GetActiveWindow` only returns the active window attached to the calling thread's queue: <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getactivewindow>
- Microsoft documents child-window coordinates as parent-client coordinates and `SetWindowPos` as the size/position/Z-order API: <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowpos>
- VideoLAN documents `libvlc_media_player_set_hwnd` as the Win32 drawable attachment: <https://github.com/videolan/vlc-3.0/blob/master/include/vlc/libvlc_media_player.h>
- LibVLC native output remains preferred over memory callbacks because the latter can reduce efficiency and hardware-decoder use.

## Selected approach

- Capture the authoritative Win32 handle from the live eframe `Frame` every update and give it to `VideoPlayer`; never rediscover the parent through focus state.
- Validate and retain the parent, recreate/reparent the child only when required, check Win32 call results, and expose parent/child/bounds/visibility diagnostics.
- Keep one player and move its visible surface between Library and Viewer ownership; no simultaneous decoders or hover autoplay.
- Preserve cached thumbnails as the non-playing representation and stop the Library owner when its virtualized tile leaves view.
- Add a native capture/runtime proof in addition to the headless layout proof.
- Start automated live instances with an inactive viewport, navigate through existing receipt intents, and add a receipt-backed live framebuffer capture that composites LibVLC's decoded sidecar at the diagnosed native bounds.

## Rejected options

- Two simultaneous players for the same selection: duplicates decode and remote I/O and violates the responsiveness goal.
- LibVLC memory callbacks uploaded as egui textures: materially higher copy/decode cost and weaker hardware-decoder behavior.
- Continuing to use `GetActiveWindow`: focus is not an authoritative application-window identity.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-060" summary="Native-surface, ownership, DPI, overlay, and stale-audio failures have direct controls and proof gates." updated_at="2026-08-09">

## Risks and controls

- Wrong or stale parent HWND: source it from eframe, validate it before use, and recreate the child on parent change.
- Child is behind the renderer or never shown: check `SetWindowPos`/`ShowWindow` results, set explicit Z-order/show flags, and report diagnostics.
- DPI or client-origin mismatch: use egui points multiplied by the live pixels-per-point and test 100%, 125%, and 150% scale.
- Settings/folder overlays are covered: hide the child whenever an in-app modal is active and prove restoration afterward.
- Library tile scrolls away while sound continues: couple ownership to visible path/generation and stop on mismatch.
- Surface handoff creates a second decoder: assert one runtime/player/child HWND before and after Library-to-Viewer transitions.
- Model testing interrupts the operator: forbid foreground activation in CODEX, launch with `gui --background`, and use `ui_snapshot`; treat any missing inspectable state as a tooling defect rather than falling back to desktop control.

## Verification

- Unit tests for parent replacement, bounds conversion, owner transitions, and hide/restore state.
- Headless inspector snapshots for Library play affordance and Viewer controls with canonical terminology.
- Fresh background live executable proof: play in Viewer, capture a composited `ui_snapshot`, play in Library, capture another, and verify non-empty decoded sidecars, placement bounds, one player, and `foreground_activation: false` in both applied receipts.
- Verify overlays, fullscreen, resize, DPI, scroll-away, selection change, folder change, and app-tab change.
- Compare virtual-grid steady-frame p50/p95 and scan timing to WP-058; no more than 10% regression and p95 below 16.7 ms on the local fixture.

</topic>
