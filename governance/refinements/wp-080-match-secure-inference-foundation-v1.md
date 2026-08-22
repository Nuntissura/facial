---
file_id: REF-WP-080-MATCH-SECURE-INFERENCE-FOUNDATION-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="active" version="1" wp="WP-080" summary="Build the first Match implementation layer only after the clean SurrealDB baseline, with secure model loading and correct multi-face extraction." updated_at="2026-08-22">

## Operator request and product relation

Match is Facial's named person-gallery feature. It must prefer unidentified faces over false matches. This packet provides the secure deterministic inference contract needed by every later Match packet; it adds no gallery or clustering UI.

## Spec anchors

- Existing identity engine and configuration: `specs/app-spec.md` sections 5.1 and 14.
- Existing runtime: `product/src/identity.rs`, `product/src/landmarks.rs`.
- Research basis: `governance/research_person_identity.md` plus the 2026-08-21 Match evaluation.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-080" summary="YuNet remains the proven detector; public InsightFace weights are not a safe distributable default; model generations and a patched tract runtime are mandatory." updated_at="2026-08-22">

## Sources and selected approach

- ArcFace method: https://arxiv.org/abs/1801.07698
- InsightFace pretrained-weight restriction: https://github.com/deepinsight/insightface/blob/master/model_zoo/README.md
- OpenCV YuNet and SFace candidates: https://github.com/opencv/opencv_zoo/tree/main/models
- tract ONNX advisory: https://advisories.gitlab.com/cargo/tract-onnx/CVE-2026-55832/
- tract NNEF advisory: https://rustsec.org/advisories/RUSTSEC-2026-0217.html

Keep YuNet. Patch tract. Introduce a manifest-backed detector/embedder adapter. Benchmark a distributable embedder and separately licensed ArcFace baseline on the exact collection. Bind hashes, dimensions, preprocessing, normalization, runtime, and threshold generation. Decode once and emit every valid face.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-080" summary="Malicious model paths, silent dimension drift, invalid alignment, and compatibility regressions are the hard failure modes." updated_at="2026-08-22">

## Risks and controls

- External-data path read: patched runtime, constrained model root, hash manifest, malicious fixtures.
- Invalid face becomes a vector: structured rejection; no whole-image fallback.
- Model-space mixing: immutable generation IDs and exact dimension checks.
- Licensing mistake: record provenance/terms and prohibit unverified bundling.
- Multi-face API breaks existing gates: compatibility adapter plus focused regression suite.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-080" summary="Patch, manifest, refactor, reject unsafe input, and benchmark exact candidate models." updated_at="2026-08-22">

## Microtasks

1. Upgrade/audit tract and add unsafe-model fixtures.
2. Define the model manifest and generation contract.
3. Expose detect-all and aligned-crop embedding APIs.
4. Remove whole-image fallback and enforce finite exact dimensions.
5. Preserve existing identity-gate behavior through an adapter.
6. Benchmark and record exact detector/embedder candidates and packaged provenance.

</topic>
