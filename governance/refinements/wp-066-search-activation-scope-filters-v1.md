---
file_id: REF-WP-066-SEARCH-ACTIVATION-SCOPE-FILTERS-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-066" summary="Make search results activatable into the current or a new tab, add a folder-only search scope toggle, and add additive plus subtractive filters." updated_at="2026-08-12">

## Operator request

Verbatim operator items folded into this packet:

- "search result appear but are not clickable. idealy it should open a new window/tab or open in current window/tab"
- "search local folder only checkmark/toggleable"
- "add filter and subtract filter: tags, labels, fav, words, media type"

## Interpretation

- A search result must be activatable. Primary activation targets the current Media tab
  (select and reveal the file). A secondary explicit activation opens the result in a new
  Media tab. "Window" and "tab" are the same operator concept here because WP-063 made the
  Media tab the document container; no new OS window is introduced.
- Search scope must be operator-controlled and independent of the scan's recursive flag, so
  the operator can hold a recursive inventory and still search only the folder in view.
- Filters must support both inclusion and exclusion (subtraction) over tags, labels,
  favorites, free words, and media type.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-066" summary="Autocomplete rows carry no file identity, search scope does not exist as a concept, and the chip parser is AND-only with four prefixes and no favorites term." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

### Result activation

- There is no separate search-results widget. Search reorders and filters the main
  thumbnail grid through an index vector: `product/src/ui.rs:5696` computes
  `self.media_display_indices(ui.ctx(), lane_id)` and passes it to
  `draw_media_library_panel` at `product/src/ui.rs:5850`.
- Grid tiles are already fully clickable: `product/src/ui.rs:7337-7359` allocates
  `ui.interact(tile_rect, id, Sense::click())` per tile; click is captured at
  `product/src/ui.rs:7418-7423`, double-click at `product/src/ui.rs:7424-7426`, and applied
  through `media_apply_tile_click` at `product/src/ui.rs:7917-7963`.
- The surface that renders rows and **cannot** activate them is the autocomplete popup at
  `product/src/ui.rs:6604-6647`. It renders `file:` / `folder:` / `tag:` / `label:` rows and
  does handle `row.clicked()` at `product/src/ui.rs:6619-6625`, but the only effect is text
  substitution into the query box (`product/src/ui.rs:6629-6644`).
- Root cause: `Suggestion` is `FileName(String) | Tag(String) | Label(String) | Folder(String)`
  at `product/src/media_search.rs:923-932`, and `insert_text()` at
  `product/src/media_search.rs:947-953` returns a bare string. A suggestion row carries no
  path, no canonical key, and no source index, so a click cannot resolve, select, or open the
  file. A file **name** is not sufficient: names are not unique across a recursive inventory.
- Dead surface to avoid regressing: the Compare-pane file list at
  `product/src/ui.rs:13219-13270` is clickable but is never search-filtered
  (`product/src/ui.rs:13006-13008`), and its "No media found for this search." label at
  `product/src/ui.rs:13225` is unreachable text.

### Search scope

- No search-scope control exists. Search always runs over whatever `lane.files` holds
  (`product/src/ui.rs:6683`, `product/src/ui.rs:6933`).
- The recursive flag is scan-scoped, not search-scoped: `CompareLane.recursive`
  (`product/src/ui.rs:141`, default `true` at `product/src/ui.rs:259`), persisted per tab as
  `MediaTabViewport.recursive` (`product/src/media_tabs.rs:84`, default `true` at
  `product/src/media_tabs.rs:107`), saved at `product/src/ui.rs:11827` and restored at
  `product/src/ui.rs:11957`.
- It is consumed only by the scan: `product/src/ui.rs:1385` and `product/src/ui.rs:1430` feed
  `collect_media_paths_for_compare` (`product/src/ui.rs:14027-14032`, walk branches at
  `product/src/ui.rs:14141` and `product/src/ui.rs:14162`).
- Both UI affordances force a full rescan: the "Tree" toggle at
  `product/src/ui.rs:6355-6367` sets `request.scan = true`, and the Compare "Include
  subfolders" checkbox at `product/src/ui.rs:13128-13134`.
- Consequence: today the operator cannot search only the current folder while keeping the
  recursive inventory; turning Tree off discards the inventory and rescans. The search hint
  text already claims folder scope and is therefore misleading:
  `product/src/ui.rs:6452` `.hint_text("search selected folder…  (tag:x label:selects)")`.

### Filter grammar

- Parser: `parse_query` at `product/src/media_search.rs:110-142`. Tokens are whitespace-split
  and quote-aware (`product/src/media_search.rs:59-81`); values are unquoted by
  `trim_matches('"')` (`product/src/media_search.rs:84-86`); the whole token is lowercased
  before prefix matching, so keys and values are case-insensitive.
