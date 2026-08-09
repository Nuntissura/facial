---
file_id: REF-WP-055-NAS-MEDIA-RESPONSIVENESS-SETTINGS-STABILITY-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-055" summary="Implement the verified NAS responsiveness, playback, and Settings stability corrections without a transactional Apply/Save redesign." updated_at="2026-08-09">

## Operator request

- Keep very large NAS media collections fast and responsive during scan, browsing,
  thumbnail generation, and embedded playback.
- Fix the Settings window growing or changing outer size when categories change.
- Keep Settings controls, especially Close, inside the viewport.
- Give Settings the same softened-background treatment as the folder navigator.
- Allow backdrop click and Escape to close Settings while preserving the existing
  live-apply and auto-save model.
- Do not implement a generic Apply/Save prompt or transactional settings redesign.
- Implement all verified adjacent performance, persistence, diagnostics, and
  hardening improvements proposed after inspecting the exact NAS folder.

</topic>

<topic id="spec-anchors" status="active" version="1" wp="WP-055" summary="WP-050, WP-052, and WP-053 behavior is refined rather than replaced." updated_at="2026-08-09">

## Existing behavior retained

- WP-050 progressive scans, virtual media grid, thumbnail priority queues, and visual
  inspector remain the base.
- WP-052 lazy LibVLC playback, one isolated FFmpeg worker, generation cancellation,
  and native-frame diagnostics remain the base.
- WP-053 unified Settings categories, live preferences, in-app folder navigation,
  mapped-drive/UNC support, and selected-folder-only search remain the base.

## Scope edges and non-goals

- Do not change NAS credentials, Windows mappings, routes, adapters, or DSM settings.
- Do not launch Explorer, VLC, or any external window during background work or tests.
- Do not replace Settings with a draft transaction or generic Apply/Cancel workflow.
- Do not treat an incomplete or unavailable network scan as authoritative deletion.
- Do not make a filesystem watcher the sole source of media-library truth.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-055" summary="Current official APIs and exact runtime measurements support a background, cached, generation-safe design." updated_at="2026-08-09">

## Sources and local evidence checked

- Rust `std::fs::DirEntry`: on Windows, `file_type` and `metadata` reuse directory-entry
  data without an extra system call.
  <https://doc.rust-lang.org/std/fs/struct.DirEntry.html>
- Microsoft `FindFirstFileEx`: large-fetch enumeration is available for directory
  queries, including network shares.
  <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfileexa>
- Microsoft `ReadDirectoryChangesExW`: network buffers have bounded behavior and
  overflow requires full reconciliation.
  <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesexw>
- VideoLAN LibVLC media API: per-media advanced options are supported, but remote
  caching values require measured selection.
  <https://videolan.videolan.me/vlc/master/group__libvlc__media.html>
- Exact current NAS path: 141,400 supported media, 1,071 ms first Facial batch,
  43,023 ms complete scan, zero directory errors. An indicative no-extra-stat walk
  completed in 11,554 ms but differed by 23 paths, so semantic reconciliation is a
  mandatory proof gate.
- Exact SMB path currently uses wired 1 GbE with 0% measured packet loss and a
  sequential sample read of 86.8 MiB/s; raw throughput is not the sampled video's
  playback bottleneck.
- Source inspection found per-entry path metadata, per-frame NAS existence checks,
  full child-folder cloning/layout, 100 ms active-video repaint, and nested Settings
  auto-sizing feedback.

## Selected approach

- Reuse `DirEntry` metadata, publish a small first batch, then efficient larger
  batches; preserve cancellation and exact traversal semantics.
- Persist a transactional, generation-tagged last-good inventory in the existing
  redb store; show it immediately and reconcile in the background.
- Keep transient share failures stale/offline and preserve last-good rows.
- Normalize safe mapped-drive/UNC aliases to a stable root identity without merging
  unrelated shares.
- Move filesystem validation and large child-folder work out of the render loop;
  virtualize visible folder rows.
- Keep LibVLC commands immediate and cache displayed state; use faster repaint only
  while playing or interacting and benchmark any remote-file cache option.
- Replace Settings auto-sizing with an explicit viewport-clamped modal layout and
  shared translucent scrim.
- Extend structured diagnostics and deterministic visual inspection to prove latency,
  offline recovery, stable bounds, and click interception.

## Rejected options

- Relying on a faster direct NAS link alone: it does not remove UI-thread round trips,
  per-entry metadata queries, or the Settings layout defect.
- Unbounded parallel filesystem work: it can saturate the NAS and make interactive
  operations worse.
- Purging inventory on scan error: a transient network outage would appear as mass
  deletion.
- Watcher-only inventory: network notification loss/overflow requires reconciliation.
- A fixed guessed LibVLC cache value: excessive buffering can worsen seek latency.
- Generic Apply/Save prompts: current settings mostly apply live and auto-save, so the
  prompt would misrepresent behavior without a full rollback-capable transaction.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-055" summary="Failure scenarios are controlled by exact-set comparison, transactional generations, bounded work, and visual interaction proof." updated_at="2026-08-09">

## Risks, controls, and remediations

- Optimized traversal changes the file set: compare sorted paths against the legacy
  scanner, including symlinks, junctions, inaccessible directories, Unicode, and case.
- A disconnected share erases the library: commit only a complete successful scan;
  retain and mark the last-good generation stale/offline on any traversal error.
- Mapped and UNC aliases collide incorrectly: resolve only proven aliases beneath the
  same configured root identity and regression-test distinct shares.
- Smaller batches flood the UI: use a small first batch, larger subsequent batches,
  bounded channel draining, and stale scan IDs.
- Cached validity becomes stale: validate asynchronously on selection/action and expose
  explicit refresh/error state without blocking paint.
- Faster playback repaint consumes CPU: use 30-60 FPS only while playing or directly
  interacting, then fall back when paused/idle.
- Cached playback state drifts: reconcile from LibVLC on a bounded cadence and surface
  command errors.
- Backdrop click leaks to Media: consume the pointer event before closing Settings.
- Settings becomes unreachable on DPI/display changes: clamp its rect on every open and
  keep a sticky Close action in the title/footer.
- Autosave fails during close: do not report Saved; preserve dirty state and expose a
  retryable error.

## Verification needs

- Exact-set scanner equivalence and incomplete-scan last-good retention tests.
- Multiple exact NAS scan runs with first-batch, total-time, error, and cancellation
  evidence; reconcile the previously observed 23-path discrepancy.
- Frame diagnostics proving no filesystem calls occur from the hot Media draw loop.
- Input-to-command and player-poll timing while a NAS scan and video playback coexist.
- Settings screenshots for every category, 30+ settle passes, category-switch sequence,
  1280x800, high font scale, backdrop click, Escape, and click-through prevention.
- Full Rust suite, canonical executable packaging, native visual inspection, and
  adversarial regression review.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-055" summary="Implementation is split into independently testable Settings, scan/inventory, playback/UI, diagnostics, and release closure units." updated_at="2026-08-09">

## Execution units

1. Stabilize Settings modal geometry, backdrop behavior, close paths, and inspector.
2. Optimize traversal and batch publication while proving exact-set equivalence.
3. Add transactional persistent inventory, root identity, offline retention, and tests.
4. Remove render-loop filesystem work and virtualize cached child folders.
5. Improve playback command feedback, polling/repaint cadence, and diagnostics.
6. Run NAS, offline, visual, full-suite, adversarial, package, and invariant gates.

</topic>
