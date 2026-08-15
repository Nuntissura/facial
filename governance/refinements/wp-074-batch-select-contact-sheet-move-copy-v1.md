---
file_id: REF-WP-074-BATCH-SELECT-CONTACT-SHEET-MOVE-COPY-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-074" summary="A toggleable select mode for batch selection, an auto-generated contact sheet of the selection with a names toggle that only appears during batch select, and context-menu move/copy/copy-into-new-folder with a navigable destination picker." updated_at="2026-08-15">

## Operator request

Verbatim operator items folded into this packet (2026-08-15):

- "togable option: select, this makes me select multiple files (selected output gets an auto
  generated contact sheet (toggle names for contact sheet feature shows only when batch select
  happens) (right click on the contact sheet or one of the selected files gives you the usual
  options but also a move to folder, copy to folder, copy in new folder (same parent folder on
  default but should be able to navigate folders)"

## Interpretation

- A **Select toggle** on the Media toolbar: while on, a plain click toggles a tile's membership in
  the selection (no Ctrl needed). Existing Ctrl/Shift/Space selection keeps working regardless of
  the toggle.
- When a batch selection exists (2 or more items), an **auto-generated contact sheet of exactly the
  selected items** becomes available as an in-app surface, rendered from the existing thumbnail
  cache. A **names toggle for the contact sheet appears only while a batch selection exists**.
- Right-click on the contact sheet or on any selected tile shows the usual context menu **plus**
  three new actions: **Move to folder**, **Copy to folder**, **Copy into new folder**. Destination
  is chosen in a navigable in-app folder picker; for "Copy into new folder" the picker defaults to
  the selection's parent folder and prompts for the new folder name.

## Recorded assumption (operator can override)

"Contact sheet" is read as an in-app selection view (a grid page showing only the selected items,
names toggleable), because the operator right-clicks it to act on the selection. An exported
contact-sheet PNG is additionally cheap because the montage compositor already exists, and is
included as a context action ("Export contact sheet...") rather than the primary surface. If the
operator instead wanted ONLY an exported image, the in-app view is the superset and the export
action still satisfies it.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-074" summary="Multi-select, context-menu dispatch, an off-thread move backend, a decouplable folder picker, and two montage compositors already exist; missing are a select mode, a copy backend, destination-directed move, and any contact-sheet surface." updated_at="2026-08-15">

## Baseline project evidence (inspected, current working tree)

- **Selection model.** Runtime selection is `selected_files: HashSet<usize>` +
  `selection_anchor` on the lane (`product/src/ui.rs:151-152`), display-space anchor
  `sel_anchor_display` (`product/src/media_explorer.rs:310`), grid cursor
  (`product/src/media_explorer.rs:268`). The one multi-select rule lives in
  `media_apply_tile_click` (`product/src/ui.rs:9486-9532`): shift = range, ctrl = toggle,
  plain = clear+select-one. Keyboard: Space toggles (`product/src/ui.rs:12684-12688`), Shift+arrow
  extends (`product/src/ui.rs:12653-12672`), Ctrl+A/Ctrl+Shift+A/Ctrl+I select-all/none/invert
  (`product/src/media_input.rs:394-396`). **No select-mode toggle exists anywhere.** Per-tab
  persistence of selection keys already exists (`product/src/media_tabs.rs:141-148`, snapshot
  `product/src/ui.rs:14193-14254`, restore `product/src/ui.rs:14670-14711`).
- **Context menu.** One builder `draw_media_context_menu` (`product/src/ui.rs:2466-2643`) reached
  from grid background, tile, and Viewer preview; complete action inventory confirmed - **no
  move-to-folder, copy-to-folder, or contact-sheet action exists**. Actions dispatch by setting
  fields on `CompareLaneRenderRequest` (`product/src/ui.rs:192-228`), consumed by
  `media_apply_extras` (`product/src/ui.rs:7342-7467`) then `apply_compare_lane_request`
  (`product/src/ui.rs:15521-15618`). `media_apply_extras` can cancel a shared verb before stage 2
  (`request.paste = false` at `product/src/ui.rs:7377`) - the exact hook the new actions ride.
- **Move backend exists and is off-thread**: cut+paste spawns a worker
  (`product/src/ui.rs:7376-7393`) calling `move_media_files_to_destination`
  (`product/src/ui.rs:16922-16938`) over `media_fs::move_files`
  (`product/src/media_fs.rs:162-172`), reporting `CompareWorkEvent::MediaMoveDone`
  (`product/src/ui.rs:3894-3940`). Its destination is hard-wired to the active lane folder at
  `product/src/ui.rs:7378` - the line a chosen destination must override.
- **No copy backend**: `media_fs.rs` has no copy helper; the only copy is a synchronous
  `fs::copy` loop on the render thread in `compare_lane_paste`
  (`product/src/ui.rs:15309-15328`). `media_fs::move_file` already shows the safe cross-volume
  pattern (copy + length verify + delete source, `product/src/media_fs.rs:141-156`) and
  `unique_target` collision naming (`product/src/media_fs.rs:60-81`).
- **Folder picker decouples cheaply**: `folder_picker.rs` already stages internally and commits
  only on "Use this folder" (`product/src/folder_picker.rs:47-60`, `:222-224`,
  `PickerEvent::Picked` `:33-38`); the lane coupling is a single call site at
  `product/src/ui.rs:19202-19207`. Widening it with a purpose tag makes it a general destination
  picker. The couch navigator remains the controller-first surface and is not required here.
- **New-folder machinery exists**: `media_fs::create_folder` (`product/src/media_fs.rs:109-117`)
  with Windows name validation (`product/src/media_fs.rs:19-40`) and an arming/modal flow
  (`product/src/ui.rs:7418-7420`, new-folder modal `product/src/ui.rs:7553`).
- **Contact-sheet composition precedents**: session-bound review montage
  (`product/src/review.rs:797-945`, 6x5 tiles, 256px, map sidecar) and the cleaner, path-list-based
  `anchor_montage` (`product/src/service.rs:2396-2527`) with its reusable `place` closure. Both are
  blocking (`image::open` per tile) so any export runs on a worker. **Tiles carry no text**:
  `ab_glyph` is currently a dev-dependency only (`product/Cargo.toml` dev-dependencies), so burning
  names onto an exported PNG requires promoting it (pure Rust, license-compatible) - the in-app
  sheet needs no rasterizer because egui draws captions.
- **In-app sheet rendering is nearly free**: tile painting is stateless and path-keyed
  (`paint_media_tile`, `product/src/ui.rs:9257-9266`), the thumbnail engine is lane-agnostic
  (`product/src/ui.rs:9087-9094`), and the WP-070 caption work (`set_names`, measured elision)
  already solved names-under-tiles.
- **Render-span guard**: no filesystem calls may run inside the Media draw span
  (`product/src/ui.rs:16944-16978`); destination validation and copy/move run in workers.

## Current external sources checked

- Windows File Explorer: multi-select semantics, "Move to folder..."/"Copy to folder..." legacy
  commands, and collision prompts are the operator-expected reference behavior.
- Adobe Bridge / Lightroom contact-sheet conventions (grid of selected assets, optional filename
  captions, export to image/PDF) establish the field meaning of "contact sheet":
  <https://helpx.adobe.com/lightroom-classic/help/print-module-options.html>
- image crate (already a dependency) for compositing; `ab_glyph` for pure-Rust glyph
  rasterization on exported sheets: <https://docs.rs/ab_glyph>
- egui 0.27 `SelectableLabel`/toggle-value patterns for the Select toggle (in-repo precedents
  suffice; no external UI dependency).
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no additional transferable
  implementation evidence for this item class.

## Selected approach

- **Select mode**: a toolbar toggle (and `A::ToggleSelectMode` action with a default chord chosen
  from free keys, bindings version bump per `media_input.rs` migration rules). While on,
  `media_apply_tile_click` treats plain click as ctrl-click; everything else (shift ranges, Space,
  Ctrl+A) is unchanged. The toggle state is per tab, persisted in the viewport snapshot, and
  reported in receipts.
- **Contact sheet (in-app)**: while 2+ items are selected, a "Sheet" affordance appears next to the
  selection count; it swaps the Library grid's display list for the selected keys only (a filtered
  display, same virtualized grid, same tiles), with a names toggle **visible only in this state**
  (rides the WP-070 caption machinery per tab without changing the tab's normal caption setting).
  Right-click inside the sheet is the normal context menu, so every existing and new action works
  on the selection. Leaving the sheet restores the normal display order untouched.
- **Export contact sheet...** context action composes a PNG on a worker from the selected paths
  using the `anchor_montage`-style compositor, honoring the names toggle via promoted `ab_glyph`,
  written next to the selection's parent (collision-safe via `unique_target`) and reported with a
  receipt naming the exact output path.
- **Move to folder... / Copy to folder...**: context actions open the decoupled folder picker
  (purpose-tagged); on pick, a worker runs `media_fs::move_files` or the new
  `media_fs::copy_files` (cross-volume-safe, length-verified, `unique_target` collisions,
  per-file outcomes) and reports a MediaMoveDone-style completion that updates the source tab's
  inventory (move) or leaves it untouched (copy).
- **Copy into new folder...**: same picker opened at the selection's parent folder by default,
  with a name prompt validated by `media_fs::validate_name`; creates the folder then copies into
  it as one worker job with per-file outcomes.
- **Model routes** (FACIAL-MODEL-001): receipt-backed intents for select-mode toggle, selection
  set/get (extending the existing `media_select`), move-to/copy-to with explicit destination paths,
  and sheet-export; all usable while the GUI holds the media database lock or rejecting with the
  documented reason.

## Rejected options

- **Replacing plain-click semantics globally** (every click toggles): breaks single-click preview
  flow the operator uses daily; the explicit mode keeps both.
- **A separate selection-tray window**: violates the no-external-window contract and the flat
  two-panel design; the in-grid filtered sheet reuses everything.
- **Synchronous copy on the render thread** (the current Compare paste pattern): blocks the UI on
  NAS; explicitly not copied; the Compare paste path is left as-is but noted for a future parity
  fix.
- **OS-native Move/Copy dialogs (IFileOperation UI)**: shell progress dialogs are foreign windows
  and break the model-quiet operation contract; in-app workers with receipts are the house
  pattern.
- **Composing the in-app sheet as a rendered PNG texture**: throws away virtualization and
  thumbnail reuse; the grid already renders arbitrary display lists.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-074" summary="Selection/display aliasing, canonical-set-vs-visible-subset bugs, copy integrity on NAS, collision policy, and picker misuse are the failure surfaces; controls and proof gates are explicit." updated_at="2026-08-15">

## Risks, failure scenarios, and controls

- **Actions target the visible subset instead of the canonical selection** (GLOBAL-SOT-027/031).
  Control: move/copy/export consume the lane's `selected_files` set resolved to canonical paths at
  dispatch time, never the rendered display slice; a test selects across a filtered view and
  proves all selected files are acted on.
- **The sheet view desyncs from the selection** (item deleted/moved while the sheet is open).
  Control: the sheet display is rebuilt from the live selection each frame like the normal display
  cache, and completion handlers update selection membership; a test moves a file out and asserts
  the sheet drops exactly that row.
- **Copy silently truncates on flaky NAS.** Control: `copy_files` verifies copied length before
  reporting success (pattern `product/src/media_fs.rs:141-156`); per-file outcomes; partial
  success reported as partial.
- **Collision policy surprises** (same name at destination). Control: `unique_target` naming with
  the outcome naming the final path per file; never overwrite.
- **Move into the same folder or into a selected item's own subtree.** Control: same-folder is a
  per-file no-op with an explicit outcome; destination-inside-source guards for folders are moot
  (files only) but destination==source-parent is reported honestly for moves.
- **Select-mode toggle state confuses tabs.** Control: per-tab state, persisted and reported;
  two-tab test proves independence.
- **Names toggle leaks outside batch select.** Control: visibility condition is selection >= 2 in
  sheet state; inspector preset asserts absence otherwise.
- **Picker commit changes the active tab folder** (the old hard-wired sink). Control: purpose
  tag branches at the single call site (`product/src/ui.rs:19202-19207`); a regression test
  asserts the active tab folder and scan generation are untouched by a destination pick
  (FACIAL-FOLDER-001 analog).
- **Export blocks the UI or writes into the render span.** Control: worker-only composition and
  writes; render-span guard test stays green.

## Verification

- Focused unit tests: select-mode click semantics, per-tab toggle persistence, sheet display
  filtering, copy_files length verification and collisions, canonical-selection dispatch, picker
  purpose branching.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic ui-inspect presets: select mode on with a batch selection (count + Sheet + names
  toggle visible), the sheet view with names on and off, the context menu showing the three new
  actions, and the destination picker over the Media surface; direct PNG/layout inspection.
- Live background proof: build a multi-file selection through receipts, move to a scratch folder,
  copy to a second, copy-into-new-folder with a validated name; verify per-file outcomes, source
  inventory updates without rescan, and export a named contact sheet PNG whose file is directly
  opened and inspected.
- Independent high-risk adversarial review per FACIAL-VERIFY-004 focused on
  selection-to-action aliasing and NAS copy integrity.

</topic>
