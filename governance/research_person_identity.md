---
file_id: research_person_identity
file_kind: research-basis
updated_at: "2026-08-16"
wp: WP-076
---

<topic id="scope-and-question" status="active" version="1" wp="WP-076" summary="What the operator asked to investigate: better face/ID recognition for models, and an iOS-Photos-style named-person browsing system that does not interfere with the tag system." updated_at="2026-08-16">

## Operator request

Verbatim (2026-08-15):

> "then i want to investigate better face and ID recognition features for facial, so better
> identity tools are available for other models. but also i envision looking for a name that is
> linked to an identity and can browse all photos with that person in it. i think this is kind of
> a back end tag system of some kind but i do not want it to interfere with the tag system we just
> created. i have seen this feature in the ios photo app i think i think it is cool and handy. it
> is also good for sorting large scraping batches for kpop content for example."

Operator direction (2026-08-16): the app is migrating from redb to SurrealDB and the old store must
go. This design therefore targets the SurrealDB layer. The redb patterns cited below are the
*semantic* contracts to carry over (stable IDs, atomic catalog+assignment writes, a separate
regenerable index), not the storage substrate.

## Two questions, deliberately separated

1. **Engine quality** — is the shipped detector/embedder still the right choice, and what is
   missing for a person system to stand on it?
2. **People system** — how does a named identity get stored, browsed, corrected, and searched
   without touching the operator tag/label system?

They are separable: the People system's data model is unchanged whichever embedder wins, because
every candidate emits an L2-normalized float vector compared by cosine. That is the single most
important architectural finding in this document.

</topic>

<topic id="baseline-evidence" status="active" version="1" wp="WP-076" summary="The shipped engine is real, deterministic, and harness-side only; four concrete gaps block a People system, all in the engine's entry points rather than its quality." updated_at="2026-08-16">

## What Facial already has (inspected, current source)

- **Real detector**: YuNet 2023mar compiled in via `include_bytes!`
  (`product/src/identity.rs:67-70`), deterministic decode + greedy IoU NMS
  (`product/src/identity.rs:302-441`).
- **Real embedder**: ArcFace via tract, 5-point similarity alignment to the 112x112 template
  (`product/src/identity.rs:464-484`), `(px - 127.5)/128.0` normalization and L2-normalized output
  (`product/src/identity.rs:227-249`).
- **Clustering**: `cluster_embeddings` — greedy, first-match-wins against each cluster's founding
  representative, order-deterministic (`product/src/identity.rs:750-775`).
- **Zero coupling to the media browser.** The engine is reached only through CLI/service commands;
  the string `person` appears nowhere in `product/src`. The namespace is free.

## The four gaps that actually block a People system

These are entry-point and persistence gaps, not model-quality gaps:

1. **No crop-level embed API.** Every public path takes a file path and decodes internally
   (`product/src/identity.rs:172-201`); `embed_aligned` is private. Multi-face-per-image indexing
   needs to embed N crops from one decode.
2. **No embedding-dimension validation.** Dim is collected unchecked
   (`product/src/identity.rs:246`) and `cosine` silently truncates to the shorter vector
   (`product/src/identity.rs:732-739`). A model swap would silently degrade instead of failing.
   Contrast `media_clip.rs:28` (`EMBED_DIM_EXPECTED`) and its self-checks (`:374-390`).
3. **Clustering is one-shot and unpersisted.** Embeddings are recomputed per run and dropped;
   `identity_dedup` carries a 20k cap (`product/src/service.rs:1950-1955`). Nothing survives to
   name.
4. **No face-instance store.** There is no table anywhere holding "this face box, in this file,
   belongs to this person."

## Prior research basis and what it did not cover

`governance/research_face_identity.md` (2026-06) selected tract + ArcFace w600k (512-d) + fixed
alignment for reproducibility, and set contracts this design inherits verbatim: never fake a
verdict (`:90-91`), stamp model id + sha256 + thresholds into every artifact (`:86-87`), calibrate
thresholds rather than guess (`:53-55`). It contains **no** research on named-identity storage,
enrollment, incremental clustering, or multi-person assignment. That is the new ground here.

</topic>

<topic id="current-external-research" status="active" version="1" wp="WP-076" summary="Field systems converge on detect-embed-index-incrementally-cluster-name; InsightFace remains the accuracy leader and its recommended thresholds are published; incremental clustering is the hard part and is patented ground with published alternatives." updated_at="2026-08-16">

## Sources checked (2026-08-16)

