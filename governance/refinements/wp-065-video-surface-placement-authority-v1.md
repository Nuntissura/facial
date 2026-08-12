---
file_id: REF-WP-065-VIDEO-SURFACE-PLACEMENT-AUTHORITY-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-065" summary="Embedded playback intermittently produces audio with no picture, leaves the last played frame flickering across the application, and breaks again after switching folders." updated_at="2026-08-12">

## Operator request

Verbatim operator report, recorded across several test sessions:

- "video playback still does not work (audio does play)"
- "not sure how or why, but video playback does work. i do not know the reason why. it could
  be because i let the folder load in for a very long time. or i went fullscreen tried to play
  the video there (which worked) and exited out of fullscreen and the videos worked."
- "i think it is a loading issue. because loading another folder gives back the same old
  problem: audio works but no image. full screen playback also only gave audio and going back
  did not solve anything."
- "in a new sessions video playback does work again. but the app flickers the lasts played
  video all over the screen/app. i did test thumbnail playback and then right panel playback.
  and it now flickers all over the place."
- "switching folder did get rid of the flickering but broke playback again."
- "playback scrubbers are to short" — **moved to WP-070**; this packet is playback correctness
  only.

## Interpretation

The operator's report is internally consistent and describes one defect class, not three:
decoding always works (audio proves it), while the **native video child window is placed,
shown, hidden, and clipped as an emergent side effect of whichever draw function happens to
run in a given frame**. Every reported symptom — audio-only, last-frame flicker across the
application, and the folder-switch regression — follows from that single missing authority.
The intermittency the operator could not explain is exactly what an emergent placement rule
produces.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-065" summary="The child HWND is created hidden at 16x16 and is only ever sized or shown from inside render code, with two owners that can both decline to place it, no clipping, a cached visibility flag, no invalidation of vacated pixels, and no folder-change teardown." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### Attachment is correct; placement is not

- Parent HWND is bound once from the exact eframe handle at
  `product/src/ui.rs:930-939`, validated and made immutable after load at
  `product/src/video_player.rs:323-350`.
- Attachment order is correct and rules out the classic late-`set_hwnd` failure:
  `ensure_window` (`product/src/video_player.rs:1371`) → `release_player`
  (`:1373`) → `media_new_path` (`:1379`) → `player_new_from_media` (`:1399`) →
  **`player_set_hwnd` (`:1406`)** → identity verification `player_get_hwnd`
  (`:1408-1413`) → **`player_play` (`:1415`)**.
- Video output module default is `wingdi`, chosen deliberately because it composes into the
  host HWND rather than using a Direct3D overlay
  (`product/src/video_player.rs:940-961`), passed as a libvlc **instance** argument at
  `product/src/video_player.rs:1303-1305` and mirrored in the prewarm instance at
  `product/src/video_player.rs:1240-1242`. Override is `FACIAL_VLC_VOUT`
  (`product/src/video_player.rs:949`).

### The child is born hidden and tiny, and only render code ever fixes that

- `ensure_window` (`product/src/video_player.rs:1663-1725`) creates the child with
  `CreateWindowExW(0, "STATIC", NULL, WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 16, 16, parent, …)`
  at `product/src/video_player.rs:1696-1712` — **no `WS_VISIBLE`**, geometry literally
  `x=0, y=0, w=16, h=16`. `WS_CLIPCHILDREN` is forced onto the parent at
  `product/src/video_player.rs:1681-1693`.
- `player_play` runs at `product/src/video_player.rs:1415` while the child is still 16×16 and
  hidden.
- The **only** code that sizes or shows it is `VlcRuntime::show_at`
  (`product/src/video_player.rs:1727-1763`), called from exactly two render sites:
  `product/src/ui.rs:7634` (Library tile) and `product/src/ui.rs:8421` (Viewer panel).
- **Therefore: any frame in which neither owner draws leaves LibVLC decoding into a hidden
  16×16 window — audio with no picture.** This is the primary mechanism.

### Both owners can decline to place the surface in the same frame

- Ownership is derived, not held: `video_surface_owner`
  (`product/src/ui.rs:585-593`) returns `"library"` when
  `media_inline_video_path == active_path`, else `"viewer"`.
- The Library tile calls `show_at` based only on `media_inline_video_path == tile path`
  (`product/src/ui.rs:7380-7382`, `product/src/ui.rs:7632-7637`) — it does **not** check
  `video_player.active_path()`.
