---
file_id: REF-WP-064-MEDIA-TAB-LIFECYCLE-NAVIGATOR-LOCK-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-064" summary="Opening a third parallel media tab leaves the app stuck behind the blurred folder-browser backdrop, and switching tabs reloads the whole folder instead of restoring it." updated_at="2026-08-12">

## Operator request

Verbatim operator items folded into this packet:

- "New Tab for browser window does not work for more then 2 parallel windows. third window gets stuck on a blurred app ( background behaviour for folder browsing window)"
- "switching tabbed folders reloads everything"

## Interpretation

- "New Tab for browser window" is the Media tab strip `+` button / Ctrl+T, which today opens
  the couch folder navigator rather than creating a tab directly. "Third window gets stuck on
  a blurred app" is the folder-navigator modal backdrop remaining up with the application
  behind it unusable.
- "Switching tabbed folders reloads everything" means activating an existing Media tab must
  restore that tab's already-known viewport immediately instead of performing a cold rescan,
  index rebuild, and display-order recomputation.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-064" summary="The navigator modal is global state opened by an unowned screenshot reply with a force-open fallback and a failure path that never closes it; tab activation always rescans and always blanks the display order." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### Tab creation and the navigator

- Ctrl+T and the tab-strip `+` do **not** create a tab. Both call
  `request_media_folder_navigator`: `product/src/ui.rs:5922` and `product/src/ui.rs:5953`
  reach `product/src/ui.rs:6007-6010`; the `+` button is `product/src/ui.rs:5997-6003`.
- A tab is created only by `MediaTabsState::open_folder_in_new_tab`
  (`product/src/media_tabs.rs:207-223`), which pushes the tab **and sets it active**
  (`product/src/media_tabs.rs:221`).
- The navigator's "Open in new tab" button only stages a request
  (`product/src/ui.rs:9263-9276`, `product/src/ui.rs:9313-9325`). Consumption is in
  `apply_compare_lane_request` at `product/src/ui.rs:12895-12904`.
- `open_media_folder_in_new_tab` (`product/src/ui.rs:12034-12047`) snapshots the active tab,
  clones state, calls `persist_media_tabs_state(&candidate)?` — a **synchronous redb write on
  the UI thread** (`product/src/ui.rs:11869-11878`, `product/src/media_db.rs:857-867`) — and
  only then commits and calls `materialize_active_media_tab()`.

### The stuck blurred backdrop

- Modal open-state is **global**, not per tab: `MediaExplorerState.show_folder_navigator`
  at `product/src/media_explorer.rs:271`. The backdrop textures and pending timestamps are
  global fields on `FacialApp` (`product/src/ui.rs:844-853`, initialized `None` at
  `product/src/ui.rs:1184-1187`).
- Per-tab viewport carries only navigator **location**, never open-state
  (`product/src/media_tabs.rs:96-97`), and restore explicitly closes the navigator
  (`product/src/ui.rs:11991`). `close_media_folder_navigator`
  (`product/src/ui.rs:8862-8866`) clears the flag, texture, and pending timestamp. So tab
  activation cannot itself restore a stuck "open" flag.
- **Failure path never closes the modal.** The navigator is closed only on the `Ok` branch of
  the new-tab commit (`product/src/ui.rs:12899`); the `Err` branch sets a message and leaves
  the modal open (`product/src/ui.rs:12901`). The model-intent path has the same asymmetry
  (`product/src/ui.rs:4307-4310`). The two `Err` sources are `persist_media_tabs_state`
  (`product/src/ui.rs:11870-11877`, including the `media_tabs_persistence_blocked` latch) and
  the `MAX_TABS` guard (`product/src/media_tabs.rs:208-213`).
- **Screenshot replies are unowned.** Backdrop capture is two-phase: request
  (`product/src/ui.rs:8826-8842`, `ViewportCommand::Screenshot` at
  `product/src/ui.rs:8839-8840`) then consume
  (`handle_folder_navigator_backdrop_capture`, `product/src/ui.rs:11660-11691`). The reply
  carries **no request ID** (comment at `product/src/ui.rs:11346-11348`), and the navigator
  handler runs at `product/src/ui.rs:15487`, before `handle_model_snapshot_capture` at
  `product/src/ui.rs:15499`, consuming any screenshot event.
