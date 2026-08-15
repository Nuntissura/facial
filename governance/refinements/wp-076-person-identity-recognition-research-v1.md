---
file_id: REF-WP-076-PERSON-IDENTITY-RECOGNITION-RESEARCH-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-076" summary="Investigate better face/ID recognition for facial and design an iOS-Photos-style named-person browsing system that does not interfere with the existing tag system." updated_at="2026-08-15">

## Operator request

Verbatim operator items folded into this packet (2026-08-15):

- "then i want to investigate better face and ID recognition features for facial, so better identity
  tools are available for other models. but also i envision looking for a name that is linked to an
  identity and can browse all photos with that person in it. i think this is kind of a back end tag
  system of some kind but i do not want it to interfere with the tag system we just created. i have
  seen this feature in the ios photo app i think i think it is cool and handy. it is also good for
  sorting large scraping batches for kpop content for example."

## Interpretation

- This packet is an **investigation and design deliverable, not product code**: the operator said
  "investigate". Its outputs are a current research basis, a concrete design proposal grounded in
  the existing engine, and promotable follow-up work-packet stubs.
- Two connected goals: (1) better face/ID recognition capability usable by other models (engine
  quality and tooling), and (2) a **People system**: a name linked to an identity, browse all
  photos containing that person, iOS-Photos-style, driven by a backend store that is structurally
  separate from the operator tag/label system (WP-061), sized for large scraped batches (kpop-scale
  folders).

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-076" summary="The repo already holds a real deterministic YuNet+ArcFace engine, a prior research basis, a proven separate-embedding-index pattern (CLIP), and a proven catalog+assignment pattern (WP-061); field systems (Immich, digiKam, PhotoPrism) converge on detect-embed-cluster-name with incremental clustering; the named gaps are crop-level embed, dim validation, persisted incremental clustering, and person storage." updated_at="2026-08-15">

## Baseline project evidence (inspected, current working tree)

- **The identity engine is real and deterministic but harness-side only**: YuNet 2023mar bundled
  via include_bytes (`product/src/identity.rs:67-70`), deterministic detect + NMS
  (`product/src/identity.rs:302-441`), 5-point similarity alignment to the ArcFace 112 template
  (`product/src/identity.rs:464-484`), L2-normalized embeddings (`product/src/identity.rs:227-249`),
  greedy deterministic clustering (`product/src/identity.rs:750-775`). It has **zero coupling** to
  the media-browser stack, and the string `person` appears nowhere in `product/src` - the
  namespace is free.
- **Gap 1 - no crop-level embed entry point**: every public path takes a file path and decodes
  internally (`product/src/identity.rs:172-201`); `embed_aligned` is private. Multi-face-per-image
  person indexing needs a crop/bytes entry point.
- **Gap 2 - no embedding-dimension validation**: dim is collected unchecked
  (`product/src/identity.rs:246`) and `cosine` silently truncates on mismatch
  (`product/src/identity.rs:732-739`), unlike CLIP's explicit checks
  (`product/src/media_clip.rs:28`, `:374-390`). Any persisted person store must stamp and validate
  dim per row.
- **Gap 3 - clustering is one-shot and unpersisted**: `cluster_embeddings` is greedy
  first-match-wins per run; embeddings are recomputed and dropped every run
  (`product/src/service.rs:1926-2110`, 20k cap at `:1950-1955`). A People system needs persistence
  and incremental assignment.
- **Template for the embedding index**: `clip_index.redb` - a separate regenerable redb cache keyed
  by canonical media key with mtime/size invalidation and per-row dim
  (`product/src/media_clip.rs:559-649`), built off-thread under I/O permits with progress batching
  (`product/src/ui.rs:13021-13155`), queried under Background-class permits
  (`product/src/ui.rs:13159-13305`). A `face_index.redb` with multi-face rows (bbox + embedding +
  person assignment) is a near-mechanical transplant.
- **Template for the person catalog**: the WP-061 multi-label catalog - stable IDs so rename never
  disconnects assignments (`product/src/media_db.rs:55-64`), catalog CRUD with atomic
  catalog+assignment transactions (`product/src/media_db.rs:1100-1287`), migration-marker pattern
  (`product/src/media_db.rs:1301-1379`), counted transactions required (`product/src/media_db.rs:296-347`).
  Persons must get their OWN write path - `set_meta_labels` (`:622-673`) is not extended, so a
  person operation can never rewrite a label vector. Worker-safe keying exists precisely for this:
  `media_db::canonical_key` (`product/src/media_db.rs:2019-2023`).
- **Search attachment is purely additive**: a `person:` chip slots into the prefix-dispatched
  parser beside `note:` (`product/src/media_search.rs:189-244`), `passes_chips`
  (`:286-395`), `is_empty`/`has_chips` (`:76-95`), and the external-vocabulary autocomplete
  pattern labels already use (`:1448`; `product/src/ui.rs:8200-8210`). Chip tokens are already
  stripped from CLIP text embedding (`product/src/ui.rs:13163-13165`). Unknown `person:` today
  falls through to free text, so the addition changes no existing arm.
- **Non-interference with the operator tag system is structural**: new redb tables, a new separate
  index file, a new chip arm; shared surfaces are only the SETTINGS table (distinct key prefix),
  key normalization, and the transaction counter. A collection-style People view can follow the
  WP-067 collection-tab pattern (metadata-driven rows, never scans).
- **Prior research basis**: `governance/research_face_identity.md` (2026-06) selected tract +
  ArcFace w600k (512-d) + fixed alignment for reproducibility, and codified the contracts a People
  system inherits: never fake a verdict, stamp model sha + thresholds into artifacts, thresholds
  are calibrated not guessed (`:40-55`, `:77-92`). It contains no research on named-identity
  storage, enrollment, or multi-person assignment - genuinely new ground this packet covers.
