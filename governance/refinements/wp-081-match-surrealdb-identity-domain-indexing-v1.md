---
file_id: REF-WP-081-MATCH-SURREALDB-IDENTITY-DOMAIN-INDEXING-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="active" version="2" wp="WP-081" summary="Create the durable Match identity graph, regenerable vector index, reason-aware pause model, revision-fenced caches, and explicit restart-resumable background indexing pipeline inside the shared SurrealDB root." updated_at="2026-08-22">

## Scope contract

The user owns People, Looks, manual regions, `operator_confirmed` assignments/evidence, trusted-reference authorization/membership, constraints, and operator pause intent. The explicit assignment/evidence states are `suggestion`, `committed_strict_automatic`, and `operator_confirmed`: a suggestion has no committed Person assignment; a strict-automatic assignment is model-derived and model-generation-bound; only explicit manual assignment or a `Same` review transition creates durable operator-confirmed Person evidence. Review transactions are exact: `Same` confirms Person evidence without trusted authorization; `Different` rejects/removes the candidate and adds a face-to-candidate-Person cannot-link; `Not sure` changes neither identity truth nor constraints; `This is not <name>` invokes the same `Different` transaction. Models own rebuildable detections, embeddings, clusters, suggestions, strict-automatic assignments, and materialized projections. Indexing is explicit, bounded, visible through structured state, and never triggered by folder navigation.

The durable identity domain is an explicit graph, not a mandatory chain. Each Look belongs to exactly one Person, and a Person owns zero or more Looks. A FaceObservation has at most one committed Person assignment and exactly one when its state is `committed_strict_automatic` or `operator_confirmed`; an assigned observation is placed in exactly one of that Person's `Unsorted` membership or one Look owned by that same Person. Each TrustedTemplateSet belongs to exactly one Look and selects zero or more explicitly authorized FaceObservations assigned to that same Look. A Person may retain several substantially different Looks without fragmenting identity. Any assignment route that supplies only a Person ID deterministically places the observation in that Person's `Unsorted` membership; moving it to an existing Look or using `Same person, new look` is explicit. An operator-confirmed FaceObservation joins the Person's identity pool, but does not become a trusted automatic-matching reference merely because it was named or assigned. TrustedTemplateSet enrollment is a separate durable decision requiring independent explicit trusted-reference authorization, explicit Look membership, alignment, quality, model-generation, pose/diversity, and provenance evidence. Passing eligibility gates never auto-enrolls an observation. A low-quality or invalid-alignment manual observation remains assignable while being ineligible to teach Match.

Pause is composed rather than represented by one lossy boolean. Persist `desired_mode` as `running | operator_paused`, combine it with an extensible runtime set of non-persisted holds, and keep both separate from job lifecycle (`queued | running | pausing | paused | blocked | cancelled | completed | failed | partial | retrying`). Admission is derived only when desired mode is running, the hold set is empty, lifecycle is runnable, and a resource budget is available. `immersive_fullscreen` is the first required transient reason: it prevents the next automatic stage from starting, releases only itself on exit, and cannot resume an operator-paused or terminal/failed job. Cancel, complete, and failure are lifecycle outcomes, never pause reasons. Manual reads and operator-owned correction data remain available while automatic work is paused.

Rendering and autocomplete consume caches only. Publish a bounded per-media People projection off the paint path and maintain a catalog-revisioned in-memory Person/alias search index whose results carry stable Person IDs. Async indexing, identity writes, projection refreshes, and autocomplete results carry the applicable job ID, canonical media key/fingerprint, schema/model generation, and identity/catalog revision; mismatches are rejected instead of overwriting newer truth.

WP-081 also owns a Match-specific resource governor beyond shared filesystem permits and `WorkClass::Background`. It admits work only under bounded item, byte, and concurrency budgets covering CPU detection/inference, decoded-image and crop memory, GPU/VRAM when an accelerator is used, SurrealDB write batches and vector-index builds, and aggregate inter-stage queues. Backpressure is explicit, visible work retains priority, and every success, error, stale result, pause, cancellation, or shutdown path releases its leases.

## Spec anchors

- Shared application store: WP-078 and `specs/app-spec.md` section 15.
- Background I/O: `product/src/media_io.rs` and WP-057/WP-069.
- Stable collection semantics: WP-061/WP-067.
- Immersive Viewer contract: `specs/app-spec.md` section 15, WP-058.
- Match durable-domain contract: `specs/app-spec.md` section 17.3.

</topic>

<topic id="research-basis" status="active" version="2" wp="WP-081" summary="Field systems separate person identity from observations and embeddings; Facial additionally separates identity-pool membership from trusted references and keeps UI reads revision-fenced and cached." updated_at="2026-08-22">

## Sources and selected approach

- Immich face/person separation and incremental workflow: https://docs.immich.app/features/facial-recognition/
- SurrealDB vector indexes: https://surrealdb.com/docs/learn/data-models/vector-search/vector-indexes
- SurrealDB index definition/query plans: https://surrealdb.com/docs/reference/query-language/statements/define/indexes

