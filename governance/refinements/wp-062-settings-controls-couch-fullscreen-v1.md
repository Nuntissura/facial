---
file_id: REF-WP-062-SETTINGS-CONTROLS-COUCH-FULLSCREEN-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-062" summary="Make Settings Controls compact and explicit, and add a distance-readable fullscreen Settings mode." updated_at="2026-08-09">

## Operator request

- The keyboard/controller mapping surface should not inherit a two-panel feel or consume unnecessary full width.
- Controller headings and cell content must always be present and readable.
- Add a fullscreen Settings option that enlarges both the surface and type for couch/distance operation.
- Do not reintroduce the historical Settings window that grows or changes size continuously.

</topic>

<topic id="evidence-and-research" status="complete" version="1" wp="WP-062" summary="Current source is three columns, but unassigned dashes and full-width stretching create ambiguity; fixed outer geometry remains mandatory." updated_at="2026-08-09">

## Current evidence

- Current worktree/source and `wp058-final` snapshots render Action, Keyboard, and Controller columns; six controller cells show only an em dash.
- The normal table expands across the entire Settings content width.
- WP-055 fixed a content-driven size feedback loop by clamping the outer window, reserving a fixed shell rectangle, and using one content ScrollArea.
- The current inspector checks 30 category passes for outer-bound stability and has a constrained high-font preset.
- The exact running binary reported by the operator has not been identified; stale packaging versus ambiguous dashes is `UNVERIFIED` until a fresh build is tested.

## Sources checked

- egui Window supports fixed/default/min/max sizing; its outer size includes frame/title margins: <https://docs.rs/egui/0.27.2/egui/containers/struct.Window.html>
- egui documents immediate-mode window/grid sizing feedback and remembered-size behavior: <https://github.com/emilk/egui/issues/4378>
- egui recommends avoiding very large fully laid-out scroll content and keeping visible work bounded: <https://github.com/emilk/egui>

## Selected approach

- Normal mode uses one centered, width-capped control table with three explicit columns and grouped action sections; it is not split into nested panels.
- Every empty binding cell reads `Unassigned`; the top includes a keyboard/controller explanation and live controller status.
- At narrow widths, use a bounded stacked action row with labeled Keyboard and Controller sub-rows rather than clipping columns.
- Couch fullscreen is a transient Settings-only mode with a separate stable window ID, viewport-inset fixed rectangle, local 28-32 point style, and 44-52 point hit targets.
- Track whether Settings entered native fullscreen and restore the exact prior app fullscreen state on exit.
- Preserve one ScrollArea, fixed header/footer, viewport clamps, and category-independent outer geometry.

## Rejected options

- Three independent scrolling panels: duplicates navigation and loses row association.
- Full-width stretched bindings in normal mode: wastes space and makes labels harder to scan.
- Mutating the global persisted app font size for couch mode: leaks a temporary interaction mode into normal UI state.
- Nested ScrollAreas or content-driven auto-sizing: reintroduces the known growth loop.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-062" summary="Geometry, font, state restoration, clipping, and controller-content failures have direct regression checks." updated_at="2026-08-09">

## Risks and controls

- Window grows per frame/category: retain fixed shell allocation and assert at most one-pixel drift across 30 passes.
- Couch mode corrupts normal geometry/font: use a distinct Settings ID and local style scope; do not persist temporary scale.
- Exit leaves the whole app fullscreen/windowed incorrectly: record prior viewport fullscreen state and restore it exactly.
- Controller text clips or appears absent: emit explicit headers, `Unassigned`, wrapping help, and per-cell layout assertions.
- Large action list costs every frame: keep one bounded Settings list and no filesystem/redb work during draw.
- Escape closes everything unexpectedly: first Escape leaves couch mode while Settings remains open; subsequent Escape closes normal Settings.

## Verification

- Inspector presets for normal, narrow/high-font, couch 1080p, and couch 4K.
- Assert all Media actions emit Action, Keyboard, and Controller text or `Unassigned`, with no clipped cells.
- Assert normal/couch bounds remain stable for 30 passes per category and header/footer remain reachable.
- Assert couch type and hit-target minimums plus enter/exit prior-fullscreen restoration.
- Fresh live build check with keyboard and connected controller when available.

</topic>