- The Viewer **early-returns without calling either `show_at` or `hide`** when the Library is
  nominally the owner: `product/src/ui.rs:8352-8381` paints a static thumbnail and the literal
  text `"Playing in thumbnail"` and returns. Comment at `product/src/ui.rs:8353-8355`: *"The
  grid was rendered first and owns the single native child this frame."*
- **If `media_inline_video_path` is `Some(p)` but that tile is not rendered this frame —
  virtualized out of the grid, filtered out by a query, or the display order has not been
  rebuilt yet — neither owner places the surface.** The player keeps running at its last
  bounds, or at 16×16 if it never showed.
- An owner change performs no surface work at all: `product/src/ui.rs:4484-4501` only mutates
  `media_inline_video_path`. There is no `hide`, no detach, no `set_hwnd(null)`, no
  `DestroyWindow`. `media_playback_lease` (`product/src/ui.rs:789`,
  `product/src/media_io.rs:360-374`) is an I/O-priority lease with **zero** effect on the HWND.

### Flicker of the last played frame across the application

- `show_at` (`product/src/video_player.rs:1727-1763`) calls `SetWindowPos` with
  `SWP_NOACTIVATE | SWP_SHOWWINDOW`, a NULL `hWndInsertAfter` and **without `SWP_NOZORDER`**
  (`product/src/video_player.rs:1736-1746`) — the child is raised to the top of Z on every
  bounds change.
- **No clipping.** `show_at` receives the raw tile rect (`product/src/ui.rs:7614-7620` →
  `product/src/ui.rs:7634`) with no intersection against the Library panel rect or the grid's
  scroll clip rect. A partially scrolled tile therefore places a full-size top-of-Z child
  overlapping the toolbar and the Viewer. This is the direct mechanism for "flickers the last
  played video all over the screen/app".
- `hide()` (`product/src/video_player.rs:1765-1770`) is guarded by the **cached**
  `surface_visible` flag rather than a live `IsWindowVisible` query; divergence makes `hide()`
  a silent no-op that leaves a visible child.
- The child is hidden, never moved offscreen and never destroyed on stop. `DestroyWindow`
  exists only in `Drop` (`product/src/video_player.rs:1789-1799`) and the creation-failure path
  (`:1718`); `VideoPlayer.runtime` is set at `product/src/video_player.rs:385` and never reset
  to `None`, so the child HWND lives for the whole process after the first Play.
- **No invalidation of the vacated region.** There is no `InvalidateRect`, `RedrawWindow`, or
  `UpdateWindow` anywhere in `product/src` (grep-verified). After `SW_HIDE`, the uncovered
  parent pixels depend entirely on egui repainting that region.
- A deliberate keep-alive window holds the player with **neither `show_at` nor `hide`** for up
  to 120 s while a scan reconciles and 10 s otherwise
  (`product/src/ui.rs:7518-7545`, `keep_inline_video_awaiting` at
  `product/src/ui.rs:13969-13983`). During that window the child stays visible at its last
  bounds while nothing draws it.
- `last_surface_bounds` is never cleared by `hide()`/`stop()` (only by `Drop`,
  `product/src/video_player.rs:1798`); the `|| !visible` term at
  `product/src/video_player.rs:1735` is what re-shows after a hide at unchanged bounds.

### The folder-switch regression

- `start_compare_scan_internal` (`product/src/ui.rs:1377-1456+`) bumps `scan_id`, clears
  inventory, textures and selection, bumps generations, and cancels workers — but **never
  touches `video_player`, `media_inline_video_path`, `media_inline_video_requested_at`, or
  `media_inline_video_pending_target`** (grep-verified across the function).
- `media_set_folder` (`product/src/ui.rs:4080-4097`), the folder-picker commit
  (`product/src/ui.rs:15474-15479`), and parent/child navigation
  (`product/src/ui.rs:10642-10651`, `product/src/ui.rs:10768-10779`) all only request a scan.
- The pending-placement machinery is cleared on any scan-id mismatch
  (`product/src/ui.rs:5708-5713`), so after a folder change the cursor never moves to the tile,
  the tile never renders, `show_at` never runs — **audio continues with no picture**. This is
  exactly the operator's "switching folder … broke playback again".
- The only stop path for a media-runtime change is `cancel_active_media_runtime`
  (`product/src/ui.rs:11884-11920`), called only from `materialize_active_media_tab`
  (`product/src/ui.rs:11923`) — i.e. **tab** switching, not **folder** switching. This is
  consistent with the operator observing that switching folders stopped the flicker in one
  session and broke playback in another: which symptom appears depends on whether the tile
  happens to be re-rendered.