Use typed `person`, `look`, `trusted_template_set`, `face_observation`, `face_embedding`, `assignment`, `constraint`, `operation`, and `index_job` records. A Look groups a known appearance under one Person; it is not a separate identity. Assignment/evidence state distinguishes review-only `suggestion`, model-derived generation-bound `committed_strict_automatic`, and durable `operator_confirmed`; only the last joins the confirmed identity pool. Typed operations preserve the exact `Same`, `Different`, `Not sure`, and `This is not` effects without conflating confirmation, cannot-link creation, and defer-only review state. Person-only assignment enters `Unsorted`. Separately authorized, explicitly Look-bound, eligibility-gated TrustedTemplateSet membership controls which observations may teach automatic matching, and eligibility never substitutes for authorization.

Store vectors natively with HNSW initially, exact-rerank candidates, and retain generation coexistence/rollback. Do not hex-encode vectors through the compatibility KV facade. Materialize bounded per-media People projections and a normalized Person/alias index outside render and keystroke paths. Publish them only when their identity/catalog and media/model revision fences still match.

</topic>

<topic id="red-team" status="active" version="2" wp="WP-081" summary="Rebuild damage, trust-pool contamination, pause-state loss, stale async publication, approximate-neighbor misses, cancellation duplication, and path relocation are the primary persistence risks." updated_at="2026-08-22">

## Risks and controls

- Rebuild overwrites truth: structural durable/derived separation and locked manual rows.
- ANN miss: recall-at-K measurement and exact rerank; misses remain unidentified.
- Interrupted job duplicates/skips: idempotent per-asset stages, persisted cursors, reconciliation.
- Rename/move disconnects faces: canonical portable identity plus proven move mapping/content identity.
- Large vector records bloat the KV facade: dedicated typed tables and measured index/storage budgets.
- Confirmed low-quality face contaminates matching: assignment and identity-pool membership do not imply TrustedTemplateSet membership; require explicit eligibility evidence and provenance.
- Eligible face silently auto-enrolls: require independent explicit trusted-reference authorization plus explicit Look membership; gate success alone cannot create TrustedTemplateSet membership.
- Automatic match becomes operator truth: keep `committed_strict_automatic` model-derived and generation-bound until explicit `Same`; suggestions remain unassigned.
- Person-only assignment silently chooses the wrong appearance family: place it in `Unsorted` until an explicit existing-Look move or `Same person, new look` action.
- Different Looks fragment one identity: Looks remain subordinate to a stable Person and can contribute separate pose/style coverage without becoming separate People.
- Fullscreen exit loses an existing operator pause: compose persisted operator intent with independent transient reasons; removing one hold cannot remove another.
- Execution state collapses intent, holds, and outcomes: persist and expose the three axes independently, define derived admission and transition precedence, and never let hold removal revive cancelled/completed/failed work.
- Late model/cache result overwrites a correction or renamed Person: compare all applicable media, job, model, identity, and catalog revisions inside the commit/publication boundary.
- Viewer or autocomplete stalls large folders: render from bounded per-media projections and filter an in-memory Person/alias index; never query storage per frame or per keystroke.
- Filesystem permits stay healthy while other resources exhaust the machine: add Match item/byte/concurrency budgets for CPU/inference, decoded memory, optional GPU/VRAM, SurrealDB writes/index builds, and aggregate queues; backpressure and reconcile every lease on every terminal path.

</topic>

<topic id="microtask-plan" status="active" version="2" wp="WP-081" summary="Land identity/trust schemas and revision invariants before pause-aware workers, cached UI projections, and vector queries." updated_at="2026-08-22">

## Microtasks

1. Define Person, Look, Unsorted, TrustedTemplateSet, FaceObservation, the three assignment/evidence states, constraint, operation, job, revision, and generation schemas plus migrations and invariants.
2. Implement repositories and transactional durable/derived guards, including strict-automatic generation ownership, confirmed identity-pool membership, and independently authorized trusted-reference membership.
3. Implement persisted desired mode, extensible transient hold reasons, independent job lifecycle, deterministic derived admission/transition precedence, and the non-persisted `immersive_fullscreen` hold.
4. Add native vector index and exact rerank path with query-plan proof.
5. Implement staged idempotent background jobs, the Match item/byte/concurrency resource governor, backpressure, safe-boundary pause/cancel, recovery, lease reconciliation, and transaction-time revision fencing.
6. Build off-render-path per-media People projections and the catalog-revisioned stable-ID Person/alias autocomplete index.
7. Add structured intents, receipts, failure reasons, stale-result diagnostics, and redacted state probes.
8. Prove assignment/evidence transitions, Unsorted/Look behavior, explicit trust authorization, nested pause reasons, stale-result rejection, restart, relocation, model coexistence, rollback, cancellation, resource ceilings/lease release under stress, and cache-only UI reads.

</topic>
