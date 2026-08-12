---
file_id: REF-WP-067-FAVORITES-LABELS-TAB-SUBTABS-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-067" summary="Promote the favorites overlay into a first-class Media tab with sub-tabs for favorite videos, favorite images, and the created color labels." updated_at="2026-08-12">

## Operator request

Verbatim operator item folded into this packet:

- "rework the fav sidebar/panel into a tab. with sub tabs for fav videos, fav images, created color labels"

## Interpretation

- The right-edge favorites overlay is removed as the primary favorites surface and replaced by
  a Media tab whose content is a curated collection rather than a filesystem folder.
- That tab carries three sub-tabs: favorite videos, favorite images, and the operator's created
  color labels. The label sub-tab lists the label catalog and, on selection, the files assigned
  to that label.
- Collection tabs live in the same tab strip as folder tabs, so the operator can keep a
  favorites view open beside folder views.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-067" summary="Favorites are a right-edge overlay of folder pins with no media-kind data, the label catalog lives inside Settings, and the tab model has no tab-kind discriminant." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### The current favorites surface

- It is an overlay `Area`, not a panel or tab: `draw_media_overlays`
  (`product/src/ui.rs:8758-8802`), invoked once per frame at `product/src/ui.rs:5860`.
  Geometry at `product/src/ui.rs:8774-8779` — width `300.0.min(book.width() * 0.5)`, pinned to
  the right edge, `egui::Order::Foreground`.
- Visibility flag `MediaExplorerState.show_favorites`
  (`product/src/media_explorer.rs:246`, default `false` at
  `product/src/media_explorer.rs:306`). Toggles: toolbar star at
  `product/src/ui.rs:6488-6498`, action `A::ToggleFavoritesPanel` at
  `product/src/ui.rs:10588-10590`, forced open at `product/src/ui.rs:10501-10503`.
- Opening it **blanks the video player** (`product/src/ui.rs:7627`,
  `product/src/ui.rs:8415`) — a side effect that disappears once favorites are a tab.
- Body `draw_media_favorites_body` (`product/src/ui.rs:9403-9504`): a "Pin current folder"
  button (`product/src/ui.rs:9415-9423`) and a flat `ScrollArea` list of `(key, display)` rows
  (`product/src/ui.rs:9436-9478`); activation navigates the lane and rescans
  (`product/src/ui.rs:9479-9500`).
- Data: `media_favorites: Vec<(String, String)>` = `(canonical_key, display_path)`
  (`product/src/ui.rs:826-827`) plus `media_favorite_keys: HashSet<String>`
  (`product/src/ui.rs:828-829`), hydrated from `media_db.favorites_keyed()`
  (`product/src/ui.rs:11745`, `product/src/ui.rs:12438`).
- **Favorites carry no media-kind metadata.** Explicit comment at
  `product/src/ui.rs:9450-9455`; the cosmetic icon is derived from the extension via
  `is_supported_image_path` / `crate::media_explorer::is_video_path`. The store mixes folder
  pins and file favorites — favorites are pitched as folder pins at
  `product/src/ui.rs:9428`.

### The label catalog surface

- The WP-061 label manager lives **inside Settings**, not in a tab:
  section begins `product/src/ui.rs:9754` (`theme::kicker(ui, "Label manager")`), description
  `product/src/ui.rs:9755-9761`, create row `product/src/ui.rs:9764-9813`, per-definition
  rename/recolor/usage/delete `product/src/ui.rs:9839-9935`, persistence
  `product/src/ui.rs:9941-9979`.
- Per-file assignment UI is the `Labels ▾` menu in the Viewer (`product/src/ui.rs:8172`) and
  the grid context menu (`product/src/ui.rs:7281`, `product/src/ui.rs:7440`).

### Available data APIs (no filesystem rescan required)

| Need | API | Site |
|---|---|---|
| All favorites with display paths | `favorites_keyed()` | `product/src/media_db.rs:822-830` |
| Membership test | `is_favorite(path)` | `product/src/media_db.rs:768-770` |
| Label catalog | `color_label_definitions()` | `product/src/media_db.rs:879` |
| Per-label counts | `color_label_usage_counts()` | `product/src/media_db.rs:1046-1054` |
| Files for a label | `list_meta_by_key(None, Some(label))` | `product/src/media_db.rs:671-733` |
| Key ↔ path | `key_for` / `path_for_key` | `product/src/media_db.rs:375`, `:380` |

