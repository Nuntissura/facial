---
file_id: refinement-wp-050-media-front-correction-v1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="complete" version="1" wp="WP-050" summary="Correct the Media front surface from operator runtime feedback." updated_at="2026-08-09">

## Operator request

- Keep Media as the front of Facial and preserve the two-page book behavior.
- Default the left thumbnail view to 500-point thumbnails.
- Hide thumbnail filenames by default and provide a persistent show-names toggle.
- Show the selected folder and all supported image/video media within it, including its tree by default.
- Make the folder-list/thumbnail divider and thumbnail/preview divider minimal, short, and easy to grab.
- Make Ctrl+F a borderless application-fullscreen mode that retains folder list plus thumbnails on the left and full preview on the right; Escape restores normal mode.
- Replace the cramped Media settings overlay with a large, readable, resizable in-app popup window.
- Make very large folders load thumbnails progressively and scroll quickly; validate with `D:\Projects\Image_sourcing\lora_avatar_test_0002`.
- Visually inspect all tabs and affected Media states; extend the built-in inspector when a state cannot otherwise be inspected.
- Update the internal user/model manual.
- Make scrollbars and their handles much larger so they do not require fine motor control; keep them hidden at rest and reveal/hide them snappily on hover, scrolling, or grab.
- Remove black borders around thumbnails, the full right preview, and tags/notes; retain a clear tags/notes input affordance through a slightly darker background.

</topic>

<topic id="spec-anchors-and-scope" status="complete" version="1" wp="WP-050" summary="Spec anchors, scope edges, and non-goals." updated_at="2026-08-09">

## Spec anchors

- `specs/app-spec.md` section 15, especially WP-043 thumbnail engine and WP-044 book explorer contracts.
- `topology.yaml` `media_browser` and `gui_inspector` surfaces.
- `CODEX.md` sections 6, 7.1, and 9.

## Scope edges

- Product: `product/src/media_explorer.rs`, `media_thumbs.rs`, `media_input.rs`, `ui.rs`, `ui_inspect.rs`, targeted tests, and `product/docs/MANUAL.md`.
- Governance/spec sync: WP-050, taskboard, spec section 15, topology Media/inspector contract.

## Non-goals

- Do not replace the existing Rust-native thumbnail engine or book layout.
- Do not add external thumbnail processes or foreground helper windows.
- Do not change Compare behavior except shared defaults required for supported media filtering.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-050" summary="Current field and source research supporting the implementation." updated_at="2026-08-09">

## Sources checked

- egui 0.27.2 source, `ScrollArea::show_rows` and `show_viewport`: long scroll content must render only the visible range; `show` warns against laying out very long content every frame. https://github.com/emilk/egui/blob/0.27.2/crates/egui/src/containers/scroll_area.rs
- egui 0.27.2 source, resizable `Window`: readable in-app popup without creating an external OS window. https://github.com/emilk/egui/blob/0.27.2/crates/egui/src/containers/window.rs
- egui 0.27.2 source, `ViewportCommand::Fullscreen(bool)`: native borderless fullscreen path already supported by pinned dependencies. https://github.com/emilk/egui/blob/0.27.2/crates/egui/src/viewport.rs
- egui repository performance guidance and cache module: avoid rebuilding expensive values every immediate-mode frame. https://github.com/emilk/egui and https://docs.rs/egui/latest/egui/cache/index.html
- Hugging Face Dataset Viewer: very large media datasets are exposed in bounded precomputed slices rather than loaded as one UI response. https://huggingface.co/docs/dataset-viewer/rows
- Civitai open-source repository and image API: adjacent large-gallery implementation/reference surface checked; no directly reusable Rust/egui component. https://github.com/civitai/civitai
- Oculante and Simp Rust image-viewer field implementations, plus current egui issue/discussion/release surfaces: reinforced background decode, retained thumbnail cache, and bounded visible work; no compatible drop-in grid retained. https://github.com/woelper/oculante and https://github.com/Kl4rry/simp
- Reddit field reports around Oculante/egui image viewers and large scraped-image workflows were checked for observed workflow pain; they were treated as discovery input, not implementation authority.

## Relevant local findings

- The supplied folder contains 54,552 files across 193 directories: 51,486 supported images in total (JPEG/JPG, PNG, WebP, and one GIF), plus non-media artifacts.
- Current Media rendering sorts all media, clones the entire file vector, and re-enumerates child folders on every frame.
- Current scan publishes nothing until the complete recursive walk and sort finish.
- Current thumbnail workers never cancel stale jobs that were once visible, so rapid scrolling can make old viewport decodes delay the current viewport.

## Selected approach

- Cache display ordering until scan/search/sort/metadata/stat/semantic generations change.
- Clone/request only visible and overscan paths; retain virtual geometry for the full set.
- Emit bounded scan batches before final sorted completion.
- Treat queued jobs from earlier viewport generations as stale regardless of their original priority, and enqueue current visible work first.
- Use the pinned egui resizable `Window` and root viewport fullscreen command.
- Use egui's pinned floating scrollbar style with a 24-point grab region, 64-point minimum handle, zero dormant opacity, and a 45 ms UI animation.

## Rejected options

- New virtual-grid/docking dependencies: unnecessary version coupling for the existing fixed book surface.
- Decode every file up front: blocks large-folder interaction and increases memory pressure.
- Native secondary settings viewport: adds focus/window-management complexity and conflicts with quiet in-app operation.

</topic>

<topic id="red-team-and-verification" status="complete" version="1" wp="WP-050" summary="Failure scenarios, controls, and proof gates." updated_at="2026-08-09">

## Risks and controls

- Progressive chunks could retarget selection while final sort lands; disable automatic preview retargeting after the first chunk and clamp by canonical file index at completion.
- Cache invalidation could show stale order; key it to scan identity/file count plus sort, query, metadata, stats, and semantic generations, with unit tests.
- Fast-scroll cancellation could starve thumbnails; only stale queued jobs are skipped, current generation remains visible-first, and disk cache persists completed work.
- Fullscreen could trap the operator; Escape always sends `Fullscreen(false)` and restores chrome, and inspector captures the chrome-hidden book state.
- Hidden captions could leave dead caption space; grid math must remove caption height when names are off, covered by geometry tests and snapshots.
- Settings popup could exceed small screens; cap it to the available viewport, keep it resizable/scrollable, and inspect it at 1280x800.
- Large always-visible scrollbars would fight the minimal design; use floating zero-dormant-opacity bars and prove both rest-hidden and hover-visible states.

## Verification

- Targeted and full Cargo test suites.
- Large-folder scan/progressive-result/per-frame-order benchmark using the supplied folder.
- `ui-inspect` captures two-panel, fullscreen/chrome-hidden, names-on, settings-popup, and all standard tabs.
- Rasterized visual review of every new Media preset and layout-JSON checks for off-canvas or overlapping state.
- Package through the canonical release script and pass the executable-layout validator.

</topic>
