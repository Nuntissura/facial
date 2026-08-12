---
file_id: REF-WP-068-PER-TAB-ORDERING-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-068" summary="Order media by size, name, last edited, and date created, ascending or descending, independently per media tab." updated_at="2026-08-12">

## Operator request

Verbatim operator item folded into this packet:

- "order files in media tabs by size, name, last edited, date created, ascending, descending. (per media window)"

## Interpretation

- Four sort keys are required: name, size, last edited (modified time), and date created.
- Each key must be selectable ascending or descending.
- The selection is **per media tab** ("per media window"), so two open tabs may sort differently
  at the same time.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-068" summary="Three of four keys exist, created-time is captured nowhere, and the live sort value is global on the explorer even though the durable tab record already stores it per tab." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

- Sort keys today are three, not four: `MediaSort { Name, Modified, Size }` at
  `product/src/media_explorer.rs:76-108`, with `label()`, `to_setting()`
  (`"name" | "modified" | "size"`) and `from_setting()` defaulting to `Name`. The persisted-tab
  mirror enum is `MediaTabSort { Name, Modified, Size }` at
  `product/src/media_tabs.rs:58-65`.
- **Ascending/descending already works.** `sorted_indices_cancellable`
  (`product/src/media_explorer.rs:545-630`) with `compare_optional_stat`
  (`product/src/media_explorer.rs:632-644`); unknown or errored stat values always sort last
  regardless of direction (`product/src/media_explorer.rs:640-641`, covered by the test at
  `product/src/media_explorer.rs:834-841`).
- **The live sort value is global, not per tab.** `MediaExplorerState.sort: MediaSort` and
  `.sort_desc: bool` at `product/src/media_explorer.rs:240-241`, persisted as the global
  settings `media_sort` / `media_sort_desc`
  (`product/src/media_explorer.rs:393-396`, `:415-416`).
- It **is** already mirrored into the durable per-tab record: `viewport.sort` and
  `viewport.sort_descending` (`product/src/media_tabs.rs:93-94`), snapshot at
  `product/src/ui.rs:11847-11852`, restore at `product/src/ui.rs:11977-11982`. So the durable
  contract is per tab while the running value is single and global — changing sort in one tab
  changes the live value used by whichever tab is active.
- UI control: combo box at `product/src/ui.rs:6379-6403`; re-selecting the active key toggles
  direction, selecting a new key resets to ascending. Label renders `"{key} ↓/↑"`
  (`product/src/ui.rs:6370-6378`). A second setter exists via `request.sort_to` from the
  context menu (`product/src/ui.rs:1974-1993`, applied at `product/src/ui.rs:6127-6129`).
- **Created-time is captured nowhere.** The scanned row is only a path —
  `Arc<Vec<String>>` (`product/src/ui.rs:11943`, `product/src/ui.rs:1615-1626`). Size and
  modified time come from a separate sidecar sweep: `FileStat::Known { mtime: Option<u64>,
  size: u64 }` at `product/src/media_explorer.rs:120-125`, map
  `MediaExplorerState::stats` at `product/src/media_explorer.rs:277`, sweep
  `media_maybe_spawn_stat_sweep` at `product/src/ui.rs:11182-11290+` with one
  `std::fs::metadata(path)` per file at `product/src/ui.rs:11270`. `FileStat` has **no
  `created` field** (`product/src/media_explorer.rs:120-147`).
- The stat sweep is spawned **only when sort != Name**
  (`product/src/ui.rs:11183-11192`) and gated on `!lane.scanning || scan_using_cached_inventory`
  (`product/src/ui.rs:11197-11199`), chunked under `WorkClass::Metadata` I/O permits
  (`product/src/ui.rs:11241-11254`).
- Sort participates in the display-cache key: `MediaDisplayCacheKey` carries `sort` and
  `sort_desc` alongside `lane_id`, `scan_id`, and the content/stat/semantic/meta generations
  (`product/src/ui.rs:6853-6864`).
- Because the fast identity-order publication requires sort == Name **and** an empty query
  (`product/src/ui.rs:2816-2820`, `product/src/ui.rs:2878-2884`), any non-Name sort defers the
  first grid paint to the worker round trip — see WP-069, which owns that behavior.

## Current external sources checked

- Rust `std::fs::Metadata::created` is supported on Windows and returns the NTFS creation time;
  it returns an error on platforms/filesystems that do not record it, so it must be optional:
  <https://doc.rust-lang.org/std/fs/struct.Metadata.html#method.created>
- Microsoft `WIN32_FILE_ATTRIBUTE_DATA` / `GetFileAttributesEx` documents `ftCreationTime`,
  `ftLastWriteTime` and the file size as a single stat retrieval, confirming all three values
  come from one call rather than three:
  <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileattributesexw>
- Microsoft notes that on some volumes the last-access and creation timestamps may be
  imprecise or disabled by policy, which is why an unknown value must be sortable rather than
  fatal: <https://learn.microsoft.com/en-us/windows/win32/sysinfo/file-times>
- Windows File Explorer's column model (Name, Date modified, Type, Size, Date created) is the
  operator's reference implementation and establishes the expected key set and the
  click-to-toggle-direction convention:
  <https://support.microsoft.com/en-us/windows/file-explorer-in-windows-ef370130-1cca-9dc5-e0df-2f7416fe1cb1>
