---
file_id: REF-WP-072-VIEWER-METADATA-PANEL-USABILITY-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-072" summary="The Viewer info section is squeezed almost unusable: make it resizable in height, make notes scrollable, and keep dropdowns from rendering out of bounds." updated_at="2026-08-15">

## Operator request

Verbatim operator items folded into this packet (2026-08-15):

- "current info on the right panel gets squeezed of screen. this is kind of by design because i wanted
  the right panel to be a very large viewport for a photo and as large as possible. this squeezes the
  box with tags and notes all the way down and almost unusable. label dropdown gets cut off for
  example, long text in notes get cut off with no way to scroll for example."
- "can we make that section/panel resizeable in height? can we make the notes scrollable, can we smart
  layout the dropdowns in general so dropdowns do not render out of bound or can possible hide items
  out of the window/panel/app."

## Interpretation

- The Viewer panel's metadata band (filename, favorite star, label chips, Labels menu, tags field,
  notes field) must become operator-resizable in height, with the image viewport still taking all
  remaining space so the "as large as possible photo" design intent is preserved.
- Notes must scroll instead of silently clipping.
- Dropdown/popup surfaces reachable from the band must never render out of the window or lose items
  out of bounds; in-band trigger rows must not be clipped away either.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-072" summary="The band is a hard 142pt cap with no ScrollArea; the notes editor grows into a clip rect; egui 0.27.2 already clamps menu and combo popups to the window; an exact vertical drag-handle precedent exists with a full persistence chain." updated_at="2026-08-15">

## Baseline project evidence (inspected, current working tree)

- The squeeze is a hard cap, not a remainder: `meta_h = if fullscreen { 0.0 } else { 142.0_f32.min(rect.height() * 0.30) }`
  at `product/src/ui.rs:9561-9565` inside `draw_media_viewer_panel` (`product/src/ui.rs:9547-9920`).
  For any Viewer taller than ~473pt the band is pinned at 142pt forever; only very short panels get
  the 30 percent branch. The image rect takes the remainder (`product/src/ui.rs:9566-9573`).
- The band is a fixed-rect child UI with a hard clip and no scroll: `ui.child_ui(meta_rect, ...)` at
  `product/src/ui.rs:9687`, `set_clip_rect` at `product/src/ui.rs:9692-9693` (added to stop label
  chips escaping horizontally), and no `ScrollArea` anywhere in the block.
- The notes editor grows unbounded and is silently cut: `TextEdit::multiline(...).desired_rows(3)` at
  `product/src/ui.rs:9897-9900`; in egui 0.27 `desired_rows` is a minimum and the editor grows with
  content, so typed text expands past `meta_rect` into the clip. Computed at default 19pt font the
  stacked rows need roughly 185pt of the 138pt usable band, so the notes box bottom is already
  clipped at defaults; larger operator fonts roughly double the need.
- The label dropdown is an egui menu, not a ComboBox: `ui.menu_button("Labels ...")` at
  `product/src/ui.rs:9749-9784` with an internal `ScrollArea::vertical().max_height(100.0)` at
  `product/src/ui.rs:9761-9783`. Tags field at `product/src/ui.rs:9853-9879`; inline label creator at
  `product/src/ui.rs:9804-9819`; read-only status at `product/src/ui.rs:9908-9912`.
- Popups are not clipped by the band: menu and combo popups are separate `Area`s on the Foreground
  order, so `meta_rect`'s clip does not cut them; what IS clipped is in-band content (chips, tags,
  notes, creator row, status). Verified in the vendored egui 0.27.2: menus constrain to the screen
  rect (`egui-0.27.2/src/menu.rs:148-151`, `area.rs:185-189`) and ComboBox popups flip above/below
  and constrain (`combo_box.rs:271-278`, `popup.rs:348-351`). So the observed "label dropdown gets
  cut off" is expected to be the in-band trigger rows and content being clipped, and/or small-window
  scroll inside the 100pt menu; an inspector reproduction at operator-like font and window sizes is
  part of this packet's verification rather than assumed.
