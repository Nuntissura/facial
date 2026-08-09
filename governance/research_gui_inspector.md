---
file_id: research_gui_inspector
file_kind: research-basis
updated_at: 2026-06-10
---

<topic id="scope" wp="WP-008" summary="Why the GUI inspector research exists">

## Scope (WP-008: built-in GUI inspection tool)

Operator feedback: models navigate the backend fine (it was built properly), but the
egui GUI is "one big mess" and the operator is lost in it. There is no tool to *see* or
structurally inspect the rendered GUI — the existing "visual debugger" is a text
event-stream (`topology.yaml: visual_debugger.type=ui_log_stream`), and
`AppStateSnapshot` is logical state, not layout. Need a built-in inspection capability so
a model can evaluate panels/buttons/fields/layout and keep the GUI clean, focused, and
organized as features are added. Hard constraint: **built-in, no external dependencies.**

</topic>

<topic id="sources" wp="WP-008" summary="Sources checked">

## Sources checked

- eframe 0.27.2 docs (`Frame`): the old screenshot API (`Frame::request_screenshot`,
  `Frame::screenshot`, `App::post_rendering`) is **absent** in 0.27 — removed during the
  viewport migration.
- egui 0.27.2 docs (`viewport::ViewportCommand`): a `Screenshot` variant **exists**;
  result is delivered via `egui::Event::Screenshot`.
- egui issues #3654 and #5229: `ViewportBuilder::with_visible(false)` is **broken** — the
  window shows anyway, and on Windows 11 a hidden window stops receiving repaint events,
  stalling viewport commands. Invisible-window capture is not viable on 0.27/Win11.
- egui PR #5438 (post-0.27): native/web screenshot implementation was reworked later,
  implying 0.27's native screenshot delivery is unreliable/backend-specific.
- egui design + `egui_kittest` (the official egui snapshot crate, an *external* dep, so
  out of scope here): confirms egui computes full layout/rects CPU-side via
  `Context::run(raw_input, ui_closure)` with **no renderer or window** — backends only
  rasterize the resulting primitives. This is the basis for headless layout capture.

Links: docs.rs/eframe/0.27.2 struct.Frame; docs.rs/egui/0.27.2 viewport/enum.ViewportCommand;
github.com/emilk/egui issues/3654, issues/5229, pull/5438.

</topic>

<topic id="decision" wp="WP-008" summary="Selected approach: headless layout capture -> JSON + SVG">

## Selected approach

**Headless egui layout capture (no window, no renderer, no new crate).** For each tab,
run one egui pass with a synthetic `RawInput` (fixed `screen_rect`), driving the app's
existing per-tab UI code, and collect each interesting widget's `(kind, label, rect)`
into a collector. egui computes all rects on the CPU, so this runs fully offscreen.

Emit two artifacts per tab under `<workspace>/.facial/ui-snapshots/<timestamp>/`:
- `<tab>.layout.json` — ordered list of `{kind, label, x, y, w, h}` plus panel rects and
  the window size. Machine-readable: a model detects overlaps, off-canvas widgets,
  cramped spacing, and inconsistent alignment from this directly.
- `<tab>.svg` — a wireframe drawing each widget rect + label as plain SVG text (zero
  deps). The operator opens it in **Firefox** (the allowed browser) to *see* the
  structure. An `index.html`/`index.json` links all tabs.

**Driver:** a new headless CLI mode `facial ui-inspect [--out DIR] [--tab VOCAB ...]`
(no GUI window; reuses the existing tab vocab and `SelectTab` semantics). The per-tab UI
rendering in `ui.rs` is refactored so each tab body is callable with `&egui::Context`
(+ `&mut self`) from both the live `update()` and the headless inspector — no behavior
change to the live GUI. A thin `rec(response, kind, label)` collector helper is the
documented pattern developers use when adding widgets.

**Real-pixel screenshot** (`ViewportCommand::Screenshot` in a transient visible window)
is deferred as an optional add-on, gated on the 0.27 backend-delivery risk; the wireframe
is the reliable primary and is arguably clearer for diagnosing organizational "mess".

</topic>

<topic id="rejected" wp="WP-008" summary="Options rejected">

## Rejected options

- **`egui_kittest` snapshot harness:** the field-standard, but an external crate —
  violates the no-external-dependency constraint. Rejected (its headless-run technique is
  reused conceptually, without the crate).
- **Invisible-window eframe screenshot:** `with_visible(false)` is broken on 0.27/Win11
  (issues #3654, #5229). Rejected.
- **eframe `Frame::screenshot` / `post_rendering`:** does not exist in 0.27. Rejected.
- **Visible transient-window real screenshot as the v1 primary:** pops a foreground window
  (against [GLOBAL-BUILD-046]) and 0.27 native delivery is unreliable. Deferred to an
  optional add-on, not the v1 core.
- **OS-level screen capture / external screenshot tools:** external dependency and needs a
  focused window. Rejected.

</topic>

<topic id="risks" wp="WP-008" summary="Risks and mitigations">

## Risks + mitigations

- **Rect collection completeness:** egui doesn't expose a public retained widget tree, so
  capture relies on instrumenting widgets via `rec(...)`. Mitigation: capture panels +
  tab bar + all interactive widgets in v1; document `rec(...)` as the required pattern
  when adding widgets ([CODEX] GUI-inspection rule) so coverage doesn't rot.
- **Headless layout fidelity:** a single egui pass may not fully settle layout that
  depends on a prior frame's sizes. Mitigation: run two passes per tab, capture the
  second; feed a realistic `screen_rect` (e.g. 1280x800) and record it in the JSON.
- **State-dependent panels:** some tabs render differently with/without a loaded
  project/run. Mitigation: capture against the default post-launch state; allow a future
  `--state` hook. Note the captured state in `index.json`.
- **`frame`-coupled widgets:** any tab body using `eframe::Frame` can't run headless.
  Mitigation: refactor tab bodies to depend only on `&egui::Context`; confirm none need
  `frame` (state snapshot already comes from `self`, not `frame`).
- **SVG label escaping:** labels may contain `<`, `&`, quotes. Mitigation: XML-escape all
  text in the SVG emitter.
- **Determinism:** snapshots feed regression diffing. Mitigation: fixed screen_rect,
  fixed pass count, stable widget ordering -> byte-stable JSON for an unchanged GUI.

</topic>

<topic id="validation" wp="WP-008" summary="How the inspector will be verified">

## Validation plan

1. `facial ui-inspect` writes one `.layout.json` + `.svg` per tab (7 tabs) + an index;
   no GUI window appears (no-foreground proof).
2. Open a tab SVG in Firefox -> panels/buttons/fields are visible as a labeled wireframe
   (operator-viewable proof).
3. Re-run -> byte-identical JSON for an unchanged GUI (determinism proof).
4. A model reads a `.layout.json` and reports at least one concrete layout issue
   (overlap / off-canvas / spacing) -> proves model-inspectability, the WP's purpose.
5. Live GUI behavior unchanged after the `ui.rs` refactor (smoke-run the app).

</topic>