**Recognition models and thresholds**
- InsightFace model zoo and pack guidance: buffalo_s/sc (mobile), **buffalo_l with the w600k_r50
  head as the recommended server default**, antelopev2 with the glintr100 head for large-server
  use. <https://github.com/deepinsight/insightface/blob/master/model_zoo/README.md>,
  <https://www.insightface.ai/guides/choose-face-recognition-model-and-evaluate>
- **Published threshold guidance** (directly relevant, because Facial currently ships an
  operator-tuned single threshold): typical 1:1 thresholds land in the **0.30-0.45 cosine range at
  FMR 1e-4 to 1e-5**, with an explicit instruction to recompute per population, pick the threshold
  on a validation split, freeze it, and never reuse a threshold across model versions.
  <https://www.insightface.ai/guides/choose-face-recognition-model-and-evaluate>
- glintr100 (ResNet-100, Glint360K) I/O contract: float32 `[1,3,112,112]` CHW in, **`[1,512]`
  L2-comparable embedding out** — the same shape Facial's pipeline already produces.
  <https://huggingface.co/HammadNaseer/arcface-glintr100-onnx>,
  <https://docs.openvino.ai/2023.3/omz_models_model_face_recognition_resnet100_arcface_onnx.html>
- Accuracy context: InsightFace reported at 99.86% LFW; CompreFace and DeepFace are wrappers over
  these same backbones rather than independent models.
  <https://www.edenai.co/post/top-free-face-compare-tools-apis-and-open-source-models>

**Runtime constraint**
- tract supports a broad operator set with speed comparable to onnxruntime; ResNet-100 uses
  standard conv/pool/FC layers that are well covered, so a glintr100 swap is *plausible* but must
  be proven by loading the exact export. <https://github.com/sonos/tract>,
  <https://lib.rs/crates/tract-onnx>

**Field People systems**
- **Immich** (closest analogue, open source, inspectable): three entities —
  `asset_face` (bounding box + optional `personId`), `face_search` (embedding vectors, pgvector
  indexed), `person` (name, hidden, favorite). A `sourceType` enum distinguishes ML-detected faces
  from faces imported from Exif/XMP. `reassignFaces()` moves faces between persons and regenerates
  thumbnails; merging is implemented as reassignment plus cleanup.
  <https://deepwiki.com/immich-app/immich/4.2-people-and-face-recognition>
- Immich clustering: **DBSCAN-derived but explicitly incremental** — it does not re-cluster the
  world; each new face looks for neighbours within a distance and inherits the person of the most
  similar already-assigned face, so **previously named people are preserved** across rounds.
  <https://docs.immich.app/features/facial-recognition/>,
  <https://github.com/immich-app/immich/discussions/8347>
- **digiKam**: two-phase detect-then-recognize; named face tags are the training signal; 8.6
  rebuilt matching around KNN+SVM. <https://docs.digikam.org/en/left_sidebar/people_view.html>
- **PhotoPrism**: People view with naming/merging/hiding, and XMP face-region import as the
  interop surface. <https://docs.photoprism.app/user-guide/organize/people/>
- Incremental clustering literature: HDBSCAN is not incremental and requires recomputation from
  scratch; **FISHDBC** provides incremental hierarchical density-based clustering; there is also
  substantial patent ground on incremental face clustering and cluster merging, which is a reason
  to prefer the simple published Immich-style approach over an invented one.
  <https://hdbscan.readthedocs.io/>, <https://arxiv.org/pdf/1910.07283>

**Not transferable**: Hugging Face/Civitai/Reddit/X sweeps produced no Rust-native People-system
implementation to copy. The design below is assembled from the above plus Facial's own seams.

</topic>

<topic id="selected-design" status="active" version="1" wp="WP-076" summary="Keep the shipped engine, add a crop-level embed entry point and dim validation, persist faces in a regenerable index, cluster incrementally with named assignments immutable, and expose People as a collection view plus a person: chip." updated_at="2026-08-16">

## Engine: keep ArcFace, fix the entry points

**Recommendation: do not change the model as part of the People work.** The shipped w600k_r50
head is the same pack InsightFace itself recommends as the server default, and it already emits
the 512-d L2-normalized vector every candidate would emit. Changing the model and building the
People system at once would make an accuracy regression indistinguishable from a clustering bug.

Engine work that *is* required, in priority order:

1. **`embed_crop(&RgbImage, &[Landmark; 5]) -> Vec<f32>`** — a public crop-level entry point so one
   decode yields N face embeddings. Today's file-path API forces one decode per face.
2. **Dimension validation** — an `EMBED_DIM_EXPECTED` constant, a load-time self-check, and a
   `cosine` that refuses mismatched lengths instead of truncating. This is what makes a later model
   swap safe.