### Why fullscreen behaved differently

- Ctrl+F is `MediaAction::ToggleChromeHide` (`product/src/media_input.rs:137`), toggled at
  `product/src/ui.rs:10509-10515` and `product/src/ui.rs:10615-10621`.
- The fullscreen and windowed Viewer paths call **the identical `show_at` line**
  (`product/src/ui.rs:8419-8424`). The differences are layout only: metadata block skipped
  (`product/src/ui.rs:8111-8113`), controls band `0` unless hovered versus always
  `min(154, 0.34·h)` (`product/src/ui.rs:8388-8395`), and `render_ui` skipping the header and
  status panels (`product/src/ui.rs:15414-15435`), which changes the client-area origin of
  every subsequent rect.
- `physical_surface_bounds` (`product/src/video_player.rs:861-881`) converts egui viewport
  points to parent-client pixels with no offset compensation, and clamps degenerate rects to
  ≥1 px rather than erroring.
- NOT_DETERMINED from source: why fullscreen produced picture while windowed did not. No
  divergent `set_hwnd`, vout, or surface-creation path exists. The evidenced candidates are
  (a) the windowed frame taking the Library-owns-the-child early return at
  `product/src/ui.rs:8352-8381`, and (b) a point-versus-client-pixel origin mismatch between
  the two panel layouts. **The selected approach removes both without needing to decide which
  applied**, and the diagnostics added by this packet make the answer directly observable.

### Diagnostics that already exist, and the gap

- `NativeSurfaceDiagnostics` (`product/src/video_player.rs:157-175`, populated
  `:1341-1363`) already reports `child_visible` (live `IsWindowVisible`), `target_bounds_px`,
  live `child_bounds_px`, and `libvlc_hwnd_matches`.
- `ui_snapshot` is already a bounds-drift detector: `current_surface_capture_region`
  (`product/src/ui.rs:610-660`, used at `product/src/ui.rs:11457-11530`) errors when
  `target_bounds_px != child_bounds_px` (`product/src/ui.rs:631-635`) and returns
  `Ok(None)` when `child_visible == false` (`product/src/ui.rs:614-616`).
- Trace phases exist for load, instance, media, set_hwnd and play
  (`product/src/video_player.rs:1284-1419`), plus `FACIAL_PLAYBACK_TRACE`
  (`product/src/video_player.rs:17`).
- **Gap: there is no trace call in `show_at`, `hide`, or `stop`** — the entire surface
  placement lifecycle, which is where every reported defect lives, is untraced.
- **Test gap:** no test anywhere calls `show_at`, `hide`, `VlcRuntime::stop`, or
  `ensure_window`; `physical_surface_bounds` is tested only in isolation
  (`product/src/video_player.rs:1871`, `:1880`). No test covers the Library↔Viewer frame
  ordering, the `product/src/ui.rs:8352-8381` early return, or the
  `product/src/ui.rs:7518-7545` keep-alive window.
- Stale governance note: `governance/refinements/wp-060-embedded-video-panel-terminology-v1.md:31`
  claims the parent HWND is discovered via `GetActiveWindow`. That is **incorrect against
  current code** — the only `GetActiveWindow` call is in `open_with_dialog`
  (`product/src/video_player.rs:1066`). This packet corrects that record.

## Current external sources checked

- VideoLAN LibVLC media player API: `libvlc_media_player_set_hwnd` is the supported Win32
  drawable attachment and `get_hwnd` its diagnostic inverse:
  <https://videolan.videolan.me/vlc-3.0/group__libvlc__media__player.html>
- LibVLC header source confirming the drawable contract:
  <https://videolan.videolan.me/vlc-3.0/libvlc__media__player_8h_source.html>
- Vlc.DotNet "black screen, audio ok" issue: the field-recurring cause is the drawable being
  attached late or the output never binding to the supplied handle:
  <https://github.com/ZeBobo5/Vlc.DotNet/issues/475>
- VLC Win32 video output source, showing the vout binds to the supplied HWND and follows its
  size/visibility:
  <https://github.com/videolan/vlc/blob/master/modules/video_output/win32/direct3d11.cpp>
- Microsoft `SetWindowPos` documentation for `SWP_NOZORDER`, `SWP_NOACTIVATE`, and
  `SWP_SHOWWINDOW` semantics, and `hWndInsertAfter` NULL meaning top-of-Z:
  <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowpos>