- `model_snapshot_owns_screenshot(request_started, settings_capture_pending)`
  (`product/src/ui.rs:595`, used at `product/src/ui.rs:11381` and `product/src/ui.rs:11389`)
  represents the **Settings** capture only; the folder-navigator pending flag is not
  represented, and `request_media_folder_navigator` has no `pending_model_snapshot` guard —
  contrast `request_media_settings` at `product/src/ui.rs:11345-11353`.
- **The timeout force-opens the modal unconditionally.** After 500 ms the handler opens the
  navigator over the neutral fallback veil regardless of intervening operator action:
  `product/src/ui.rs:11681-11687`, force-open at `product/src/ui.rs:11678-11679` and
  `product/src/ui.rs:11686`. Fallback veil is `Color32::from_black_alpha(42)`
  (`product/src/ui.rs:14269-14274`).
- While the navigator is open or its capture is pending, the application is broadly gated:
  tab shortcuts disabled (`product/src/ui.rs:5903-5906`), video force-hidden
  (`product/src/ui.rs:7625-7631`, `product/src/ui.rs:8414-8417`), controller actions rerouted
  into the navigator (`product/src/ui.rs:10452-10462`). This is why a stuck modal reads as
  "the app is stuck on a blurred app".
- Blur pipeline: `gaussian_settings_backdrop(source, 640)` at
  `product/src/ui.rs:14283-14308`, `image::imageops::blur(&downsampled, 6.0)` at
  `product/src/ui.rs:14303`, shared by both modals (`product/src/ui.rs:11640`,
  `product/src/ui.rs:11671`).
- NOT_DETERMINED from source alone: which of the two mechanisms (failure-path retention vs.
  unowned-screenshot/force-open) produces the operator's specific third-tab repro. Both are
  live defects and both are in scope.

### Tab switching reloads everything

- `activate_media_tab` (`product/src/ui.rs:12021-12032`) snapshots, clones, persists
  synchronously, then calls `materialize_active_media_tab`.
- `materialize_active_media_tab` (`product/src/ui.rs:11922-12019`) **always** rescans when the
  tab has a folder (`product/src/ui.rs:12006-12018`). `preserve_cached_inventory = true` only
  skips clearing visible rows and the redb preload (`product/src/ui.rs:1405-1435`,
  `product/src/ui.rs:1518-1543`); the **full directory walk still runs**
  (`product/src/ui.rs:1559-1572`).
- The runtime inventory cache exists — `MediaTabRuntimeInventory`
  (`product/src/ui.rs:161-165`), caps 8 tabs / 1,000,000 rows
  (`product/src/ui.rs:177-178`), populated by `cache_active_media_tab_inventory`
  (`product/src/ui.rs:11760-11796`) — but has a **guaranteed-miss condition** at
  `product/src/ui.rs:11764-11766`:
  ```rust
  if lane.files.is_empty() || lane.inventory_generation.is_none() { return; }
  ```
  `inventory_generation` becomes `Some` only after `ScanCacheReady`
  (`product/src/ui.rs:2810`) or a clean `ScanDone` requiring `dir_errors == 0` and a working
  inventory store (`product/src/ui.rs:2992-2996`, `product/src/ui.rs:1592-1611`,
  `product/src/ui.rs:1503-1506`). A tab left mid-scan, any folder with one unreadable
  subdirectory, or an unavailable redb inventory store caches nothing — switching back is a
  cold rescan.
- Even on a cache hit the grid does not paint immediately: `materialize_active_media_tab`
  restores `lane.files` (`product/src/ui.rs:11943-11949`) but then blanks the display order
  (`product/src/ui.rs:12000-12001`), and also clears the search index, semantic state, and
  diagnostics (`product/src/ui.rs:2002-2005`, `product/src/ui.rs:1436-1487`).
- The cheap identity-order publication only applies when the query is empty **and** sort is
  Name (`product/src/ui.rs:2816-2820`, `product/src/ui.rs:2878-2884`). Otherwise the grid
  waits through a 75 ms debounce plus a worker round-trip
  (`product/src/ui.rs:6869-6897`, `product/src/ui.rs:6899-6932`,
  `product/src/ui.rs:6948-6964`).
- Viewer texture is dropped unconditionally on activation
  (`product/src/ui.rs:11955-11956`), and `cancel_active_media_runtime`
  (`product/src/ui.rs:11884-11920`) bumps generations and stops video. The grid thumbnail LRU
  survives same-root switches but is cleared when the scan root identity changes
  (`product/src/ui.rs:2772-2787`).
