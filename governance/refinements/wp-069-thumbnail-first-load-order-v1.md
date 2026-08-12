---
file_id: REF-WP-069-THUMBNAIL-FIRST-LOAD-ORDER-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-069" summary="Thumbnails must appear and scrolling must be usable immediately; playback options, labels, and favorite badges load afterwards. This is the app's reason to exist over Explorer." updated_at="2026-08-12">

## Operator request

Verbatim operator item folded into this packet:

- "look for performance gains, load order revisit. thumbnail first, so i can start viewing and
  scrolling and do not have to wait for the thunbnails to load (sole purpose of the app,
  because explorer is horrible for large folders with media), then load playback options,
  labels and fav icons etc."

## Interpretation

- First visible paint of the thumbnail grid must not wait on anything that is not required to
  place a tile: not the full directory walk, not stat, not the search index, not the sort
  worker, not metadata.
- Once tiles are on screen and scrollable, the remaining layers load in a stated priority
  order: thumbnails for the visible band, then overscan, then playback affordances, then
  labels and favorite badges.
- The operator states this is the product's core value proposition against Windows Explorer, so
  it is a primary acceptance surface, not a tuning exercise.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-069" summary="Rows already stream in 64-row batches, but the grid renders the display order, which is blanked at scan start and only fast-published when the query is empty and sort is Name." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### What is already good

- Directory enumeration already streams in progressive batches — first 64 rows, then up to
  1024 (`product/src/ui.rs:14101-14103`, `product/src/ui.rs:14180-14186`;
  `ScanBatch` consumed at `product/src/ui.rs:2848-2888`), matching the declared
  `scan_batches: {first: 64, subsequent_max: 1024}` in `topology.yaml:357`.
- A last-good redb inventory is loaded before enumeration and published as `ScanCacheReady`
  (`product/src/ui.rs:1519-1542`, consumed `product/src/ui.rs:2796-2846`).
- Metadata is **already batched once**, not per row and not per frame: `load_media_metadata`
  hydrates notes, tags, labels, favorites, and favorite keys from a single
  `list_meta_by_key(None, None)` plus `favorites_keyed()`
  (`product/src/ui.rs:11725-11752`).
- Thumbnails are requested from the render path for the visible band then the overscan band,
  skipping already-uploaded keys (`product/src/ui.rs:7574-7593`), with texture uploads capped
  at 8 per frame (`product/src/ui.rs:11159-11179`).
- The render path performs no real database transactions. `key_for`
  (`product/src/media_db.rs:375-377`) and `path_for_key`
  (`product/src/media_db.rs:380-392`) are pure string operations; `is_writable`/`status` are
  field reads (`product/src/media_db.rs:341-343`, `:353-355`). Actual transactions occur only
  behind explicit operator actions.

### What actually blocks the first paint

- The grid renders `display` — the display-order vector
  (`product/src/ui.rs:5696`, passed at `product/src/ui.rs:5850-5856`) — **not** `lane.files`.
- The display order is blanked at scan start (`product/src/ui.rs:1444`) and again on tab
  materialization (`product/src/ui.rs:12000-12001`).
- The cheap identity-order publication applies **only when the query is empty and sort is
  Name** (`product/src/ui.rs:2816-2820`, `product/src/ui.rs:2878-2884`). In every other case
  the grid stays empty through a 75 ms debounce
  (`product/src/ui.rs:6888-6896`, `product/src/ui.rs:6899-6904`), the search-index build
  (`product/src/ui.rs:6906-6915`), and the worker sort/rank round trip
  (`product/src/ui.rs:6948-6964`).
- Consequence: **any non-default sort or any active query converts a streaming folder open into
  a blank grid until a worker completes.** With WP-068 adding size and created-time sorts, this
  becomes the dominant first-paint cost unless fixed here.
- The stat sweep runs only when sort != Name (`product/src/ui.rs:11183-11192`) — so choosing a
  stat-dependent sort adds one `std::fs::metadata` per file
  (`product/src/ui.rs:11270`) between the operator and the first tile.

### Per-frame costs on the render path

- `key_for` allocates a `String` per visible tile per frame at `product/src/ui.rs:7841`;
  lookups afterwards hit in-memory maps (`product/src/ui.rs:7845`, `:7873`).
