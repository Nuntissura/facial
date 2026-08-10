---
file_id: REF-WP-058-MEDIA-VIEWER-INTERACTION-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-058" summary="Correct Settings, immersive preview use, configurable labels, and gated inline tile playback without sacrificing large-library responsiveness." updated_at="2026-08-09">

## Operator request

- Make the unified header-adjacent Settings window interactable.
- Replace the overexposed, saturated veil with a soft Gaussian-blurred view of the
  unchanged Media surface.
- Remove the obsolete Settings button from the Media toolbar; retain the unified
  Settings button beside Global Refresh.
- In Ctrl+F fullscreen, hide the complete metadata region: tags, notes, favorite/rating
  affordance, and color labels.
- Increase image and video use of the right panel in normal and fullscreen modes.
- In fullscreen, place playback controls in a transparent bottom bar that appears only
  when the video/control region is hovered.
- Keep a small play affordance on video thumbnails.
- Evaluate one-tile inline video playback with play/pause, scrub, and volume controls
  before retaining it; large local/NAS folders must remain snappy.
- Replace fixed presentation colors with configurable named label swatches. Store an
  opaque `#RRGGBB` value in the backend without showing the hex code in the operator UI.

</topic>

<topic id="current-evidence" status="complete" version="1" wp="WP-058" summary="Source inspection identifies the exact current defects and implementation constraints." updated_at="2026-08-09">

## Current implementation evidence

- `draw_soft_modal_backdrop` paints a 148-alpha copy of `theme::sheet`; it performs no
  blur. On the paper theme that is a white wash, explaining the reported exposure shift.
- The full-screen dismissible backdrop is an interactive `Order::Middle` Area competing
  with the Settings Window in the same movable-window order, explaining dead controls.
- The Media toolbar still renders a second Settings toggle even though WP-053 moved the
  canonical entry beside Global Refresh.
- The right preview permanently reserves up to 178 points for metadata. Video reserves
  another 190 points for controls, materially shrinking the media surface.
- Fullscreen currently hides app chrome but still renders the metadata block.
- Video tiles already paint a play glyph with no decode/player cost.
- The video runtime owns one hardware-efficient native LibVLC HWND. One player can move
  between the right preview and one active tile; multiple simultaneous tile decoders are
  not required.
- The exact operator NAS path `Z:\Video\4K Video\4K Video 21-08-2025` and drive `Z:`
  were not mounted in the current session. Exact NAS proof is therefore open, not
  replaced by local evidence.
- The product has no 0-5 rating field. The fullscreen correction treats the existing
  favorite star/rating-like affordance as part of the metadata region; it does not invent
  a new rating schema.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-058" summary="Current primary documentation and field implementations favor stable label identities, one lazy player, cached thumbnails, and explicit modal layering." updated_at="2026-08-09">

## Sources checked

- egui Area source and layer model: floating Areas back Windows; interactive Areas in
  `Order::Middle` participate in movable-window ordering.
  <https://docs.rs/egui/0.27.2/src/egui/containers/area.rs.html>
- egui Window source: Windows are Area-backed and movable-window order controls hit
  testing.
  <https://docs.rs/egui/0.27.2/src/egui/containers/window.rs.html>
- egui color-picker API: `color_edit_button_srgba` provides an in-app swatch picker.
  <https://docs.rs/egui/0.27.2/egui/widgets/color_picker/index.html>
- image crate Gaussian blur API, already available in Facial's dependency graph.
  <https://docs.rs/image/0.25/image/imageops/fn.blur.html>
- LibVLC media-player API: one output HWND per player; native-window rendering is the
  performance path.
  <https://videolan.videolan.me/vlc-3.0/libvlc__media__player_8h_source.html>
- LibVLC callback warning: custom-memory video rendering is considerably less efficient
  than a native output window and may disable hardware decoding.
  <https://videolan.videolan.me/vlc-3.0/libvlc__media__player_8h_source.html#l00382>
- Adobe Bridge label behavior: label names are metadata identity, and renaming can leave
  old items unmatched/white. Facial should not copy that failure.
  <https://helpx.adobe.com/bridge/desktop/organize-and-find-files/tag-and-find-files/label-and-rate-files.html>
- digiKam label workflow: ratings, pick labels, and renamed color labels remain distinct
  searchable concepts.
  <https://docs.digikam.org/en/left_sidebar/labels_view.html>
- PhotoPrism issue #703: on-demand parallel FFmpeg work can choke browsing and block
  thumbnail progress; cached/bounded work is required.
  <https://github.com/photoprism/photoprism/issues/703>
- Immich issue #8657: hover and full-view playback can both expose stutter despite
  hardware acceleration; playback must be measured rather than assumed smooth.
  <https://github.com/immich-app/immich/issues/8657>
- Hugging Face image-dataset documentation: structured labels/captions belong in durable
  metadata rather than visual-only UI state.
  <https://huggingface.co/docs/datasets/main/en/image_dataset>