- Microsoft `WS_CLIPCHILDREN` / `WS_CLIPSIBLINGS` documentation and the requirement to
  invalidate uncovered regions after hiding a child:
  <https://learn.microsoft.com/en-us/windows/win32/winmsg/window-styles>
- Microsoft `SetWindowRgn` documentation as the supported mechanism for clipping a child
  window to a non-rectangular or scrolled visible area:
  <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowrgn>
- Microsoft `RedrawWindow` / `InvalidateRect` documentation for forcing repaint of a region
  vacated by a hidden child:
  <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-redrawwindow>
- vlcj (the mature Java LibVLC binding) documents that embedded playback in a heavyweight
  child component requires the component to be realized, sized, and visible **before** play,
  and that overlays must be clipped by the host — the same discipline this packet adopts:
  <https://github.com/caprica/vlcj>
- Reddit, Hugging Face, Civitai, and X/Twitter searches produced no directly transferable
  Rust/egui + LibVLC child-surface implementation; the Win32 and LibVLC references above
  remain the field basis.

## Selected approach

**Replace emergent placement with a single deterministic surface-placement authority.**

- Introduce one explicit, authoritative playback placement state holding: the owner
  (`library` | `viewer` | `none`), the exact target path, the requested rect in egui points,
  and the clip rect of the owning panel. Drawing code **records an intent** into this state;
  it never calls `show_at` or `hide` directly.
- Reconcile exactly once per frame, after the UI tree is built and before the frame is
  presented. The reconciler performs the only `show_at` / `hide` / clip calls, so every frame
  has exactly one placement decision. If no owner recorded an intent this frame, the surface
  is hidden rather than left floating — this removes the audio-only, orphaned-surface, and
  keep-alive-flicker mechanisms together.
- **Clip the child to the owning panel's visible rect.** Intersect the requested rect with the
  panel and scroll clip rects; when the intersection is empty, hide. Where a partial overlap
  must still render, apply `SetWindowRgn` so the child cannot paint outside the panel. This
  removes "flickers all over the place".
- **Make hide authoritative.** Query live `IsWindowVisible` instead of trusting the cached
  `surface_visible` flag, clear `last_surface_bounds` on hide/stop, and force an
  `InvalidateRect`/`RedrawWindow` of the vacated parent region so no stale frame survives on
  screen.
- **Show before play.** Size and show the child at its real target bounds before
  `player_play`, rather than creating it 16×16 hidden and hoping a later render frame fixes
  it. Where the target is not yet known, defer `play` rather than starting playback into a
  hidden window.
- **Handle folder changes explicitly.** `start_compare_scan_internal` must make a deliberate
  decision about the active playback: either retain it with a valid re-placement path or stop
  it and release the surface. Silence is no longer permitted.
- **Trace and prove the placement lifecycle.** Add `vlc.show_at` / `vlc.hide` / `vlc.stop` /
  `vlc.clip` trace phases and surface owner, requested rect, clipped rect, live child bounds,
  and live visibility in the `media_video_control` receipt and the state snapshot.

## Rejected options

- Switching the default vout away from `wingdi`: the evidence shows the failure is placement,
  not decode or output-module selection, and `wingdi` was chosen deliberately for
  compositing and capture (`product/src/video_player.rs:940-946`). Changing it would mask the
  defect and risk the capture path.
- Recreating the child window on every owner change: `ensure_window` reuse is correct and a
  create/destroy cycle per switch would add latency and new failure modes.
- Reparenting between Library and Viewer host windows: there is only one parent HWND, and the
  parent is immutable after load by design (`product/src/video_player.rs:338-341`).
- Painting video through egui as a texture instead of a native child: a full re-architecture
  of the playback path, far beyond this defect, and it would lose LibVLC's own presentation.
- Extending the keep-alive window or adding retries: the keep-alive is itself a flicker
  source; the fix is a definite per-frame decision, not a longer indefinite one.
- Fixing only the Viewer early return: it removes one audio-only path but leaves the
  unclipped surface, the cached-visibility no-op hide, and the folder-switch silence.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-065" summary="Placement races, clipping cost, hide correctness, deferred play, and capture-path regressions have explicit controls and independent live proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Hiding when no intent was recorded stops legitimate playback during a transient frame.**
  A single dropped frame during a scan could hide a video the operator is watching. Control:
  the reconciler hides the **surface** without stopping the **player**, and only after a
  bounded number of consecutive intent-free frames; the decode state and position survive.
  Test: scroll the playing tile out of view and back, and assert playback continues and the
  picture returns at the correct position.