- `path_for_key` runs per tab per frame at `product/src/ui.rs:5934-5938` and
  `product/src/ui.rs:4655`.
- A debug probe counts label paints per tile (`product/src/ui.rs:7842-7844`).
- The `render_db_calls: forbidden` rule is declared in `topology.yaml:331` but has **no
  compile-time, lint, or test enforcement** — a single repo-wide search finds only the topology
  declaration. It is currently an honor-system rule.

### Video thumbnails

- Video thumbs go through the same engine (`product/src/media_thumbs.rs:1010`,
  `is_video_source` at `product/src/media_thumbs.rs:1100`), resolving FFmpeg lazily
  (`product/src/media_thumbs.rs:1113-1139`) and running a subprocess with a timeout
  (`product/src/media_thumbs.rs:1205-1254`). Declared caps are visible 15 s / prefetch 5 s
  (`topology.yaml:369`).

### Tab-switch interaction

- WP-064 owns restoring a cached inventory and display order on tab activation. This packet
  owns the **first open** of a folder and the layered priority thereafter. The two share the
  display-order publication path and must not fight: the same cached-order mechanism serves
  both.

## Current external sources checked

- XnView MP large-collection threads: the field practice is to build thumbnails only for
  displayed items and to disable or defer metadata for very large folders:
  <https://newsgroup.xnview.com/viewtopic.php?t=40884>
- XnView slow-large-folder browsing reports confirming that metadata and catalog work, not
  decode, dominates perceived latency:
  <https://newsgroup.xnview.com/viewtopic.php?t=13665>
- FastStone versus XnView comparisons showing operators judge these tools primarily on
  time-to-first-usable-view in a large folder:
  <https://www.dpreview.com/forums/thread/3247237>
- Microsoft's `IThumbnailProvider` / Explorer thumbnail model documents on-demand extraction
  for visible items with a disk-backed cache, the same shape as the existing engine:
  <https://learn.microsoft.com/en-us/windows/win32/shell/thumbnail-providers>
- Chromium/Web list-virtualization guidance on rendering only the visible window plus a small
  overscan, and on avoiding layout work proportional to total item count:
  <https://developer.chrome.com/docs/lighthouse/performance/dom-size>
- egui 0.27 `ScrollArea::show_rows` / `show_viewport` documentation confirming the virtualized
  row API already in use supports rendering a known row count without materializing content:
  <https://docs.rs/crate/egui/0.27.0>
- Rust `std::fs::read_dir` streams entries lazily, so batch-publishing during enumeration is
  the idiomatic path rather than collecting first:
  <https://doc.rust-lang.org/std/fs/fn.read_dir.html>
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no directly transferable
  Rust/egui large-media-browser load-order implementation; the references above are the field
  basis.

## Selected approach

- **Publish a provisional display order for every batch, unconditionally.** Remove the
  empty-query-and-Name-sort restriction on fast publication. Every `ScanCacheReady` and
  `ScanBatch` publishes an immediately renderable order; the worker's ranked or sorted order
  replaces it when ready. The grid is therefore never blank while rows exist.
- **Never blank a published order.** Replace the blanking at `product/src/ui.rs:1444` and
  `product/src/ui.rs:12000-12001` with a generation-stamped swap: the previous order stays
  visible and is marked provisional until the new one arrives. This is the single highest-value
  change in the packet.
- **Layer the work explicitly**, in this priority order, with the existing I/O permit classes:
  1. row identity (path) — enough to place a tile,
  2. thumbnails for the visible band, then overscan,
  3. playback affordances for visible video tiles,
  4. labels and favorite badges,
  5. stat, search index, and semantic index.
  Each layer is independently cancellable and none of layers 3–5 may gate layer 1 or 2.
- **Decouple sort from first paint.** A stat-dependent sort renders the provisional order
  immediately and re-orders when the stat sweep completes, with a visible "ordering…"
  indication, instead of withholding the grid.
- **Remove avoidable per-frame allocation** on the render path: cache the canonical key
  alongside each row instead of recomputing `key_for` per tile per frame at
  `product/src/ui.rs:7841`.
- **Make `render_db_calls: forbidden` enforceable** rather than declarative: add a counter that
  the existing debug probe pattern already suggests, assert it stays zero across a rendered
  frame in tests, so the rule cannot silently regress.
