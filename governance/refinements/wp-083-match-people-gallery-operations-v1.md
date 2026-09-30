---
file_id: REF-WP-083-MATCH-PEOPLE-GALLERY-OPERATIONS-V1
file_kind: refinement
updated_at: "2026-08-24"
---

<topic id="operator-request" status="active" version="1" wp="WP-083" summary="Deliver Match as a quiet, explicit media feature: name a person and open their gallery without turning visual browsing into an annotation prompt stream." updated_at="2026-08-22">

## Product contract

The feature is named **Match**. Its primary sections are **People**, **Suggestions**, and **Unidentified**. **Strict matching** is the default policy and explains that uncertain faces remain unidentified. Active Match remains silent in ordinary Media browsing: it never opens a prompt, suggestion overlay, modal, tab, or text field because a photo was selected or a background result arrived. Suggestions accumulate only in Match -> Suggestions.

**Match -> People** is the canonical People manager and gallery. **Settings -> Match** owns processing policy and state (roots, exclusions, pause/resume, failures) and links to Manage people; it does not duplicate the catalog. Operator pause stops automatic work without disabling cached People browsing or explicit manual correction.

The synthetic 10,000-People fixture belongs only to the virtualized **Match -> People** manager, which must open within 200 ms p95 under the WP-087 benchmark protocol without loading or rendering all People per frame. **Settings -> Match** contains processing controls plus the **Manage people** route and materializes zero People rows, covers, counts, or catalog projection. Route feedback must be visible within 100 ms p95; the destination open is then measured against the separate Match -> People budget.

## Spec anchors

- Collection tabs: WP-067 and `product/src/media_tabs.rs`.
- Viewer metadata band: WP-072.
- Background-safe intents and inspection: CODEX sections 7.1/7.2.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-083" summary="Google, Samsung, Immich, PhotoPrism, and digiKam converge on a People catalog and person galleries; Facial can improve trust through explicit jobs and partial results." updated_at="2026-08-23">

## Sources and selected approach

- Google Photos face groups: https://support.google.com/photos/answer/6128838
- Apple Photos optional face-name display and inline naming: https://support.apple.com/en-nz/guide/photos/phtad9d981ab/mac
- Samsung Gallery: https://www.samsung.com/us/support/answer/ANS10002535/
- PhotoPrism People: https://docs.photoprism.app/user-guide/organize/people/
- digiKam People view: https://docs.digikam.org/en/left_sidebar/people_view.html
- SurrealDB `GROUP BY` and latest-record-per-group query pattern: https://surrealdb.com/docs/reference/query-language/clauses/group
- SurrealDB 3.2.4 compound-index and `WITH INDEX` planner regressions in the vendored
  `surrealdb-core-3.2.4/tests/select.rs` source.

Reuse the Media collection viewport and cached metadata projection. Provide explicit roots/exclusions, resumable job controls, partial results, failures, naming, aliases, covers, visibility/favorites, and dynamic person galleries. Follow the field pattern of a separate People surface plus explicit per-photo editing; do not make background recognition synonymous with interruption.

For catalog cover and person-gallery source resolution, select the newest source record independently per `media_key` with SurrealDB's documented grouped-record `.map(...)` pattern. This avoids a global pre-group `LIMIT` starving older media keys while retaining a bounded per-page result.

For high-cardinality Person galleries, denormalize the canonical `media_key` onto each
assignment and use a composite `(person_id, media_key)` index for distinct ordered paging.
This reuses the face key already loaded during assignment and avoids both an unbounded
Person-wide face-ID vector and a new hot Person-media aggregate document. Schema migration
must backfill from canonical faces and fail closed on an orphan.

Resolve the bounded gallery/cover page through a separate media-key-leading job-asset
index and name that index in the query. Bind each discovery mutation to a single-use
store-session/job/media permit with serialized-payload authorization. Admit iterator advances,
metadata probes, and snapshots separately under the shared background filesystem ceiling;
carry a single-use observation across Pausing and acquire the writer only after I/O. Keep the
owned snapshot's zero-writer byte authority live through exact-byte resize and atomic Detect
transfer so saturated inference cannot accumulate unaccounted encoded buffers. During media rekey,
move persisted Person covers and catalog revisions in the same transaction as faces,
assignments, projections, and job assets. Sensitive capture markers must be size-bounded
before allocation and retire superseded video sidecars while the transition marker still
binds both generations.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-083" summary="Implicit scans, unsolicited prompts, duplicate managers, misleading progress, paint-loop database work, and model-centric UX are the central usability failures." updated_at="2026-08-22">

## Risks and controls

- Silent library scan: opt-in roots and explicit start only.
- Endless inbox: prioritized bounded suggestions, dismiss/never-ask-again, no completion quota, and no prompts outside the review surface.
- Browsing interruption: no unsolicited overlay, modal, tab change, notification, text focus, or app activation.
- Split People authority: Match -> People is canonical; Settings only controls analysis and routes into it.
- Settings freezes while preparing the destination: Settings renders no People rows/counts/covers, and route feedback is measured separately from the virtualized Match -> People open.
- False completion: partial/settled flags and exact failed/skipped counts.
- Retry erases unresolved discovery evidence: retain `unavailable:` placeholders until
  positive re-observation and persist directory/root walk errors.
- Pause races become failures: accept the one in-flight bounded discovery result while
  `pausing`, then settle Paused at the next admission boundary; reject new discovery
  observations once Pausing begins, govern iterator/stat/snapshot I/O under the shared ceiling,
  avoid holding writers across filesystem latency, retain continuous byte authority while Detect
  is saturated, and prove concurrent saturation plus zero residual usage.
- High-cardinality gallery materializes every face ID: composite assignment media-key
  paging plus a media-key-leading job-asset source index, with query-plan proof under
  600 distinct media and 1,000 unrelated historical assets.
- Rekey drops the selected cover: atomically move Person cover references and catalog
  revisions, then reopen and verify both catalog cover and gallery source resolution.
- Sensitive replacement leaves an unbound prior sidecar or allocates an adversarial
  marker: enforce a 64 KiB pre-read ceiling, bounded strings, transition-bound retirement,
  successful A-to-B directory enumeration, pending-first recovery, and exact expected-pending
  verification when identical framebuffer bytes make both generations complete. First-generation
  failures conditionally roll back only an unchanged app-created incomplete transition and
  hash-bound sidecar. Restart cleanup additionally requires an absent framebuffer; any present
  entry is ambiguous and preserves the full state. Artifact hashing rejects all Windows reparse points.
- UI stutter: cached projection; no database/filesystem work in paint.
- Machine-only workflow: every action has an intent, receipt, diagnostic, fixture, and Manual route.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-083" summary="Land catalog and job routes before building the visible Match workspace." updated_at="2026-08-22">

## Microtasks

1. Add catalog/job intents, receipts, snapshots, and projections.
2. Add Match navigation and People/Suggestions/Unidentified view state with the pull-only/no-prompt contract.
3. Add Settings -> Match plus indexing setup, exclusions, reason-aware progress/control, failures, retry, and the Manage people route.
4. Add naming, aliases, covers, hide/favorite, and person galleries.
5. Add deterministic fixtures and live background-safe navigation, including the 10,000-People Match -> People fixture and a Settings route fixture that proves zero People-row materialization.
6. Update the built-in Manual and prove a fresh-context walkthrough.

</topic>