- Exactly four chip prefixes exist: `tag:`, `label:`, `note:`, `kind:`. Unknown prefixes and
  unknown `kind:` values fall through to free text.
- No negation exists in any form. A leading `-` is not stripped anywhere, so `-tag:x` fails
  `strip_prefix("tag:")` and degrades into a free-text term that fuzzy-matches file names.
- No favorites term exists. `MediaQuery` at `product/src/media_search.rs:25-32` carries only
  `text / tags / labels / kinds / notes_contain`, and the index build at
  `product/src/ui.rs:6700-6733` passes tags, notes, labels, and `is_video` only — favorites
  are never joined into the search index.
- Combination semantics are AND across chips with OR only inside `kinds`: legacy
  `passes_chips` at `product/src/media_search.rs:155-197` (kinds `any` at
  `product/src/media_search.rs:187-195`) and the indexed mirror
  `IndexedRowMeta::passes_chips` at `product/src/media_search.rs:394-428`.
- `label:` already matches stable ID **or** current visible name because the index injects
  both (`product/src/ui.rs:6713-6721`) — reuse this, do not add a second alias mechanism.
- Favorites are available in memory without a new query: `load_media_metadata` at
  `product/src/ui.rs:11725-11753` performs one `list_meta_by_key(None, None)` pass, and
  `media_db.rs:679-689` shows favorite rows are already included in that key set.

### Existing test coverage

- `product/src/media_search.rs` tests start at line 1430 and cover chip extraction (1435),
  AND semantics (1464), multi-label alias matching (1496), fuzzy ranking (1519), indexed and
  legacy parity (1578), cancellation (1673, 1741), and 141,400-row scale (1777, 1983).
- No test covers negation, favorites-as-filter, or search scope. These are new test surfaces.

## Current external sources checked

- voidtools Everything search syntax defines the field-tested desktop grammar for this exact
  problem: `!term` for NOT, space for AND, `|` for OR, `<>` for grouping, and the documented
  term shape `[!][search-modifier:][search-function:]<text>`:
  <https://www.voidtools.com/support/everything/search_syntax/>
- voidtools exclusion threads confirm operators reach for a leading NOT character on the term
  rather than a separate exclusion UI:
  <https://www.voidtools.com/forum/viewtopic.php?t=6355>
- GitHub code search documents the widely-learned `-` prefix for excluding a qualifier
  (`-language:js`), which is the other dominant field convention:
  <https://docs.github.com/en/search-github/searching-on-github/searching-code>
- Microsoft Win32 tab guidance requires that changing tabs has no side effects and that
  page-scoped controls live inside the tab content, which constrains where a per-tab scope
  toggle may live: <https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-tabs>
- egui 0.27 documentation for `Response::clicked` and modifier state
  (`InputState::modifiers`) confirms modifier-qualified activation is available without a new
  input layer: <https://docs.rs/crate/egui/0.27.0>
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no directly transferable
  Rust/egui local-media search-grammar implementation; the desktop-search conventions above
  remain the field basis.

## Selected approach

- Give suggestions resolvable identity. Extend `Suggestion::FileName` to carry the canonical
  key plus the source index it was derived from, keeping `insert_text()` behavior for chip
  suggestions. Activation resolves the index against the **same generation** that produced
  it, exactly as `media_apply_tile_click` already does, and falls back to a key lookup when
  the generation has advanced.
- Two activation verbs on a result row: plain click / Enter selects and reveals it in the
  current tab; Ctrl+click / Ctrl+Enter (and a context-menu item) opens it in a new Media tab
  rooted at the file's parent folder with that file selected. This reuses the existing
  WP-063 "Open in new tab" commit path rather than adding a second tab-creation route.
- Add a per-tab `search_scope` value (`folder` | `tree`) that is **independent of the scan
  recursive flag**. `folder` filters the existing inventory by parent directory at query
  time and never triggers a rescan. This is the point of the operator's request: keep the
  recursive inventory, narrow the search.
- Extend the chip grammar with a leading negation marker accepted as either `!` or `-`
  (accept both; Everything teaches `!`, GitHub teaches `-`), applied uniformly to `tag:`,
  `label:`, `kind:`, `note:`, the new `fav:`, and bare free words. Negated terms are
  subtractive and AND-combined after inclusion filtering.
- Add a `fav:` chip (`fav:1` / `fav:0`, plus bare `fav:` meaning favorited) sourced from the
  already-loaded favorites key set, and extend the search index row to carry a `favorite`
  bit so the indexed and legacy paths stay behaviorally identical.
- Keep AND-of-all-terms as the combination rule. Do not add `|` / `<>` grouping in this
  packet: the operator asked for add and subtract filters, and grouping is a separate
  grammar surface that would need its own precedence tests.