- `MediaMeta` (`product/src/media_db.rs:160-171`) is `notes, tags, label, labels, favorite` —
  **no media kind, no size, no mtime**. Media kind is derivable from the path extension with no
  filesystem access, which is exactly what the existing code already does: the search-index
  worker derives `is_video` from the path alone (`product/src/ui.rs:6730`).
- `list_meta_by_key` already includes favorite rows in its key set
  (`product/src/media_db.rs:679-689`), so an "all favorited files" query needs no second call.
- The whole metadata cache is already hydrated in one batched pass by `load_media_metadata`
  (`product/src/ui.rs:11725-11753`).
- `label:` filtering already accepts stable ID **or** visible name because the search index
  injects both (`product/src/ui.rs:6713-6721`).

### The tab model has no tab-kind discriminant

- `MediaTab { id, viewport }` (`product/src/media_tabs.rs:125-129`),
  `MediaTabsState { schema_version, next_serial, active_tab_id, tabs }`
  (`product/src/media_tabs.rs:131-137`), `MAX_TABS = 256`
  (`product/src/media_tabs.rs:19`), schema version 1 (`product/src/media_tabs.rs:17`).
- `MediaTabViewport` (`product/src/media_tabs.rs:72-98`) assumes a folder
  (`folder_key`), and titles are derived lexically from the folder path
  (`product/src/media_tabs.rs:347-358`, duplicate ordinals at
  `product/src/ui.rs:5946-5979`).
- Every tab is materialized by `materialize_active_media_tab`
  (`product/src/ui.rs:11922-12019`), which scans a folder — a collection tab must not take
  that path.
- `MediaTabsState::close` never removes the last tab; it resets it in place
  (`product/src/media_tabs.rs:236-252`).

## Current external sources checked

- Microsoft WinUI TabView guidance for heterogeneous document tabs, required active tab, and
  label/close behavior:
  <https://learn.microsoft.com/en-us/windows/apps/design/controls/tab-view>
- Microsoft Win32 tab guidance: page-scoped controls belong inside tab content, and switching
  tabs must not have side effects:
  <https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-tabs>
- Adobe Lightroom Classic Collections documentation as the dominant field model for virtual
  collections that reference files without moving them:
  <https://helpx.adobe.com/lightroom-classic/help/photo-collections.html>
- digiKam's albums-versus-tags/labels model, an open-source implementation of the same split
  between filesystem folders and curated views, inspectable at source level:
  <https://github.com/KDE/digikam>
- MediaChips documents built-in favorites, ratings, colors, and virtual-folder tags for a
  desktop media organizer, confirming the "virtual folder keeps the physical path intact"
  convention: <https://mediachips.app/docs/>
- Microsoft's nested-tab guidance discourages deep tab nesting; the accepted pattern for a
  second level is a segmented/pivot control inside the tab body, not a second tab row:
  <https://learn.microsoft.com/en-us/windows/apps/design/controls/pivot>
- egui 0.27 documentation for stable widget IDs across dynamic collections, required for a
  heterogeneous tab strip: <https://docs.rs/crate/egui/0.27.0>
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no directly transferable
  Rust/egui collection-tab implementation; the references above remain the field basis.

## Selected approach

- Add an explicit **tab kind** to the persisted tab record: `folder` (existing behavior,
  default) and `collection`. Bump the tab schema version and migrate existing records to
  `folder`, preserving the WP-063 versioned-record and rejected-value recovery contract.
- A collection tab holds a sub-view selector (`fav_videos` | `fav_images` | `labels`), a
  selected label ID for the label sub-view, and its own selection/cursor/scroll — reusing the
  existing `MediaTabViewport` fields rather than inventing a parallel viewport.
- Render the sub-views through the **same Library grid and Viewer panel** used by folder tabs.
  The only difference is where the row list comes from: a collection tab's rows are produced
  from the in-memory metadata cache instead of a filesystem scan, so thumbnails, selection,
  labels, favorites, context menus, playback, and the WP-065 surface reconciler all work
  unchanged.
- Materialization for a collection tab bypasses `start_compare_scan_internal` entirely and
  publishes rows plus a display order in the same frame — there is no directory to walk.
- Split favorites into images and videos by path extension using the existing
  `is_supported_image_path` / `is_video_path` helpers. Favorited **folders** remain first-class:
  surface them as a pinned-folders section that activates a folder tab, so the current
  folder-pin workflow is preserved rather than dropped.
- The labels sub-view lists the catalog from `color_label_definitions()` with counts from
  `color_label_usage_counts()`; selecting a label lists its files via `list_meta_by_key`.
- Keep the label **manager** (create/rename/recolor/delete) in Settings as the single mutation
  authority; the labels sub-view is a browse-and-select surface that links to it. This avoids
  two divergent CRUD paths.