- **Per-frame `SetWindowRgn` is expensive and can itself flicker.** Control: apply the region
  only when the clipped geometry changes, cache the last applied region, and prefer plain
  bounds intersection when the visible area is rectangular. Measure frame cost.
- **Live `IsWindowVisible` per frame adds a Win32 round trip.** Control: query once per frame
  in the reconciler, not per call site.
- **Forcing invalidation causes a repaint storm.** Control: invalidate only the exact vacated
  rectangle, once, on the hide transition.
- **Deferring `play` until bounds are known makes Play feel unresponsive or silently
  no-op.** Control: bound the deferral, surface a visible "preparing" state, and report the
  deferral in diagnostics; on expiry, fail loudly rather than starting hidden playback.
- **The pending large-folder placement contract from WP-063 regresses.** The 10 s / 120 s
  pending resolution exists for a real reason (`product/src/ui.rs:7518-7545`). Control: keep
  pending **selection** resolution while removing pending **surface** ambiguity, and re-run the
  existing `pending_playback_relocates_after_terminal_scan_sort` test
  (`product/src/ui.rs:14439`).
- **`ui_snapshot` capture breaks because bounds now change more often.** The capture path
  already refuses on `target_bounds_px != child_bounds_px`
  (`product/src/ui.rs:631-635`). Control: the reconciler is the single writer of target
  bounds, so target and live bounds converge within one frame; re-run
  `native_video_capture_requires_current_visible_matching_surface`
  (`product/src/ui.rs:14559`) and
  `native_video_capture_clips_partial_surface_without_rescaling_hidden_pixels`
  (`product/src/ui.rs:14591`).
- **Owner thrash between Library and Viewer on alternating frames.** Control: owner changes
  are explicit state transitions with hysteresis, not per-frame derivations; assert a bounded
  number of surface transitions across a scripted owner-swap sequence.
- **Fullscreen origin mismatch persists.** Control: assert that the reconciler's requested
  rect and the live child rect agree in both windowed and borderless-fullscreen states, on
  both a primary and a scaled/secondary monitor if available; this converts the open
  NOT_DETERMINED into a measured result.
- **Regression on the operator's exact media.** Control: the acceptance gate is the operator's
  own local folder and the 141k-video mapped-drive folder, not synthetic fixtures.
- **The defect is intermittent and a single passing run proves nothing.** Control: proof
  requires a scripted multi-transition sequence — Library play → scroll out → scroll back →
  Viewer play → folder change → tab change → fullscreen → windowed — with placement asserted
  at every step, repeated from a cold process start and from a session that has already run a
  long scan.

## Verification

- Focused unit tests for the new reconciler: owner resolution, intent-free frame handling,
  clip intersection including the empty case, hide/stop clearing `last_surface_bounds`,
  live-visibility-based hide, and deferred play bounds.
- Focused tests for the two previously untested render behaviors: the Library-owns-the-child
  early return and the keep-alive window.
- Full `cargo test --manifest-path product/Cargo.toml`.
- `FACIAL_PLAYBACK_TRACE` capture across the full scripted transition sequence above, with the
  new `vlc.show_at` / `vlc.hide` / `vlc.stop` / `vlc.clip` phases inspected directly.
- Background live proof without foreground activation: `facial.exe --background`, then
  `facial-cli media_video_control --action play` and `--action play_library`, `--action status`,
  and `facial-cli ui_snapshot --out …` at every step. Receipts must show advancing time,
  `child_visible == true`, `libvlc_hwnd_matches == Some(true)`, and
  `target_bounds_px == child_bounds_px`.
- Direct visual inspection of the composited `ui_snapshot` PNG and the `-video.png` sidecar at
  each transition — the picture must be present in the panel and absent everywhere else.
- Deterministic `facial-cli ui-inspect` coverage for the media video presets, with affected
  PNG and `layout.json` artifacts opened and inspected.
- Operator-media acceptance: the exact local folder and the 141,787-video mapped-drive folder,
  proving Library placement, Viewer placement, folder change, tab change, and fullscreen
  round-trip each keep or correctly release the picture.
- Correct the stale `GetActiveWindow` claim in
  `governance/refinements/wp-060-embedded-video-panel-terminology-v1.md:31`.
- Independent high-risk adversarial review of the placement reconciler, Win32 clipping and
  invalidation, deferred play, and capture-path interaction.

</topic>