3. **Model provisioning by the CLIP pattern** (`media_clip.rs:57-105`): env override, conventional
   `product/models/` path, and a structured "what is missing" status string rather than an
   `Option`. Face models are large and operator-provisioned; absence must degrade, not crash.

Deferred, and explicitly a separate packet: evaluating glintr100/antelopev2. It is a drop-in shape
(`[1,3,112,112]` -> `[1,512]`) but must be proven to load under tract, and per the InsightFace
guidance **its threshold must be recalibrated, never inherited**.

## Data model: three concerns, three stores

| Concern | Where | Why |
|---|---|---|
| Person catalog (id, display name, hidden, favorite, cover face) | `media.redb` -> SurrealDB, alongside labels | Operator-authored, must be durable and backed up |
| Person assignments (media key -> person ids) | same store, own table, own write path | Enables `person:` search without touching the face index |
| Face instances (file, bbox, landmarks, embedding, dim, model sha, assigned person, source) | **separate regenerable index** (`face_index`), mirroring `clip_index.redb` | Large, derived, deletable; must never contend with the metadata store's single writer |

The split follows the two precedents already proven in this codebase: the WP-061 label catalog
(stable IDs so renaming never disconnects assignments; atomic catalog+assignment transactions;
usage-aware delete) and the CLIP index (separate file, per-row invalidation by mtime/size, freely
deletable). Immich's three-table split is the same shape, which is corroboration rather than
coincidence.

**Non-interference is structural, not conventional:**
- New tables, never an extension of `set_meta_labels` — a person write can never rewrite a label
  vector.
- A distinct settings key prefix.
- A new `person:` chip in the prefix-dispatched parser (`media_search.rs:189-244`), where an
  unrecognized chip currently falls through to free text — so adding the arm changes no existing
  arm's behavior.
- Embeddings live outside the metadata store entirely.

## Clustering: incremental, with named assignments immutable

Adopt the Immich-derived rule, which is the published, non-patented, and operationally proven one:

- Each new face searches for neighbours within a cosine threshold.
- If a neighbour already belongs to a person, the new face inherits that person.
- Otherwise the face joins/forms an unnamed cluster.
- **An operator-assigned person is never overwritten by a later clustering pass.** This is a hard
  contract, not a heuristic: it is what makes the feature trustworthy at scale.
- Merge = reassign every face of person B to person A, then delete B (one transaction).
- Split/correct = reassign the selected faces to a new or existing person.

Rejected: re-clustering the world each run (destroys operator corrections and is O(n^2) at kpop
scale), and HDBSCAN (not incremental; would require full recomputation per import).

Threshold: start from the published 0.30-0.45 cosine band, then **calibrate on the operator's own
material** using the existing `calibrate_threshold` machinery (`service.rs:2298-2391`), and stamp
the chosen value plus the model sha into the index. Per the InsightFace guidance, the threshold is
frozen per model version and recomputed on any model change.

## Surfaces

- **People view** as a WP-067-style collection tab sub-view: rows built from the metadata cache,
  no filesystem scan, so it works while a NAS share is offline.
- **`person:NAME` search chip**, additive and subtractive like every other chip, with autocomplete
  fed by the external-vocabulary pattern labels already use (`ui.rs:8200-8210`) rather than the
  per-snapshot indexed catalog — a person catalog does not churn per scan.
- **Naming and correction in the Viewer metadata band**: the face(s) in the current image, each
  with a name control. WP-072 made that band resizable and scrollable, which is what makes room
  for this.
- **Model routes** for every operator action, per FACIAL-MODEL-001.

## Throughput at kpop scale (the honest number)

Measured basis from this repo's own field record: a 5,267-image gate took roughly an hour on tract
CPU (`governance/backlog_field_feedback.md`, STUB-K/L) — about **0.7 s per image**, dominated by
the 166 MB ArcFace forward pass, and that is one face per image.

At the operator's real scale (the canonical mapped folder holds **146,634 files**), a full face
index at that rate is **~28 hours of CPU**, more with multiple faces per image. That is not
interactive, and no amount of UI work hides it. Honest consequences:

- Indexing must be an explicit, resumable, cancellable background job with visible progress —
  never implicit on folder open.
- It must run under the existing `WorkClass::Background` I/O budget so it cannot starve thumbnails
  or playback (the WP-069 layer order already provides this).
- **STUB-K (warm model daemon) and STUB-L (GPU/multi-core inference) are hard prerequisites for
  kpop-batch use**, not optional polish. A GPU path is the difference between ~28 hours and
  minutes-to-hours.
- Per-row invalidation by mtime/size (the CLIP pattern) means re-indexing is incremental after the
  first pass.