- Correct the misleading `hint_text` at `product/src/ui.rs:6452` so it states the active
  scope.

## Rejected options

- Building a separate search-results list panel: the grid is already the result surface and
  already virtualizes to 141k rows; a second surface would duplicate selection, thumbnails,
  and playback ownership.
- Resolving a clicked suggestion by file **name**: names are not unique in a recursive
  inventory and would silently open the wrong file.
- Reusing the `recursive` scan flag as the search scope: it destroys the inventory and forces
  a rescan, which is precisely the behavior the operator is trying to avoid.
- Adopting the full Everything grammar (`|`, `<>`, wildcards) in this packet: unrequested
  scope, and grouping precedence needs its own contract.
- Adding a separate exclusion input box or a filter-builder dialog: heavier UI, and the field
  convention operators already know is an inline negation prefix.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-066" summary="Stale-index activation, negation ambiguity with filenames, scope drift, and index/legacy divergence have explicit controls and proof gates." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **Suggestion activation opens the wrong file after the inventory changes.** A stored source
  index is only valid inside its generation. Control: stamp the suggestion with the
  generation that produced it, resolve against that generation, fall back to canonical-key
  lookup, and report a visible "no longer available" state instead of opening a neighbor.
  Test with a scan completing between suggestion render and activation.
- **`-` negation collides with real filenames.** Media filenames very commonly begin with
  `-`. Control: negation is only recognized on a token whose remainder is a known chip
  (`tag:`/`label:`/`kind:`/`note:`/`fav:`) or on a quoted/bare word where the operator
  typed the marker as a standalone prefix; `"-foo"` in quotes is always literal. Add explicit
  tests for `-foo.jpg`, `"-foo"`, `-tag:x`, and `!tag:x`.
- **Negation makes an empty result look like a bug.** Control: when subtractive terms remove
  every row, the status line must state how many rows were excluded and by which terms, not
  render a bare empty grid.
- **Indexed and legacy filter paths diverge.** Two `passes_chips` implementations already
  exist (`media_search.rs:155-197` and `media_search.rs:394-428`). Control: extend both and
  keep the existing parity test pattern (`immutable_index_matches_legacy_rank_for_all_modes_and_limits`,
  line 1578) covering negation and `fav:`.
- **Favorites bit goes stale in the index.** Favorites mutate through toggles while the index
  is live. Control: treat the favorite bit as index-invalidating on mutation, or evaluate
  `fav:` against the live favorites key set rather than a frozen copy; test toggle-then-search.
- **Folder-only scope silently disagrees with what the operator sees.** Control: scope is
  per-tab persisted state, visibly reflected in the toggle and in `hint_text`, and proven by
  a receipt-backed intent that reports scope, matched count, and inventory count.
- **Scope toggle accidentally triggers a rescan.** This would reintroduce the exact defect.
  Control: assert scan ID and inventory length are unchanged across a scope toggle.
- **Per-tab scope leaks across tabs.** Control: store scope in `MediaTabViewport` alongside
  `recursive`, save/restore through the existing paths at `product/src/ui.rs:11827` and
  `product/src/ui.rs:11957`, and test switching between tabs with different scopes.
- **Performance regression at 141k rows.** Negation and scope add per-row predicates.
  Control: extend the existing 141,400-row ranking tests to the new predicates and keep
  parent-directory comparison on pre-normalized keys rather than re-deriving paths per row.
- **New tab per result explodes tab count.** `MAX_TABS = 256` (`product/src/media_tabs.rs:19`).
  Control: honor the cap with a visible refusal, and reuse an existing tab already rooted at
  the same folder instead of creating a duplicate.

## Verification

- Focused unit tests: negation parsing for `!`/`-` across every chip and free words, literal
  `-` filename handling, `fav:` inclusion and exclusion, AND-combination of additive and
  subtractive terms, scope filtering by parent directory, and per-tab scope persistence.
- Indexed-versus-legacy parity tests extended to negation, `fav:`, and scope.
- Scale test: the existing 141,400-row fixtures re-run with negation, `fav:`, and folder
  scope applied.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic `facial-cli ui-inspect` presets for: autocomplete popup with activatable
  rows, folder-scope toggle on and off, and a query mixing additive and subtractive terms.
  Affected PNG and `layout.json` artifacts are opened and directly inspected.
- Receipt-backed proof without foreground activation: `media_search` intent reports scope,
  additive terms, subtractive terms, matched count, and inventory count; a result-activation
  intent proves current-tab selection and new-tab opening, with `ui_snapshot` confirming the
  visible outcome.
- Assert scan ID and inventory length are unchanged across scope toggles and result
  activation.
- Independent high-risk adversarial review of query parsing boundaries, stale-generation
  activation, favorites mutation races, and tab-cap behavior.

</topic>