- **Plugin face features are proxies** and must not back the People system; the only real capability
  is `identity.rs` + `landmarks.rs` (verified per-plugin with source stamps).
- **Backlog overlap**: STUB-K (warm-model daemon) and STUB-L (GPU/multi-core inference) in
  `governance/backlog_field_feedback.md` bear directly on scan-scale embedding throughput
  (a 5,267-image gate took about an hour on tract CPU); kpop-batch People indexing multiplies that
  cost and this packet must size it honestly.

## Current external sources checked (initial sweep; the packet deepens this)

- Immich facial recognition: InsightFace buffalo_l detection+recognition, embeddings indexed for
  search, and a DBSCAN-derived **incremental** clustering that preserves existing named people as
  new assets arrive - the key algorithmic pattern for evolving libraries:
  <https://docs.immich.app/features/facial-recognition/>,
  <https://github.com/immich-app/immich/discussions/8347>,
  <https://huggingface.co/immich-app/buffalo_l>
- digiKam: two-phase detect-then-recognize workflow, named face tags train recognition, 8.6
  rebuilt the matcher around KNN+SVM; XMP face-region metadata is the interop surface other tools
  import: <https://docs.digikam.org/en/left_sidebar/people_view.html>,
  <https://userbase.kde.org/Digikam/Tutorials/Tagging_and_Face_Tags>
- PhotoPrism People (naming/merge/hide UX; XMP import):
  <https://docs.photoprism.app/user-guide/organize/people/>
- iOS Photos People and Pets is the operator's stated UX reference (named people, browse-by-person,
  confirmation loops improving accuracy).
- To be covered in the packet's deep-dive before any design freeze: current model landscape beyond
  ArcFace w600k (AdaFace/TransFace-class recognition heads, newer detectors) constrained by tract
  op coverage and the determinism mandate; incremental clustering literature (DBSCAN variants,
  centroid maintenance, merge/split UX consequences); on-disk vector-index needs at kpop scale
  (brute-force cosine vs ANN, informed by the existing 146k-row folder reality); GitHub/Hugging
  Face/Civitai/Reddit/X implementation evidence per GLOBAL-RESEARCH.

## Selected approach (for the investigation itself)

- Deliver `governance/research_person_identity.md` (this prose format) covering: model-upgrade
  evaluation under tract/determinism constraints; the People data model (persons catalog +
  face-instance index + person assignments, per the attachment map above); enrollment and
  correction UX patterns from Immich/digiKam/PhotoPrism/iOS mapped onto Facial's surfaces
  (People as a WP-067-style collection view, `person:` chip, Viewer band affordance); incremental
  clustering design with explicit determinism/auditability trade-offs; throughput sizing for
  kpop-scale batches with the STUB-K/STUB-L interaction stated; storage sizing; privacy/scope
  notes (local-only, no cloud).
- Produce promotable work-packet stubs (engine entry points + validation; face index + clustering;
  People UI + naming/merge; search chip + collection view) with dependencies and acceptance
  sketches, appended to the backlog per the established stub pattern.
- Record rejected directions with reasons, matching house style.

## Rejected options

- Building the People feature in this packet: the operator asked to investigate; design-first also
  respects that clustering/model choices are expensive to rewrite once persisted.
- Basing People on the proxy plugin layer: proxies are explicitly not identity evidence.
- Reusing the operator tag/label tables for person assignments: the operator explicitly forbade
  interference; structural separation is the design constraint, not a preference.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-076" summary="The investigation itself has failure modes: stale or shallow research, designs that ignore tract/determinism limits, throughput hand-waving at kpop scale, and scope creep into implementation; each has an explicit control." updated_at="2026-08-15">

## Risks, failure scenarios, and controls

- **Research recommends models tract cannot run or that break determinism.** Control: every
  candidate model is checked against tract op coverage and the research_face_identity.md
  reproducibility contract before it may appear in the recommendation; unverifiable claims are
  labeled UNVERIFIED.
- **Throughput is hand-waved and the design collapses on a real kpop batch.** Control: the design
  must include measured or clearly-derived per-image embed cost on this workstation's CPU, sized
  against the operator's real 146k-row folder, and state the STUB-K/STUB-L dependency explicitly
  if interactive indexing is not honest on CPU alone.
- **The People store design quietly couples to the tag system.** Control: the design document must
  contain a non-interference section proving separation at the table/write-path/chip level against
  the attachment map in this refinement.
- **Incremental clustering design loses operator corrections on re-cluster.** Control: named
  assignments are contractually immutable under re-clustering in the design (Immich's
  preserve-people property is the field precedent); merge/split flows must be explicit.
- **Scope creep into implementation.** Control: the deliverable list is fixed (research doc,
  design, stubs); any code beyond throwaway measurement probes is out of scope.
- **Research substitutes memory for current sources.** Control: every model/version/feature claim
  in the research doc carries a current source link or an UNVERIFIED label per GLOBAL-RESEARCH.

## Verification

- The research document exists in the mandated prose format, names sources checked per
  GLOBAL-RESEARCH-049, patterns found, reuse opportunities, rejected options, selected approach,
  risks, mitigations, and validation plan - explicit enough for a no-context model
  (GLOBAL-RESEARCH-059).
- The design section maps every component onto cited existing code seams (this refinement's
  attachment map) with no unmapped component.
- Follow-up stubs are appended to the backlog with dependencies and are individually promotable.
- Operator review is the acceptance gate for the direction before any implementation packet is
  promoted.

</topic>