- Keep every existing bound: 8 texture uploads per frame, visible-plus-overscan requests only,
  stale-job discard before decode, and the `media_io` root-kind permits.

## Rejected options

- Rendering `lane.files` directly and bypassing the display order: it would break search,
  sort, and the WP-064 restore path, which all depend on a single ordered vector.
- Removing the 75 ms debounce: the debounce protects the worker from per-keystroke churn; the
  defect is the blank grid during it, not the debounce itself.
- Pre-generating thumbnails for the whole folder on open: the exact behavior the operator
  rejects Explorer for, and unbounded on a 141k-file folder.
- Persisting a full thumbnail index per folder: a large new durable surface; the existing
  sharded disk cache already avoids repeat decode.
- Loading labels and favorites lazily per visible row: they are already batched in one pass,
  and per-row lookups would add render-path work rather than remove it.
- Raising the per-frame texture upload cap: trades frame time for fill rate and risks the
  responsiveness the packet is meant to protect.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-069" summary="Provisional-order correctness, reordering jitter, cancellation, and interaction with WP-064 restore and WP-068 sorts have explicit controls and measured proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **A provisional order lets the operator act on a row that the final order moves or removes.**
  Control: actions resolve through the canonical key, not the display index, and destructive
  actions are gated on a settled generation. Test acting on a tile while the order is
  provisional.
- **Visible reordering jitter as batches land.** Control: append-only publication during
  enumeration for the default order, a single settle transition at the end, and a visible
  provisional indicator so movement is explained rather than surprising. Test perceived
  stability on a 141k-row folder.
- **Search results appear stale because a provisional order is shown during ranking.** Control:
  while a query is active the provisional order must be clearly marked as unranked, or the
  prior ranked order retained, never a silently wrong ranking. Test typing a query mid-scan.
- **Removing the blanking causes cross-tab or cross-scan bleed.** Control: the order carries its
  full `MediaDisplayCacheKey` (`product/src/ui.rs:6853-6864`) plus tab identity, and a
  mismatched order is rejected rather than rendered. Test rapid tab and folder switching under
  concurrent scans.
- **Layering starves lower layers indefinitely on a busy remote root.** Control: each layer has
  a permit budget and a floor so labels and favorites eventually land; assert all layers
  complete on the operator's mapped-drive folder.
- **Playback priority regresses.** WP-065 and the existing playback lease must still preempt
  thumbnail work (`product/src/media_io.rs:360-374`). Control: re-run the playback-priority
  assertions with the new layering.
- **Enforcing `render_db_calls` breaks a legitimate call.** Control: the assertion targets
  storage transactions, not pure key/path string helpers, which are explicitly permitted and
  documented.
- **Caching keys per row increases memory on a 141k-row folder.** Control: measure; the keys are
  already produced per frame, so the trade is CPU for bounded memory, and the row count cap
  (`MAX_MEDIA_TAB_RUNTIME_ITEMS`, `product/src/ui.rs:177-178`) bounds it.
- **"Faster" is claimed without measurement.** Control: the acceptance surface is a measured
  time-to-first-tile and time-to-scrollable on the operator's own folders, recorded before and
  after, not a subjective judgement.

## Verification

- Instrumented measurement on the operator's exact folders — the local folder and the
  141,787-video recursive mapped-drive folder — recording time to first row published, time to
  first thumbnail visible, time to scrollable, and time to fully settled order. Before and
  after values are both recorded; an improvement must be shown, not asserted.
- The same measurements repeated for each sort key from WP-068, proving a stat-dependent sort
  no longer blanks the grid.
- Focused unit tests: provisional order published for every batch regardless of query and sort;
  a published order is never blanked, only swapped; order rejection on key or tab mismatch;
  layer priority and cancellation; zero storage transactions during a rendered frame.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets capturing a mid-scan provisional grid, a
  settled grid, and a stat-sorted grid; affected PNG and `layout.json` artifacts opened and
  directly inspected.
- Receipt-backed model proof: scan and query diagnostics report published row count, order
  provenance (cached, provisional, or settled), and per-layer progress without foreground
  activation.
- Frame-time sampling during scroll on the large folder, confirming no regression against the
  current build.
- Independent high-risk adversarial review of order provenance, cross-tab attribution,
  starvation, and action-resolution correctness under provisional orders.

</topic>
