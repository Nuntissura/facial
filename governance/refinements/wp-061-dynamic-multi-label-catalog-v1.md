---
file_id: REF-WP-061-DYNAMIC-MULTI-LABEL-CATALOG-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-061" summary="Replace the fixed single-label palette with dynamic reusable labels and multiple assignments per file." updated_at="2026-08-09">

## Operator request

- Create a label with a unique name and color/hex when a file has no label.
- Add existing labels from a dropdown and remove assigned labels from that same manager.
- Manage all labels in Settings: view, create, rename, recolor, and remove.
- Allow multiple labels per file and show their colors at the Library thumbnail's top-right corner.
- Keep tags and notes behavior intact and preserve large-folder responsiveness.
- Defer the broader search overhaul; implement only compatibility required to prevent label search regression.

## Spec anchors and supersession

- Anchors: `specs/app-spec.md` Media browser section and WP-058 label section.
- This packet supersedes only the fixed-seven/single-assignment label decisions in WP-042 and WP-058; their redb, portability, autosave, modal, and performance contracts remain authoritative.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-061" summary="A versioned stable-ID catalog plus per-path ID arrays gives flexible labels without render-frame database work." updated_at="2026-08-09">

## Current evidence

- `color_labels` currently stores one path-to-string value; `MediaMeta` exposes one `label`.
- `color_label_definitions_v1` requires exactly seven built-in IDs and the UI can only rename/recolor them.
- UI caches metadata in `Arc<BTreeMap>` values and performs one key lookup per visible tile; this is the performance shape to preserve.
- Search currently compares one label exactly and autocompletes compile-time IDs.

## Sources checked

- redb documents multimap tables for multiple values per key: <https://docs.rs/redb/latest/redb/struct.MultimapTableDefinition.html>
- redb transactions provide the atomic boundary needed for catalog/assignment changes: <https://docs.rs/redb/latest/redb/>
- egui's immediate-mode guidance warns against laying out large offscreen lists every frame; bounded visible work is required: <https://github.com/emilk/egui>
- digiKam separates reusable labels from ratings and supports label-driven browsing: <https://docs.digikam.org/en/left_sidebar/labels_view.html>
- Adobe Bridge demonstrates the failure of coupling assignment identity to editable label names; Facial keeps immutable IDs: <https://helpx.adobe.com/bridge/desktop/organize-and-find-files/tag-and-find-files/label-and-rate-files.html>

## Selected approach

- Persist an arbitrary-length v2 catalog of immutable label IDs, editable unique names, canonical unique `#RRGGBB` colors, and stable ordering.
- Preserve the seven existing IDs during migration; new IDs are opaque and never derived from mutable names or reused after deletion.
- Store each asset's ordered/deduplicated label-ID vector as versioned JSON in the existing label value table; accept a legacy plain string as a singleton during migration and reads.
- Keep one in-memory path-to-small-vector map plus an ID-to-definition map. Library paint performs one path lookup and bounded badge painting only.
- Create, update, and assignment operations are atomic. Deleting an assigned label requires usage visibility and explicit confirmation, then removes the definition and assignments atomically.
- Add live-GUI intent/receipt support so models can manage labels while the GUI holds the exclusive redb lock.
- Current search receives only membership compatibility: `label:` resolves current name or stable ID and matches any assigned ID. Ranking/index redesign remains out of scope.

## Rejected options

- Mutable label names as asset values: rename would orphan assignments.
- One decoder/database query or catalog scan per thumbnail: violates the 50k-item render contract.
- Silent deletion of an in-use label: loses operator metadata meaning without an explicit decision.
- Full search redesign in this packet: conflicts with the operator's requested sequencing.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-061" summary="Migration, deletion, UI scale, search compatibility, and model-operation failures have explicit controls." updated_at="2026-08-09">

## Risks and controls

- Migration loses assignments: parse legacy values, migrate in one transaction with a marker, and test reopen/idempotence.
- Delete orphans asset IDs: show usage count, require confirmation, and remove catalog plus memberships atomically.
- Duplicate meaning: reject case-insensitive duplicate names and duplicate canonical hex values before commit.
- Settings edits stutter: stage/debounce text/color changes instead of writing redb for every keystroke.
- Many badges obscure tile controls: reserve the top-right badge lane, bound visible swatches, and render `+N` overflow.
- Multi-select semantics are ambiguous: show all/some/none state and explicit add-to-all/remove-from-all operations.
- Search stops matching labels: resolve dynamic names/IDs and use membership tests now; do not change broader ranking.
- GUI lock blocks model commands: route mutations through the live intent/receipt workflow.

## Verification

- DB tests: legacy singleton migration, reopen/idempotence, arbitrary CRUD, uniqueness, multi-add/remove, delete cleanup, relocation, and failed-write atomicity.
- API/live-intent tests: list/create/update/delete, assignment add/remove/clear, invalid duplicate rejection, and receipt state.
- UI inspector presets: no labels, multiple labels, manager with 20+ labels, narrow/high-font layouts, delete confirmation, top-right badges, and favorite/play non-overlap.
- Search compatibility tests: either assigned label matches by name or ID and multiple label chips combine deterministically.
- Synthetic 50k-item proof: zero DB/filesystem calls in paint, bounded visible badge work, metadata hydrate measurement, and no more than 10% steady-frame regression.

</topic>
