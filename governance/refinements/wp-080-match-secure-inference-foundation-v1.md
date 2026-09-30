---
file_id: REF-WP-080-MATCH-SECURE-INFERENCE-FOUNDATION-V1
file_kind: refinement
updated_at: "2026-08-23"
---

<topic id="operator-request" status="active" version="2" wp="WP-080" summary="Build the first Match implementation layer only after the clean SurrealDB baseline and an explicit tract runtime-line decision, with secure model loading and correct multi-face extraction." updated_at="2026-08-23">

## Operator request and product relation

Match is Facial's named person-gallery feature. It must prefer unidentified faces over false matches. This packet provides the secure deterministic inference contract needed by every later Match packet; it adds no gallery or clustering UI. Before source changes, it must also settle the tract runtime line so WP-086 acceleration cannot force a breaking migration after the storage and recognition layers are built.

## Spec anchors

- Existing identity engine and configuration: `specs/app-spec.md` sections 5.1 and 14.
- Match secure-inference contract: `specs/app-spec.md` section 17.2.
- Existing runtime: `product/src/identity.rs`, `product/src/landmarks.rs`.
- Research basis: `governance/research_person_identity.md` plus the 2026-08-21 Match evaluation.

</topic>

<topic id="research-basis" status="active" version="2" wp="WP-080" summary="YuNet remains the proven detector; public InsightFace weights are not a safe distributable default; model generations and an explicitly selected patched tract runtime are mandatory." updated_at="2026-08-23">

## Sources and selected approach

- ArcFace method: https://arxiv.org/abs/1801.07698
- InsightFace pretrained-weight restriction: https://github.com/deepinsight/insightface/blob/master/model_zoo/README.md
- OpenCV YuNet and SFace candidates: https://github.com/opencv/opencv_zoo/tree/main/models
- tract ONNX advisory: https://advisories.gitlab.com/cargo/tract-onnx/CVE-2026-55832/
- tract NNEF advisory: https://rustsec.org/advisories/RUSTSEC-2026-0217.html
- tract release/API/GPU history: https://github.com/sonos/tract/blob/main/CHANGELOG.md

Keep YuNet. Decide between the patched 0.21 backport line and the 0.23 facade/runtime line before editing Match source. The decision is recorded in `governance/validation/wp-080-tract-runtime-decision-v1.yaml` from security, MSRV, Windows packaging, current-model loading, CPU output parity, API migration, WP-086 acceleration feasibility, and rollback evidence; one line is accepted and the other is explicitly rejected. Then introduce a manifest-backed detector/embedder adapter, benchmark a distributable embedder and separately licensed ArcFace baseline on the exact collection, bind hashes/dimensions/preprocessing/normalization/runtime/threshold generation, decode once, and emit every valid face.

</topic>

<topic id="red-team" status="active" version="2" wp="WP-080" summary="Malicious model paths, silent dimension drift, invalid alignment, late runtime migration, and compatibility regressions are the hard failure modes." updated_at="2026-08-23">

## Risks and controls

- External-data path read: patched runtime, constrained model root, hash manifest, malicious fixtures.
- Late 0.23 migration after Match storage lands: settle 0.21 versus 0.23 from measured packaged evidence before source changes and include WP-086 acceleration feasibility in the decision.
- Invalid face becomes a vector: structured rejection; no whole-image fallback.
- Model-space mixing: immutable generation IDs and exact dimension checks.
- Licensing mistake: record provenance/terms and prohibit unverified bundling.
- Multi-face API breaks existing gates: compatibility adapter plus focused regression suite.

</topic>

<topic id="microtask-plan" status="active" version="2" wp="WP-080" summary="Select the runtime line, patch, manifest, refactor, reject unsafe input, and benchmark exact candidate models." updated_at="2026-08-23">

## Microtasks

1. Complete and independently review the tract 0.21-versus-0.23 runtime decision artifact; no Match source changes precede its accepted verdict.
2. Upgrade/audit the selected tract line and add unsafe-model fixtures.
3. Define the model manifest and generation contract.
4. Expose detect-all and aligned-crop embedding APIs.
5. Remove whole-image fallback and enforce finite exact dimensions.
6. Preserve existing identity-gate behavior through an adapter.
7. Benchmark and record exact detector/embedder candidates and packaged provenance.

</topic>