Storage is negligible by comparison: 512 floats = 2 KB per face; ~150k faces is ~300 MB with
bounding boxes and metadata, in a file the operator can delete at will.

## Rejected options

- **Building People before the engine entry points exist** — multi-face indexing would need one
  decode per face.
- **Storing embeddings in the metadata store** — they are regenerable cache data and would contend
  with the single writer, exactly the mistake the CLIP index was designed to avoid.
- **Reusing the tag/label tables for person assignment** — the operator explicitly forbade
  interference; structural separation is the constraint.
- **Basing People on the proxy plugins** (deepface/facet/ofiq/ediffiqa/imagededup) — all are
  self-declared proxies; only `identity.rs` + `landmarks.rs` are real.
- **Auto-naming from filenames** at scrape time — tempting for kpop batches, but it would create
  authoritative-looking identities from unverified text, violating the never-fake-a-verdict
  contract. Filename text may *suggest* a name in the UI for one-click confirmation; it must never
  assign one silently.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-076" summary="The failure modes are operator-trust failures, not model failures: destroyed corrections, silent model drift, a starved UI, and a person store that quietly couples to tags." updated_at="2026-08-16">

## Risks and controls

- **A re-cluster destroys hours of naming.** Control: named assignments are immutable under
  clustering, by contract and by test; clustering may only fill unassigned faces.
- **A model swap silently changes similarity.** Control: dim validation, model sha stamped per
  index row, and a stored threshold bound to that sha; a mismatch forces an explicit reindex
  rather than mixing vector spaces.
- **Indexing starves browsing.** Control: Background work class, explicit start/stop, resumable,
  progress visible; the WP-069 layer order and its fairness floor already exist.
- **The person store couples to tags over time.** Control: separate tables, separate write path,
  a source-level guard in the spirit of the WP-074/075 guards asserting person code never calls
  the label write path.
- **Wrong-person merges are hard to undo.** Control: merge is a reassignment (not a delete of
  face rows), so the inverse operation exists; deletion of a person never deletes faces or files.
- **Faces from scraped batches carry no consent context.** Out of assistant scope by standing
  operator policy; recorded here only so the design does not invent a compliance surface.
- **Privacy**: everything is local; no network call is part of this design.

## Validation plan for the implementation packets

- Focused tests: crop-embed determinism (same crop -> byte-identical vector); dim mismatch refused;
  named assignment survives a re-cluster; merge/split round trip; `person:` chip parse/match
  including negation; collection view builds without a scan.
- Threshold calibration run on operator material with the recommendation recorded, not guessed.
- Deterministic inspector presets for the People view and the Viewer naming control.
- Live receipt probes for every person intent under the GUI database lock.
- Scale probe on a bounded subset of the 146k folder measuring per-image cost, with the projected
  full-run figure stated explicitly.

</topic>

<topic id="promotable-stubs" status="active" version="1" wp="WP-076" summary="Four sequenced follow-up packets, each independently promotable." updated_at="2026-08-16">

## Follow-up work-packet stubs

- **STUB-P1 — Identity engine entry points and validation.** `embed_crop`, `EMBED_DIM_EXPECTED`
  plus load-time self-check, non-truncating `cosine`, CLIP-style model provisioning with a
  structured status. No UI. Independently valuable: it hardens the existing gate commands.
  Depends on: nothing.
- **STUB-P2 — Face index and incremental clustering.** `face_index` store (separate, regenerable,
  mtime/size invalidation, model sha + threshold stamped), background indexing job under
  `WorkClass::Background` with progress/cancel/resume, incremental assignment with named
  assignments immutable. Depends on: P1.
- **STUB-P3 — People surfaces.** Person catalog + assignments in the (SurrealDB) metadata store
  with WP-061 semantics, People collection sub-view, Viewer naming/correction control, merge and
  split, model intents for all of it. Depends on: P2.
- **STUB-P4 — `person:` search chip.** Parser arm, matcher arms (additive and subtractive),
  `is_empty`/`has_chips`, indexed-row support, autocomplete via the external-vocabulary pattern.
  Depends on: P3. Small and mechanical once P3 lands.

Optional, unsequenced: XMP face-region import/export for digiKam/PhotoPrism interop; a
filename-suggests-a-name one-click confirmation flow for scraped batches (never automatic).

**Operator decision needed before P1 is promoted**: whether kpop-batch indexing is in scope soon.
If yes, STUB-K (warm daemon) and STUB-L (GPU inference) must be promoted *ahead of* P2, because
CPU-only indexing of the 146k folder is a ~28-hour job.

</topic>
