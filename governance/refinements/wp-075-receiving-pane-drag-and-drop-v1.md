---
file_id: REF-WP-075-RECEIVING-PANE-DRAG-AND-DROP-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-075" summary="Right-click a Media tab to open it in the right panel as a receiving folder, then drag and drop Library items into it (or use the move/copy actions) to file media into other folders." updated_at="2026-08-15">

## Operator request

Verbatim operator items folded into this packet (2026-08-15):

- "i want to a simple way to drag and drop items into other folders, so the right panel becomes the
  recieving folder."
- "we can tab multiple windows/folders now, but a right click option should be open in right panel
  so i can drag and drop files or use the before mentioned features)"

## Interpretation

- Right-clicking a Media tab offers **Open in right panel**: the right panel (normally the Viewer)
  becomes a **receiving pane** showing that tab's folder.
- Items dragged from the left Library grid and dropped on the receiving pane are filed into that
  folder - move by default, copy with Ctrl held (Explorer parity on one volume is move; explicit
  modifier beats Explorer's volume-dependent guessing).
- The "before mentioned features" (WP-074 move/copy context actions) must also work while the
  receiving pane is open, so drag and menu are two routes to the same backend.
- A visible affordance returns the right panel to the normal Viewer.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-075" summary="Tab strip has no context menu yet but a clean insertion point; the Viewer content fn is swappable at one dispatch site; an inactive tab's cached inventory is readable without activation; egui 0.27 ships a payload drag-and-drop API unused in this repo; the video surface hides the frame the Viewer stops claiming it." updated_at="2026-08-15">

## Baseline project evidence (inspected, current working tree)

- **Tab strip**: `draw_media_document_tabs` (`product/src/ui.rs:7210-7338`). Tabs are
  `selectable_label`s whose response is only read for `.clicked()`
  (`product/src/ui.rs:7300-7309`); **no tab has a context menu today** (the complete
  `context_menu` site list in ui.rs excludes the strip). Deferred one-intent-per-frame application
  at `product/src/ui.rs:7325-7337` is where a new `open_in_right_panel` intent joins
  add/close/activate.
- **Panel swap point**: the Viewer is drawn by the self-contained
  `draw_media_viewer_panel(ui, rect, lane_id, request)` (`product/src/ui.rs:9547-9920`), invoked at
  exactly one dispatch site (`product/src/ui.rs:7154-7156`). Swapping in a receiving-pane draw fn
  for the same rect is a one-line dispatch change. The Library panel fn is already
  rect-parameterized (`product/src/ui.rs:8431-8438`).
- **Second folder's rows without activation**: per-tab runtime inventories
  `media_tab_runtime_inventories: HashMap<String, MediaTabRuntimeInventory>` with
  `Arc<Vec<String>> files` + `Arc<Vec<usize>> display` (`product/src/ui.rs:161-170`, `:1205-1206`,
  cap 8 at `:185`). Cloning the Arcs renders another tab's rows read-only with zero scanning;
  thumbnails are lane-agnostic and path-keyed (`product/src/ui.rs:9087-9094`, `:9257-9266`).
  **Caveat A**: a tab never activated this session has no cache entry (only
  `cache_active_media_tab_inventory` writes, `product/src/ui.rs:14134-14191`); **activation is
  destructive to global state** (`materialize_active_media_tab`, `product/src/ui.rs:14430-14604`)
  and must NOT be used to fill the pane. **Caveat B**: the cached display order is stale while the
  tab is inactive; `inventory_generation` (`product/src/ui.rs:164`) lets the pane label staleness.
- **Singleton grid state**: cursor, columns, scroll, display cache, search fields, and
  `media_scroll_to_cursor` are process-wide singletons wired to the active grid
  (`product/src/media_explorer.rs:268`, `:308`, `:314`; `product/src/ui.rs:1150`, `:1358`,
  `:8229-8308`, `:12389-12516`). The receiving pane must therefore be **mouse-only and
  non-focusable** in v1: no cursor, no keyboard routing, no search; the folder-navigator input
  interception (`product/src/ui.rs:12560-12631`) is the precedent if focus is ever added.
- **Drag infrastructure**: **none exists**. Exhaustive grep found zero uses of
  `egui::DragAndDrop` / `dnd_*` / `Sense::drag` on content (tiles are `Sense::click()` at
  `product/src/ui.rs:8853`); the only drags are the split gutter and strip handle. egui 0.27.2
  ships the payload API (`dnd_set_drag_payload` / `dnd_hover_payload` / `dnd_release_payload`).
- **Move/copy backend**: WP-074 provides destination-directed off-thread move/copy with per-file
  outcomes (over `media_fs::move_files` at `product/src/media_fs.rs:162-172` and the new
  `copy_files`); the legacy destination hard-wire at `product/src/ui.rs:7378` is already overridden
  there. A drop simply dispatches the same job with the pane's folder as destination.
- **Video surface constraint (WP-065)**: the Viewer's only claim is
  `product/src/ui.rs:10009-10017`; if `draw_media_viewer_panel` is not called, no claim is recorded
  and the reconciler hides the native child that frame (`product/src/ui.rs:18926-18948`), decoder
  still running. CODEX 8.2 requires folder/surface changes to decide playback explicitly, so
  opening the receiving pane must **explicitly stop playback** (pattern
  `product/src/ui.rs:9593-9596`) rather than let the surface vanish silently. The single-writer
  guard test (`product/src/ui.rs:17199-17235`) forbids any new `show_clipped`/`hide` call site.
- **Render-span guard**: drop-target hit testing and any validation drawn in the Media span must
  not touch the filesystem (`product/src/ui.rs:16944-16978`).

## Current external sources checked

- egui `DragAndDrop` payload API and demo (available in 0.27):
  <https://docs.rs/egui/latest/egui/struct.DragAndDrop.html>,
  <https://github.com/emilk/egui/blob/main/crates/egui_demo_lib/src/demo/drag_and_drop.rs>,
  <https://github.com/emilk/egui/discussions/3869>
- Dual-pane file-manager convention (Total Commander, Directory Opus): a fixed source/target pane
  pair with explicit move/copy semantics is the decades-hardened pattern for exactly this filing
  workflow; Explorer's own drag defaults (same-volume move, cross-volume copy) are documented but
  widely criticized as guess-dependent, supporting the explicit modifier rule.
- Windows File Explorer drag/modifier conventions (Ctrl = copy, Shift = move) for the modifier
  vocabulary.
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no additional transferable
  implementation evidence for egui-based dual-pane drag filing.

## Selected approach

- **Tab context menu**: add `.context_menu` on the tab response (`product/src/ui.rs:7300`) with
  Open in right panel (and Close as a convenience); a new deferred intent alongside
  activate/close/add sets `media_receiving_pane = Some(tab_id)` on the app.
- **Receiving pane draw fn**: same rect as the Viewer; header shows folder name, full path hover,
  staleness note when `inventory_generation` is behind, an item count, and a prominent close
  affordance returning to the Viewer. Body renders the target tab's cached `files`/`display` Arcs
  read-only (small tiles, no cursor, no captions beyond the WP-070 default), or - when no cache
  exists - a clearly labeled drop-target empty state naming the folder ("contents not loaded;
  drops still work"). No activation, no scan, no global-state writes.
- **Drag source**: tiles get `Sense::click_and_drag()`; a drag on a selected tile carries the whole
  canonical selection, on an unselected tile just that file (Explorer parity). The payload is the
  canonical path list; a compact drag overlay shows the count. Click/double-click/context behavior
  must remain byte-identical for non-drag interactions.
- **Drop target**: the pane is one `dnd`-style drop zone with a visible hover highlight; release
  dispatches the WP-074 worker with the pane folder as destination - **move by default, copy while
  Ctrl is held**, stated in the hover hint. Completion updates the source tab inventory (move) and
  the pane's cached rows, all through existing completion handlers.
- **Playback**: opening the receiving pane explicitly stops active playback with a status message;
  closing the pane restores the normal Viewer (selection preview returns via the existing paths).
- **Persistence**: the receiving-pane state is session-transient by design in v1 (not persisted in
  the tab record); receipts and the state snapshot report it so models can prove it.
- **Model route** (FACIAL-MODEL-001): `media_tabs --action open_receiving_pane / close_receiving_pane`
  receipt intents plus the WP-074 move/copy intents as the drag equivalent; the pane state appears
  in `media_tabs` receipts.

## Rejected options

- **Making the right pane a full second live grid** (own cursor/search/sort): requires
  de-singletonizing display cache, cursor, columns, scroll, and input routing
  (`product/src/ui.rs` sections above) - a large re-architecture serving keyboard workflows the
  operator did not ask for. Recorded as the v2 path if filing workflows demand it.
- **Activating the target tab to fill the pane**: `materialize_active_media_tab` overwrites lane 0,
  search, layout, and the display cache - it IS the left pane; activation would swap the panes out
  from under the drag.
- **A second scanning lane for the pane**: `CompareLane` supports it structurally, but every
  Media-side helper assumes lane 0; the read-only cached view covers the receiving use without
  touching scan arbitration.
- **OS drag-out / drag-in (files to and from Explorer)**: eframe 0.27 exposes dropped-files input,
  but OS-level drag interop opens foreign-window interactions and is not part of this request;
  in-app filing is the ask.
- **Explorer's volume-dependent move/copy guessing**: explicit default-move plus Ctrl-copy beats
  silent behavior differences between local and NAS destinations.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-075" summary="Silent video loss, drag misfires on click, stale-pane misdirection, cross-pane state bleed, and drop-to-wrong-folder are the failure surfaces; each has a control and a proof gate." updated_at="2026-08-15">

## Risks, failure scenarios, and controls

- **Playback vanishes silently when the pane opens.** Control: explicit stop with a status message
  per CODEX 8.2; a test asserts the stop reason is recorded and the single-writer guard test stays
  green; ui_snapshot proof that no orphan native child remains.
- **Drag threshold turns clicks into accidental micro-drags.** Control: egui's built-in drag
  threshold plus requiring pointer travel before payload activation; a regression test asserts
  click/double-click/context behavior unchanged with the new Sense.
- **Drop lands on the wrong folder** (pane switched mid-drag, tab closed underneath). Control: the
  payload pins the destination tab id AND its folder path at drag start; release validates the pane
  still shows that tab or rejects with a message; closing a tab clears any pane bound to it.
- **The pane shows stale rows and the operator files into a folder that moved on.** Control: the
  pane is honest about staleness (generation note); drops do not depend on shown rows - the
  destination is the folder path; completion refreshes the pane's cache entry.
- **Global grid state bleeds** (cursor/scroll/search react to pane interactions). Control: pane is
  mouse-only, never writes `media_explorer` fields; a test drives pane hover/drops and asserts
  cursor, scroll, columns, and search fields are untouched.
- **Move source==destination** (dragging a file onto its own folder's pane). Control: per-file
  same-folder no-op outcome from `move_files` surfaces as skipped, not success.
- **The one-intent-per-frame else-if chain drops the new intent.** Control: the new intent joins
  the chain with a test covering same-frame collisions (activate vs open-in-right-panel).
- **Receipt claims a pane state the render has not applied.** Control: pane receipts report state
  after command per CODEX 8.3, verified live.

## Verification

- Focused unit tests: drag payload composition (selected vs unselected origin), destination
  pinning and rejection, same-folder no-op, intent-chain collision, pane close on tab close,
  no-global-state-writes assertion.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic ui-inspect presets: receiving pane open with cached rows, with the not-loaded drop
  state, and with the drop-hover highlight; tab context menu open; direct PNG/layout inspection.
- Live background proof: open a second tab's folder in the right panel through the receipt intent,
  drive the WP-074 move intent as the drag equivalent onto that destination, verify per-file
  outcomes, source-tab inventory update, and pane refresh; ui_snapshot during active playback
  before and after pane open proving the explicit stop and no orphan video child; no foreground
  activation.
- Manual operator drag check recorded with exact steps (the pointer-drag gesture itself cannot be
  synthesized headlessly; the backend it dispatches is fully receipt-proven).
- Independent high-risk adversarial review per FACIAL-VERIFY-004 focused on drag misfire, video
  surface authority, and destination pinning.

</topic>
