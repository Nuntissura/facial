---
file_id: REF-WP-052-VIDEO-THUMBNAIL-PLAYBACK-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-052" summary="Fast video thumbnails and a complete VLC-backed right-panel player." updated_at="2026-08-09">

## Operator request

- Show real video thumbnails without weakening large-folder snappiness.
- Embed video playback in the right panel with play/pause, scrubbing, audio-track
  selection, and subtitle-track selection.
- Provide explicit Open in VLC and Windows app-selector paths.
- Keep the work controller-usable and model-inspectable.
- Do not emit audible test playback into the operator's headset.
- Remain stable on large external video folders with Unicode filenames.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-052" summary="Official LibVLC API and current local runtime evidence." updated_at="2026-08-09">

## Sources checked

- VideoLAN LibVLC 3.0 media-player API: native HWND embedding, time get/set,
  play/pause/stop, track-description ownership, and `libvlc_video_take_snapshot`.
  <https://videolan.videolan.me/vlc-3.0/group__libvlc__media__player.html>
- VideoLAN LibVLC 3.0 audio API: volume and audio-track get/set.
  <https://videolan.videolan.me/vlc-3.0/group__libvlc__audio.html>
- Installed runtime inspected: VLC 3.0.23 includes `libvlc.dll`, `libvlccore.dll`,
  `vlc.exe`, and plugins. FFmpeg 8.1.1 is available through PATH.
- Existing Facial thumbnail engine inspected: priority queues, generation cancellation,
  sharded JPEG cache, failure memoization, and bounded UI texture upload are reusable.
- Exact runtime failure inspected on `D:\Tumblr\kpop vids`: metadata key generation
  sliced a UTF-8 path at the workspace-root byte length before proving the prefix,
  panicking inside the Korean character `우`.

## Selected approach

- Keep image workers unchanged and add exactly one dedicated video worker.
- Spawn FFmpeg directly with no shell or visible window, seek near the start, cap each
  attempt at five seconds, and cache the extracted frame through the existing key path.
- Dynamically load the operator-installed LibVLC only after explicit Play. Render into
  a Win32 child HWND inside the right preview; use LibVLC APIs for all transport/tracks.
- Hide the child for overlays and destroy/release player resources deterministically.
- Add a receipt-backed `media_video_control` intent for independent model operation.
- Pair the deterministic egui inspector with a receipt-backed LibVLC-native frame export
  so a model can inspect the real child surface without desktop automation.
- Guard workspace-prefix slicing with `str::get` before taking the remainder, and retain
  external Unicode paths as lowercase external keys when they are outside the workspace.
- Pass LibVLC `--no-audio` whenever `FACIAL_TEST_SILENT=1`; never start playback for
  folder-scan, thumbnail, or inspector validation.

## Rejected options

- Decoding videos on every image worker: rejected because a visible video run could
  starve image thumbnails.
- Loading LibVLC during scan or selection: rejected because it adds avoidable startup
  and scrolling cost.
- Copying every decoded video frame into an egui texture: rejected for this Windows
  build because native VLC output avoids continuous CPU copies and texture uploads.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-052" summary="Failure scenarios and enforced controls." updated_at="2026-08-09">

## Risks, controls, and verification

- Missing FFmpeg: retain film-strip fallback; expose `FACIAL_FFMPEG`; memoize failure.
- Broken/remote/huge video stalls decode: five-second attempt cap and one isolated worker.
- VLC missing or incompatible: lazy local error; loader test resolves every required symbol.
- Native child covers popups: hide for folder/settings/favorites/picker overlays and when
  leaving Media; visual test the live player and overlay transitions.
- Wrong track ID or unseekable format: preserve playback; LibVLC returns local failure;
  control receipts remain attributable.
- External app unexpectedly steals focus: external launch exists only behind explicit
  Open in VLC/Open file/Choose app actions, never scan, selection, tests, or inspector.
- Resource leak/crash on path changes: stop and release player before replacement; destroy
  child HWND and release LibVLC instance on drop.
- Native child cannot appear in headless egui SVG: `capture_frame` calls LibVLC's own
  bounded snapshot API and reports a workspace-relocatable PNG in the applied receipt.
- External Unicode path panics before thumbnails settle: boundary-check the candidate
  prefix and regression-test the exact Korean filename shape.
- Automated proof produces unexpected sound: silent diagnostic mode disables LibVLC
  audio at instance construction; scanning and video thumbnails do not load LibVLC.

## Acceptance proof

- A generated real video produces a cached 256×144 thumbnail.
- Installed LibVLC loads every required symbol.
- A live GUI capture shows moving native video, an advancing timeline, two audio tracks,
  and a subtitle track with rendered subtitle text.
- Facial's own `capture_frame` action produces a non-empty decoded-frame PNG and a
  structured applied receipt without Computer Use or foreground input injection.
- The exact 664-video folder scans in 17 ms with zero directory errors; an isolated
  minimized GUI remains responsive while cached thumbnails are generated and stderr
  remains empty. No playback is started.
- Full Cargo suite, headless visual inspector, release packaging, and canonical executable
  checks pass.

</topic>