- Retain `Ctrl+B` as the favorites affordance: it now opens or focuses the collection tab
  instead of the overlay. Remove the overlay and its video-blanking side effect.

## Rejected options

- Keeping the overlay and adding sub-tabs inside it: the operator explicitly asked for a tab,
  and the overlay's foreground `Area` is what forces the video blanking.
- A second row of real tabs for the sub-views: Microsoft guidance discourages nested tab rows;
  a segmented control inside the tab body is the sanctioned pattern.
- A separate top-level application tab beside Media/Project/Compare: it would not share the
  Library/Viewer viewport, the thumbnail engine, or the playback lease, and would duplicate the
  entire media surface.
- Building collection rows by scanning the filesystem for favorited files: unnecessary — the
  metadata cache already holds every favorite and label assignment, and scanning would
  reintroduce the large-folder cost this app exists to avoid.
- Moving label CRUD out of Settings into the new tab: two mutation paths for one catalog, with
  the usage-aware delete confirmation contract duplicated.
- Storing media kind in `MediaMeta`: redundant with the path, and it would need a migration
  plus a correctness risk when a file is renamed across kinds.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-067" summary="Tab-schema migration, collection tabs taking the folder scan path, missing files, label deletion, and kind misclassification have explicit controls and proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Schema bump corrupts or discards existing tab sessions.** Control: keep the WP-063
  versioned-record contract — validate on load, migrate absent `kind` to `folder`, retain the
  complete rejected raw value under `media_tabs_v1_rejected`, and block persistence rather than
  overwrite the only recoverable record. Test round-trip from a v1 record.
- **A collection tab reaches `materialize_active_media_tab` and triggers a folder scan of an
  empty path.** Control: kind is checked before materialization and collection tabs take a
  separate publish path; assert no scan is started and `scan_id` is unchanged.
- **Favorited files that no longer exist render as broken tiles.** Control: missing entries are
  shown in an explicit unavailable state with a remove action, and are never silently dropped
  from the store — favorites are operator data.
- **Extension-based kind classification misfiles items.** Control: reuse the existing shared
  helpers so the collection view, the search index, and the grid icon agree by construction;
  test container extensions that both helpers could claim, and files with no extension.
- **Folder pins and file favorites are conflated.** Control: the store already mixes them;
  classify explicitly and route folder activation to a folder tab and file activation to
  selection in the collection tab. Test a favorites store containing both.
- **Deleting a label while its collection tab is open leaves a dangling view.** Control: the
  existing usage-aware delete confirmation stays authoritative; open collection tabs referencing
  a deleted label fall back to the catalog list with a stated reason. Test delete-while-open.
- **Renaming a label breaks the tab's stored reference.** Control: store the stable label ID,
  never the visible name — the catalog already guarantees stable IDs
  (`product/src/media_db.rs:879`). Test rename-while-open.
- **Collection tabs count against `MAX_TABS` and duplicate endlessly.** Control: activating an
  existing collection tab focuses it instead of creating another; honor the 256 cap with a
  visible refusal.
- **Large favorites or label sets stall the UI.** Control: rows are produced from the already
  batched in-memory cache and rendered through the existing virtualized grid; no per-row DB
  call in the render path, consistent with the `render_db_calls: forbidden` contract in
  `topology.yaml:331`. Test with a large synthetic favorites set.
- **Removing the overlay breaks existing bindings or model intents.** Control: `Ctrl+B` and the
  `ToggleFavoritesPanel` action are remapped to the collection tab and remain in the bindings
  table; the change is documented in the Manual.

## Verification

- Focused unit tests: tab-kind migration from v1, collection-tab materialization without a
  scan, favorites kind split, folder-pin versus file-favorite routing, label-ID stability
  across rename, and behavior when a referenced label is deleted.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets for each sub-view — favorite videos, favorite
  images, the label catalog, a selected label's files, and an empty favorites state — with
  affected PNG and `layout.json` artifacts opened and directly inspected for overflow, overlap,
  cramped spacing, and wasted space.
- Receipt-backed model proof: `media_tabs` reports tab kind and sub-view; opening, switching,
  and closing a collection tab is provable without foreground activation, with `ui_snapshot`
  confirming the visible result.
- Assert no scan is started and no filesystem enumeration occurs on collection-tab activation.
- Assert playback works from a collection tab through the WP-065 surface reconciler in both
  Library and Viewer placement.
- Independent high-risk adversarial review of the schema migration, tab-kind branching, label
  lifecycle, and favorites data preservation.

</topic>