- All Media tabs share one lane, id 0 (`product/src/ui.rs:11934-11940`,
  `product/src/ui.rs:6008`, `product/src/ui.rs:11678`). Async attribution is by
  `(lane_id, scan_id)` (`product/src/ui.rs:2769`, `2806`, `2857`, `2903`, `3149`); only the
  inline-video pending target is tab-id aware (`product/src/ui.rs:2964`,
  `product/src/ui.rs:3015`).

## Current external sources checked

- Microsoft Win32 tab guidance: changing tabs must not have side effects, and page content
  should be preserved across switches:
  <https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-tabs>
- Microsoft WinUI TabView guidance for document tabs, required active tab, and tab lifetime:
  <https://learn.microsoft.com/en-us/windows/apps/design/controls/tab-view>
- Microsoft guidance on modal dialogs requiring an explicit dismissal path and never trapping
  the user behind a blocking surface:
  <https://learn.microsoft.com/en-us/windows/win32/uxguide/win-dialog-box>
- egui 0.27 `ViewportCommand::Screenshot` and the `Event::Screenshot` reply carry no
  correlation identifier in this release line, confirming that request ownership must be
  tracked by the application: <https://docs.rs/crate/egui/0.27.0>
- egui viewport/screenshot issue history was checked in the upstream repository for
  request/reply correlation patterns: <https://github.com/emilk/egui>
- Browser tab-restore practice (Chromium session restore, Firefox tab unloading) treats a
  restored tab as "paint last-known state first, revalidate in background", which is the same
  publish-then-reconcile shape the topology already declares:
  <https://developer.chrome.com/docs/web-platform/page-lifecycle-api>
- Reddit and X/Twitter searches produced no directly transferable egui modal-ownership
  implementation; the Win32/WinUI guidance above remains the field basis.

## Selected approach

- **Own every screenshot request.** Introduce a monotonic capture-request token with an
  explicit owner (`settings` | `folder_navigator` | `model_snapshot`). A screenshot reply is
  consumed only by the current owner; a reply arriving with no matching owner is discarded
  rather than claimed. Extend `model_snapshot_owns_screenshot` to represent the
  folder-navigator pending state, and add the same `pending_model_snapshot` guard that
  `request_media_settings` already has.
- **Never force-open on timeout.** The 500 ms fallback opens the navigator only if the open
  intent is still current and no dismissal happened in the interim; otherwise the pending
  request is abandoned and cleared. The navigator opens over the neutral veil when the
  capture genuinely failed, which is acceptable, but it must not resurrect a cancelled open.
- **Close the modal on every terminal outcome.** The new-tab commit closes the navigator on
  both `Ok` and `Err`, surfacing the failure in the existing action-message surface. Same for
  the model-intent path.
- **Make the modal recoverable by construction.** Escape, backdrop click, and the footer close
  already exist; add an unconditional invariant that the navigator cannot remain open when its
  owning capture token is stale, and a receipt-backed diagnostic reporting modal open-state,
  capture owner, and pending age so a model can prove the lock without foregrounding.
- **Make tab activation restore-first.** On activation with a usable cached inventory, publish
  the cached rows **and** a cached display order in the same frame, then reconcile in the
  background. Preserve the display-order cache per tab instead of blanking it at
  `product/src/ui.rs:12000-12001`, and extend the identity fast-path beyond the
  empty-query/Name-sort special case by caching the last published order per
  `MediaDisplayCacheKey`.
- **Widen the cache-eligibility condition.** Cache a tab's inventory when rows exist even if
  `inventory_generation` is `None`, marking it explicitly as unverified-but-displayable; a
  partial or error-bearing scan still yields a viewport worth restoring, and reconciliation
  corrects it. This is the direct fix for "reloads everything".
- **Do not rescan on activation when a cached inventory was published.** Reconciliation runs
  as a background revalidation pass, not as `start_compare_scan_internal`'s full teardown.

## Rejected options

- Making `show_folder_navigator` per-tab state: the navigator is a single application modal;
  per-tab modality would multiply the stuck-state surface rather than remove it.
- Removing the blurred backdrop: it is a shared build-rule requirement
  (`governance/build_rules.yaml` FACIAL-STYLE-001/003) and is not the defect.
