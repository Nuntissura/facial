---
file_id: REF-WP-051-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-051" summary="Add couch-distance controller folder navigation without weakening the desktop folder strip." updated_at="2026-08-09">

## Operator request

- Preserve the existing compact folder list for desktop behavior.
- Add a button that opens a large in-app folder navigator.
- Make folder names and folder icons readable from couch distance.
- Double the navigator's default visual scale, prevent its window from slowly growing,
  and lightly blur or soften the rest of the app while folder mode is active.
- Support scrolling and complete navigation with a controller.
- Keep the media-browsing context visible and return cleanly to it.
- Preserve Steam's controller window-switch shortcut; Facial must not open Settings
  when the operator uses Steam/Guide + Start/Menu for Alt+Tab.
- Provide a built-in controller app switch when Steam's chord is not delivered, plus
  an explicit controller cursor mode that can move and click the Windows pointer.
- Stop pointer injection and controller actions immediately when Facial loses focus so
  Steam and the newly focused application receive clean control.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-051" summary="Current controller and television UI guidance." updated_at="2026-08-09">

## Sources checked

- Microsoft Xbox Accessibility Guideline 101: console text at least 26 px at 1080p and couch-distance scaling.
- Microsoft Xbox Accessibility Guidelines 107, 112, and 113: single-press digital navigation, logical controller focus order, consistent accept/back behavior, remapping, and always-visible focus.
- Android TV focus, navigation, and typography guidance: D-pad focus groups, predictable movement, large readable typography.
- Apple focus and selection guidance: lists use whole-row highlights; focus and activation remain separate.
- Steam Guide Button Chord Layout field evidence: Steam/Guide + Start/Menu is the
  default Alt+Tab window-switch chord; Steam's current settings surface exposes the
  chord layout under Settings -> Controller -> Guide Button Chord Layout.
- Local gilrs 0.11.2 source: `Button::Mode` exposes the Guide/Mode button when the
  platform/controller backend reports it.
- Microsoft Win32 `SendInput`, `MOUSEINPUT`, and virtual-key documentation: a single
  ordered input batch can synthesize Alt+Tab, relative pointer movement, and mouse
  button transitions; injection is restricted by Windows UIPI.
- Existing Facial WP-044 folder strip, WP-046 action/remap system, WP-050 progressive scan/cache and visual-inspector surfaces.
- Microsoft `GetLogicalDrives`: returns a bitmask of assigned local, removable, and
  mapped drive letters without probing every possible root.
  <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getlogicaldrives>
- `resvg` 0.48.1 official API/source: pure-Rust SVG parsing and tiny-skia raster output,
  suitable for deterministic in-process inspector PNGs.
  <https://docs.rs/resvg/latest/resvg/> and <https://github.com/linebender/resvg>

## Selected approach

Keep the compact strip's density and add one large in-app `Folders` window. The toolbar button and a remappable controller action open it. It uses a stable viewport-clamped 1800×1360 surface, a non-interactive frosted veil over the media context, a vertical virtual list with 52-point names/icons, 112-point rows, an explicit accent focus fill, D-pad/left-stick scrolling, A to enter, B to parent, and restoration of grid context on close. Assigned drive roots come from one shared `GetLogicalDrives` helper and appear in the compact strip, existing picker, and couch list. At a drive root B focuses that drive instead of closing; Select/Back or Escape is the explicit close action. The stable fixed size and truncated path label prevent content-driven window growth.

Rasterize every inspector SVG to PNG in-process with `resvg`, loading the vendored Inter
and bundled Phosphor fonts. Keep SVG and layout JSON as parallel vector/structured proof.

Reserve Start/Menu as Facial's built-in immediate Alt+Tab action, including when a
Steam Guide chord exposes only its Start component. Send the complete Alt-down,
Tab-down/up, Alt-up sequence in one Windows input batch, clear Facial's held state,
and stop all pointer/action injection as soon as focus leaves Facial.

Add an explicit controller cursor mode, toggled by R3 or a visible toolbar control.
While active, the right stick moves the Windows pointer, A holds/releases left click,
and B holds/releases right click. Native D-pad navigation remains available. Cursor
mode turns off and releases any held mouse button on focus loss, preventing stuck input
or leakage into the newly focused application.

## Rejected options

- Enlarging the desktop strip: harms dense desktop use and still scrolls away.
- Shortcut-only parent/sibling navigation: cannot choose an arbitrary child folder.
- Always-on pointer emulation: conflicts with predictable D-pad focus. Use an explicit,
  visible pointer mode so native focus navigation and mouse-style control remain distinct.
- External system folder window: violates the in-app/no-external-window product constraint.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-051" summary="Risks, controls, and proof paths." updated_at="2026-08-09">

## Risks and controls

- Stale scan replaces a newly entered folder: reuse scan IDs and ignore stale batches.
- Focus becomes invisible: scroll the active row into view and inspector-check long lists.
- B exits unexpectedly: B navigates parent when possible and focuses the current drive at a filesystem root; only the explicit close action exits.
- Disk discovery stalls on disconnected roots: use the Windows assigned-drive bitmask;
  do not call `Path::exists()` across all 26 letters during window open.
- Repeated entry loses context: retain a per-parent child target and restore the previous grid cursor/scroll state.
- Huge child-folder counts lag: reuse the child-folder cache and virtualize large navigator rows.
- Popup leaks controller actions to the media grid: route actions to the navigator first while open.
- Existing binding conflict on Select/Back: version the binding table and migrate only the exact old default `OpenLocation=Select` mapping.
- Steam Alt+Tab chord leaks Start into Facial or Guide is not exposed by the backend:
  reserve Start as built-in Alt+Tab, send balanced key-up events in the same batch,
  clear held state, and suppress all Facial input after focus loss.
- Cursor input leaks to another app or leaves a mouse button held: gate every injection on
  current Facial focus and release both pointer buttons before handoff or mode exit.
- Native navigation and cursor clicks double-fire: pointer mode intercepts A/B before the
  binding dispatcher, while R3 explicitly enters/exits the mode.

## Verification

- Unit-test focus transitions, folder cursor clamping, binding migration, parent restoration, empty folders, and roots.
- Add inspector states for the couch navigator, deep folders, and long lists.
- Inject actions through a structured diagnostic route and verify folder/current-focus state.
- Raster-inspect app-emitted PNGs at 1280x800 and live app resolution; confirm drive
  roots, names, icons, focus, prompts, and no clipping without desktop automation.
- Package the canonical executable, run the layout invariant, and relaunch the exact 51,486-media folder.

</topic>