- Civitai search was checked for comparable local gallery implementation guidance; no
  authoritative implementation source for this interaction was found.
- Reddit field reports on large media libraries repeatedly prefer cached thumbnail or
  focused/hover preview over many simultaneous decoders; these are usability signals,
  not implementation authority.
  <https://www.reddit.com/r/Windows11/comments/1uspuq8/media_player_for_windows_with_hover_preview/>
- X/Twitter search was checked; no implementation-grade source relevant to egui/LibVLC
  tile playback was found.

## Selected approach

- Capture the unobscured viewport before opening Settings, downsample it, apply Gaussian
  blur off the full-resolution hot path, and paint the blurred texture with no colored
  tint. Use a neutral low-alpha fallback only when screenshot capture is unavailable.
- Keep the dismissible backdrop below a Settings layer that is deterministically moved
  to the top every frame; suppress hidden Media actions while a modal is open.
- Remove the duplicate Media-toolbar Settings control.
- Make fullscreen preview media-first: zero metadata allocation and a compact hover-only
  playback bar. Reduce normal-mode fixed reservations and fitting margins.
- Store seven stable label IDs separately from editable names and colors. Existing
  per-file label values remain valid; renaming or recoloring never rewrites asset rows.
- Persist palette definitions as versioned backend data, validate opaque `#RRGGBB`, and
  expose definitions through structured command receipts. UI shows name + picker only.
- Reuse one LibVLC player for at most one active tile. Never autoplay on hover, never
  instantiate per-tile players, and stop/move the surface when it leaves the virtual
  viewport or selection changes.

## Rejected options

- Keep the white translucent veil: it is not blur and causes the reported washout.
- Add the new `backdrop-blur-egui` crate: its current release targets egui 0.34, requires
  Rust 1.92, and would force an unrelated GUI-stack migration from Facial's egui 0.27.
- Use LibVLC memory callbacks to upload frames as egui textures: official LibVLC guidance
  identifies this path as substantially less efficient and potentially hardware-decoder
  disabling.
- Start a player for every visible video or autoplay on hover: this scales decode and NAS
  reads with viewport contents and conflicts with Facial's responsiveness purpose.
- Store label name as the per-asset identity: renaming would orphan existing assignments.

</topic>

<topic id="performance-gate" status="active" version="1" wp="WP-058" summary="Inline tile playback is retained only when one-player measurements preserve browsing responsiveness." updated_at="2026-08-09">

## Gate

- Baseline and candidate use the same video-heavy folder, viewport, thumbnail cache
  state, scan mode, and diagnostic interval.
- Candidate permits exactly one active inline player and one visible control overlay.
- No full-collection work may enter the render frame; virtualized tile count stays
  bounded to visible rows.
- Retain inline playback only when median and p95 UI frame time do not regress by more
  than 10%, p95 interactive frame time remains below 16.7 ms on the local fixture, and
  scan first-batch/total time do not regress by more than 10% across three runs.
- Verify play, pause, scrub, volume, scroll-away, folder switch, fullscreen transition,
  and right-preview handoff without a second player or leaked audio.
- Run the exact same cold/warm mixed-load scenario on the named Z: folder when mounted.
  Until then, local/synthetic evidence may permit implementation but cannot close the NAS
  acceptance gate or WP-055/056/057.
- If the gate fails, retain the play badge and cached static video thumbnail, remove tile
  decoding, and offer cached storyboard/scrub thumbnails as the lower-cost remediation.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-058" summary="Failure scenarios have explicit controls and verification paths." updated_at="2026-08-09">

## Risks, failures, and controls

- Screenshot captures Settings itself: use a request/open state machine and open only
  after receiving the unobscured screenshot event; regression-test state transitions.
- Blur changes exposure/saturation: use no tint, opaque sRGB output, and compare sampled
  mean RGB before/after with only edge-local convolution differences permitted.
- Settings remains dead: assert the Settings layer is above the backdrop and add
  interaction tests for tabs, sliders, text edits, color picker, Close, Escape, and
  outside click with click-through prevention.
- Screenshot is unavailable: render a neutral low-alpha dark/gray fallback, keep Settings
  usable, and expose a diagnostic instead of blocking open.
- Fullscreen metadata leaks: inspector layout JSON must contain no tags, notes, favorite,
  or label controls in the fullscreen preset.
- Native video covers hover controls: constrain the HWND to the non-control portion while
  controls are visible; restore the full media rectangle when hidden.
- Inline playback scrolls out but keeps audio/video alive: bind target to stable path plus
  current visible generation and stop/hide on mismatch.
- Label rename breaks old media: store stable IDs on assets and definitions separately;
  tests rename/recolor definitions with unchanged asset rows.
- Invalid/duplicate label definitions cause ambiguity: validate ID, unique case-insensitive
  name, 32-character limit, and opaque six-digit hex; reject atomically.
- Palette save fails: retain the prior valid definitions, display a retryable save error,
  and never partially rewrite asset metadata.

</topic>