- Exact reusable precedent for a vertical resize handle: the folder-strip height handle at
  `product/src/ui.rs:8652-8680` (`allocate_exact_size(vec2(w, 7.0), Sense::click_and_drag())`,
  accumulates `drag_delta().y`, clamps `STRIP_MIN..STRIP_MAX`, `CursorIcon::ResizeVertical`), and the
  Library/Viewer gutter at `product/src/ui.rs:7081-7145` (double-click reset).
- Full persistence chain to copy: runtime fields `media_explorer.rs:258` / `:264`, bounds
  `media_explorer.rs:353-358`, DB settings keys in load/save `media_explorer.rs:391-392` / `:412-417`
  / `:435` / `:439`, debounced flush `product/src/ui.rs:13691-13695` and `product/src/ui.rs:14764`,
  per-tab viewport mirror `media_tabs.rs:158` / `:161` with defaults `:185` / `:188` and sanitize
  clamp `:407`, snapshot `product/src/ui.rs:14193` (writes `:14235`, `:14238`, `:14271`, `:14274`)
  and materialize `product/src/ui.rs:14430` (reads `:14483`, `:14486`), Settings sliders
  `product/src/ui.rs:11799-11812` / `:11835-11848`, split intent and state reporting
  `product/src/ui.rs:741-760`, `:5423-5429`, `:4678`, `:4706-4708`.
- Inspector coverage today: `media_labels_multi` (`product/src/ui_inspect.rs:585-708`) opens the
  Labels menu with a synthesized click and asserts unclipped rows; `media_grid` asserts the
  no-label affordance unclipped (`product/src/ui_inspect.rs:567-576`). All presets render at
  1280x800 (`product/src/ui_inspect.rs:23-24`). Gap: no preset renders the Viewer metadata band at a
  high font or tall window, which is exactly why the 142pt overflow has no guard today.
- Fullscreen/chrome-hidden skips the band entirely (`product/src/ui.rs:9557-9560`, `:9682-9684`);
  this packet must not change that.

## Current external sources checked

- egui issue "ComboBox clipped by window" documents parent-clip-rect popup clipping and its fix
  space: <https://github.com/emilk/egui/issues/825>
- egui issue "context menu can go outside the window when too far right":
  <https://github.com/emilk/egui/issues/1176>
- egui 0.27 `Ui` docs for `menu_button` / popup behavior and the later `Popup` unification PR
  (0.32-era, not available on 0.27, so in-repo mitigation must not depend on it):
  <https://docs.rs/egui/latest/egui/struct.Ui.html>, <https://github.com/emilk/egui/pull/5713>
- The vendored egui 0.27.2 source itself was inspected for constrain behavior (menu.rs, area.rs,
  combo_box.rs, popup.rs) — this is the authoritative basis over any web claim.
- Windows File Explorer's preview/details pane is the operator's reference: details panes are
  resizable and their text fields scroll; this establishes the expected interaction, not the code.
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no directly transferable
  implementation evidence for this item; the framework sources above are the field basis.

## Selected approach

- Replace the hard `142.0` cap with an operator-draggable horizontal divider between image and
  metadata band, copying the folder-strip handle interaction (`product/src/ui.rs:8652-8680`) and the
  gutter's double-click-to-reset (`product/src/ui.rs:7122-7125`, reset to the current compact
  default so the shipped look stays the default).
- Persist the band height through the existing chain: `MediaExplorerState` field + `META_MIN`/
  `META_MAX` bounds + `media_viewer_meta_height` settings key + `MediaTabViewport` field with
  sanitize clamp + snapshot/materialize touch points, so the height is per tab like split and strip.
- Clamp so neither surface collapses: minimum band height keeps filename + one field row reachable;
  maximum leaves a usable image viewport; fullscreen behavior unchanged (band stays absent).
- Wrap the band content below the filename row in `ScrollArea::vertical()` so overflow scrolls
  instead of clipping; the notes editor keeps growing naturally inside the scroll. Keep the existing
  horizontal clip fix.
- Keep the Labels menu as a menu (already window-constrained by egui) but raise its internal
  `max_height` toward the available screen space and keep the create action pinned reachable.