- Dropping the 500 ms fallback entirely: a genuinely lost screenshot would then leave the
  navigator permanently unopenable.
- Keeping the full rescan on activation and only speeding it up: the operator's complaint is
  the reload itself, and a 141k-row folder cannot be walked fast enough to hide it.
- Giving each tab its own lane and scan pipeline: a much larger refactor than the defect
  requires, and the existing `(lane_id, scan_id)` attribution already rejects stale delivery.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-064" summary="Capture-token races, failure-path modal retention, stale restored inventories, and persistence-blocked latches have explicit controls and proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Capture token leaks and the modal never opens.** Control: the token has a bounded age; on
  expiry the pending state is cleared and the affordance becomes usable again. Test: request
  a capture, drop the reply, assert the navigator is openable on the next attempt.
- **A model snapshot and a modal capture race and one gets the other's frame.** Control:
  owner-checked consumption; a non-owner never writes a receipt frame. Test: issue
  `ui_snapshot` and open the navigator in the same frame, assert the receipt frame and the
  backdrop texture are both correct or both explicitly refused.
- **Persistence-blocked latch makes every new tab fail forever.** `media_tabs_persistence_blocked`
  (`product/src/ui.rs:11870-11875`) is sticky. Control: the failure must close the modal, state
  the reason in operator-visible text, and expose the latch in diagnostics; adding a tab must
  not appear to silently do nothing. Test: force a persistence failure and assert the modal
  closes with a stated reason.
- **`MAX_TABS` refusal is indistinguishable from a hang.** Control: explicit refusal message
  plus modal close. Test at the 256 boundary.
- **Restored inventory is stale and the operator acts on files that no longer exist.**
  Control: restored-but-unreconciled state is visibly marked, reconciliation is always
  scheduled, and destructive actions are gated on a reconciled generation. Test: delete files
  externally, switch tabs, assert the stale rows reconcile and no action targets a missing file.
- **Restoring a cached display order publishes rows in the wrong tab.** Control: the display
  cache key already carries `lane_id`, `scan_id`, and content/stat/semantic/meta generations
  (`product/src/ui.rs:6853-6864`); the per-tab cache must key on tab id as well and reject
  mismatches. Test: rapid A→B→A switching with concurrent scans.
- **Widening cache eligibility caches a half-enumerated folder and it looks complete.**
  Control: store a completeness flag with the cached inventory and surface "reconciling" in
  the status line until a clean generation lands. Test: cancel a scan mid-batch, switch away
  and back.
- **Synchronous redb write on the UI thread stalls tab switching on a slow or remote
  workspace.** Control: measure and bound the write; if it exceeds budget, persist
  asynchronously while keeping the persist-before-visible-mutation invariant for the
  authoritative record. Test on a mapped network workspace.
- **Removing the rescan hides genuinely changed folders.** Control: reconciliation is
  mandatory, not optional, and F5 remains an explicit full rescan.
- **Regression of the WP-063 staged-versus-committed contract.** Control: re-run the existing
  assertion that browse actions leave active folder, scan ID, inventory, selection, and
  playback unchanged.

## Verification

- Focused unit tests: capture-token ownership and expiry; navigator closes on `Ok` and `Err`;
  `MAX_TABS` refusal; persistence-blocked refusal; cache eligibility with and without
  `inventory_generation`; per-tab display-order cache hit/miss and cross-tab rejection;
  activation without a full directory walk.
- Regression test for the operator repro: open three tabs in sequence through the navigator,
  asserting the modal closes each time and the third activation leaves the application
  interactive.
- Tab-switch timing assertion: activating a cached tab publishes rows and a display order in
  the same frame, with no `start_compare_scan_internal` teardown.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets: three-tab strip, navigator open over each
  tab, navigator dismissed, and a restored tab immediately after activation. Affected PNG and
  `layout.json` artifacts are opened and directly inspected.
- Background live proof: `facial.exe --background`, drive `media_tabs` and
  `media_folder_navigate` intents through `facial-cli`, and capture `ui_snapshot` without
  foreground activation. Receipts must report modal open-state, capture owner, active tab,
  staged versus committed folder, and whether a scan was requested.
- Large-folder proof on the operator's 141k-video mapped-drive folder: switch away and back
  and confirm the viewport is restored without a cold rescan.
- Independent high-risk adversarial review of capture ownership, modal terminal states,
  persistence failure latches, and cross-tab async attribution.

</topic>