- Microsoft Win32 tab guidance requires page-scoped controls to live inside the tab content,
  which is the direct argument for a per-tab rather than global sort control:
  <https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-tabs>
- Rust `slice::sort_by` is a stable sort, so ties keep their prior order — relevant to keeping
  equal-size or equal-timestamp runs deterministic:
  <https://doc.rust-lang.org/std/primitive.slice.html#method.sort_by>
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no directly transferable
  implementation evidence for this item; the platform references above are the field basis.

## Selected approach

- Extend `MediaSort` and `MediaTabSort` with a fourth key, `Created`, with setting token
  `"created"` and safe `from_setting` fallback to `Name` so older records load unchanged.
- Extend `FileStat::Known` with `created: Option<u64>`, populated from the **same**
  `std::fs::metadata` call that already yields size and mtime — no additional filesystem round
  trip. `created()` returning an error yields `None` and sorts last, exactly as unknown `mtime`
  already does.
- Make the live sort state **per tab**. The durable record already stores it
  (`product/src/media_tabs.rs:93-94`); the fix is to stop treating
  `MediaExplorerState.sort`/`sort_desc` as the single source of truth for the active viewport
  and to restore/snapshot it as authoritative per tab, keeping the global setting only as the
  default for newly created tabs.
- Keep the existing combo-box interaction (re-select toggles direction, new key resets to
  ascending) and the existing context-menu `sort_to` path; both write to the active tab's state.
- Keep sort in `MediaDisplayCacheKey` so a per-tab sort change invalidates only that tab's
  display order.
- Spawn the stat sweep for any sort key that needs stat data — now `Modified`, `Size`, and
  `Created` — retaining the existing `WorkClass::Metadata` I/O permits and cancellation.
- Preserve the existing rule that unknown stat values sort last in both directions, and keep
  the sort stable so equal keys retain enumeration order.

## Rejected options

- Capturing creation time during directory enumeration for every file: it would put a stat cost
  on the scan path for all sorts including Name, directly opposing WP-069's thumbnail-first
  goal.
- A separate stat sweep for created-time: doubles filesystem traffic on remote roots for no
  benefit, since one `metadata` call already returns all three values.
- Making sort fully global and documenting it as intended: the operator explicitly asked for
  per media window.
- Adding sort keys beyond the four requested (type, dimensions, duration, rating): unrequested
  scope; duration in particular would require decoding every video.
- Sorting on the worker without stat data by falling back to name: it would silently render a
  wrong order under the label of the selected key.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-068" summary="Per-tab live state divergence, stat cost on remote roots, missing creation times, and cache-key correctness have explicit controls and proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Per-tab sort leaks across tabs through the still-global explorer field.** Control: a single
  authoritative read/write path for the active tab's sort, with snapshot on deactivate and
  restore on activate; test two tabs with different keys and directions, switching repeatedly.
- **A stale display order is reused after a sort change.** Control: sort and direction remain in
  `MediaDisplayCacheKey` (`product/src/ui.rs:6853-6864`); assert a key change invalidates only
  the affected tab's cached order.
- **Enabling a stat-dependent sort on a 141k-file remote folder triggers a metadata storm.**
  Control: keep the existing `WorkClass::Metadata` permits and chunking
  (`product/src/ui.rs:11241-11254`), keep cancellation on scan/tab change, and measure sweep
  duration and I/O on the operator's mapped-drive folder.
- **Creation time is unavailable on some volumes and the order looks arbitrary.** Control:
  `None` sorts last in both directions, the status line states how many rows lack the value,
  and a test covers an all-unknown set.
- **Windows creation time is misleading after a copy** (a copied file gets a new creation time,
  newer than its modified time). Control: this is platform behavior, not a defect; the Manual
  states it plainly so the operator is not surprised. No attempt is made to synthesize a
  "true" creation time.
- **Sort becomes unstable and rows shuffle between frames for equal keys.** Control: stable
  sort with a deterministic final tiebreak on the canonical key; test equal-size and
  equal-timestamp runs for byte-identical order across repeated runs.
- **A non-Name sort delays the first grid paint.** Control: this is WP-069's contract; here the
  requirement is only that a sort change never blanks an already-published order until the new
  one is ready. Test that the previous order stays visible during recomputation.
- **Old persisted records containing an unknown sort token fail to load.** Control:
  `from_setting` falls back to `Name`; test loading a record with `"created"` on a build that
  predates it and a record with an unknown token.
- **`u64` timestamp conversion overflows or panics on odd filesystem values.** Control: use the
  existing checked conversion pattern used for `mtime` and map failures to `None`.

## Verification

- Focused unit tests: `Created` round-trips through `to_setting`/`from_setting`; unknown
  created-time sorts last ascending and descending; stable order for equal keys; per-tab sort
  isolation across activation; display-cache invalidation on key and direction change.
- Focused test that one `std::fs::metadata` call populates size, mtime, and created together.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets showing the four-key sort control, an
  ascending and a descending state, and two tabs sorted differently; affected PNG and
  `layout.json` artifacts opened and directly inspected.
- Receipt-backed model proof: the state snapshot reports the active tab's sort key and
  direction, and a second tab's differing sort, without foreground activation.
- Scale proof on the operator's 141,787-video mapped-drive folder: measure stat-sweep duration
  and confirm the viewport stays responsive and cancellable during it.
- Independent high-risk adversarial review of per-tab state authority, cache-key correctness,
  and stat-sweep cost on remote roots.

</topic>