- Add a deterministic inspector preset rendering the Viewer metadata band at a high font and a tall
  window, asserting the notes field, Labels trigger, tags field, and create-label affordance render
  unclipped (or scrollably reachable), so the regression class is guarded like WP-070's caption
  overlap.
- Expose the band height to models: report it in the state snapshot next to split ratio, and accept
  it through the existing `media_tabs` action surface (mirroring `set_split`), so a no-context model
  can reproduce and verify the layout headlessly.

## Rejected options

- Making the metadata band a fixed larger constant: trades one squeeze for a smaller image viewport
  on every layout; the operator explicitly wants the photo as large as possible by default.
- Auto-sizing the band to its content each frame: the image rect would jump while typing notes,
  and content-driven layout thrash is exactly what the fixed-rect design avoids.
- Moving metadata into a floating window or overlay: breaks the flat two-panel design language and
  the deterministic inspector contract for panel-owned content.
- Upgrading egui to get the 0.32 `Popup` unification: an egui upgrade is a separate, risk-heavy
  packet (0.27 is pinned across theme, inspector, and video-surface code); nothing here needs it.
- A custom always-on-top popup implementation for labels: egui menus already constrain to the
  window; duplicating them adds an unmaintained popup layer for no measured gain.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-072" summary="Resize state must not leak across tabs or versions, scroll must not eat editor focus or drag, the video controls band must stay reachable, and inspector gates must cover high-font tall-window layouts." updated_at="2026-08-15">

## Risks, failure scenarios, and controls

- **The new height leaks across tabs or resets on restart.** Control: follow the split/strip chain
  exactly (runtime field, settings key, per-tab viewport mirror, sanitize clamp, snapshot and
  materialize touch points), with a two-tab test asserting independent heights across switching and
  restart, like the WP-068 two-tab sort proof.
- **Old persisted viewports without the new field fail to load.** Control: serde default + clamp in
  the `media_tabs` sanitizer (`media_tabs.rs:407` pattern), covered by a decode test with a legacy
  record.
- **The drag handle fights the image or the video controls band.** Control: the handle claims a
  dedicated 7pt lane like the strip handle; the video controls rect (`product/src/ui.rs:9973-9987`)
  is computed inside the image rect and must remain fully visible at both clamp extremes; inspector
  preset asserts the transport row is unclipped at min and max band height with video active.
- **ScrollArea swallows text-edit interactions or drag-scrolls while selecting text.** Control: use
  vertical-only scrolling; verify text selection, caret navigation, and the label creator row by the
  synthesized-click technique already used in `media_labels_multi`
  (`product/src/ui_inspect.rs:628-641`).
- **A dropdown still reads as cut off for the operator.** Control: reproduce first — the preset
  renders the exact band at 32pt font and a tall window before and after; if the menu itself (not
  the band) is the offender on small windows, raise the menu `max_height` and assert the create
  action stays reachable, which `product/src/ui.rs:9763-9766` already documents as the intent.
- **Resize state changes break the deterministic inspector.** Control: presets pin the band height
  explicitly; unchanged presets must produce byte-identical layout JSON.
- **The band minimum makes the notes field useless again at huge fonts.** Control: `META_MAX` is a
  fraction of panel height, not a constant; at 32pt the preset asserts at least the notes field's
  first three rows are visible inside the scroll viewport.

## Verification

- Focused unit tests: height clamp bounds; per-tab round trip through snapshot/materialize; legacy
  viewport decode without the field; settings key load/save.
- Full `cargo test --manifest-path product/Cargo.toml`.
- New deterministic ui-inspect preset(s): Viewer metadata at default font 1280x800, and at 32pt on a
  tall window; assertions on unclipped-or-scroll-reachable notes, tags, Labels trigger, create
  affordance; direct inspection of the emitted PNG and layout JSON.
- Live background proof: `facial.exe --background`, drive the band height through the model action
  surface, read it back from the state snapshot, capture `ui_snapshot`, and directly inspect it; no
  foreground activation.
- Independent high-risk adversarial review of persistence producer/consumer pairs and modal/popup
  layering per FACIAL-VERIFY-004.

</topic>
